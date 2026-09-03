//! The Soulseek backend, driven by a running [slskd](https://github.com/slskd/slskd).
//!
//! Talking to a daemon rather than implementing the Soulseek protocol keeps the
//! login, the peer connections and the transfer queue in one long-lived process
//! that outlives any single command — which is what the network wants, since a
//! queue position can take hours to come good.
//!
//! Two things make this backend unlike the others:
//!
//! * **slskd downloads the file, not us.** Its API lists and deletes files in
//!   its download directory but will not hand over their bytes, so collecting
//!   one is a separate step: a local move when that directory is reachable as a
//!   path, or an HTTP GET against `files_url` when slskd is on another machine.
//! * **A search is a blocking call with a server-side window.** slskd waits for
//!   peers itself and returns when they go quiet, so `search_limit` — not the
//!   timeout — is what keeps a popular query from running long.
//!
//! We never start or stop slskd. An unconfigured or unreachable one is reported
//! through `credentials()` so `backends` can say what is missing.

pub mod api;

use std::path::Path;
use std::time::{Duration, Instant};

use super::error::{BackendError, Result};
use super::types::*;
use crate::config::{self, CredentialSource, Credentials, Secret};

const ID: BackendId = BackendId::Soulseek;

/// How often the download poll asks slskd again.
const POLL_EVERY: Duration = Duration::from_secs(2);

/// Everything we fetch lands under here, inside slskd's download directory, so
/// a staging area we can name in advance never collides with the user's own
/// downloads. Mirrors the `.rr-<uuid>` staging directory the SoundCloud backend
/// uses locally.
const STAGING_ROOT: &str = "rekord-ripper";

/// How many spent batch ids to step past before giving up. Small on purpose:
/// needing more than a couple means something else is wrong.
const ATTACH_ATTEMPTS: u32 = 4;

/// Namespace for the deterministic batch id. Fixed forever: changing it would
/// orphan every in-flight transfer a previous build queued.
const BATCH_NAMESPACE: uuid::Uuid = uuid::Uuid::from_bytes([
    0x8f, 0x2b, 0x4c, 0x1d, 0x6e, 0x37, 0x4a, 0x9c, 0xb8, 0x51, 0x0d, 0x7a, 0x3f, 0xc6, 0x92, 0x14,
]);

pub struct Soulseek {
    url: String,
    files_url: String,
    api_key: Option<(Secret, CredentialSource)>,
    files_user: String,
    files_password: Option<(Secret, CredentialSource)>,
    search_window: Duration,
    search_limit: usize,
    fetch_timeout: Duration,
    clean_up_remote: bool,
    budget: Duration,
}

impl Soulseek {
    pub fn new(cfg: &config::Soulseek, creds: &Credentials, budget: Duration) -> Self {
        Self {
            url: cfg.url.trim().trim_end_matches('/').to_string(),
            files_url: cfg.files_url.trim().trim_end_matches('/').to_string(),
            // A failure to read a key file is reported when the key is needed,
            // not by silently disabling the backend.
            api_key: creds.soulseek_api_key().ok().flatten(),
            files_user: creds.soulseek.files_user.trim().to_string(),
            files_password: creds.soulseek_files_password().ok().flatten(),
            search_window: Duration::from_secs(
                cfg.search_window_secs.max(api::MIN_SEARCH_WINDOW_SECS),
            ),
            search_limit: cfg.search_limit.max(1),
            fetch_timeout: Duration::from_secs(cfg.fetch_timeout_secs.max(1)),
            clean_up_remote: cfg.clean_up_remote,
            budget,
        }
    }

    fn client(&self) -> Result<api::Client> {
        if self.url.is_empty() {
            return Err(BackendError::NoCredentials {
                backend: ID,
                how_to_fix: "set [soulseek] url in config.toml to your slskd address, \
                             e.g. https://slskd.example.com:5030"
                    .into(),
            });
        }
        let Some((key, _)) = self.api_key.as_ref() else {
            return Err(BackendError::NoCredentials {
                backend: ID,
                how_to_fix: "put an slskd API key in credentials.toml as [soulseek] api_key \
                             — generate one with `slskd --generate-secret 32`"
                    .into(),
            });
        };
        Ok(api::Client::new(&self.url, key.expose(), self.budget))
    }

    /// Where a fetch stages its file, relative to slskd's download directory.
    fn staging(batch_id: &str) -> String {
        format!("{STAGING_ROOT}/{batch_id}")
    }
}

// ---------------------------------------------------------------------------
// Item refs
// ---------------------------------------------------------------------------

/// `size:bitrate:username:filename`, with 0 for either number when unknown.
///
/// The numbers lead because they are digits and the filename is last because it
/// is the only part that can contain a colon — `ItemRef` splits off the backend
/// on the first one and leaves the rest to us.
///
/// The bitrate is in here because a search knows it and a fetch cannot find it
/// out again: without it a 320kbps MP3 would reach `fetch` indistinguishable
/// from one of unknown quality, which is both a preference check the user did
/// not ask for and a format understated in the result.
fn item_key(size: u64, bitrate: u32, username: &str, filename: &str) -> String {
    format!("{size}:{bitrate}:{username}:{filename}")
}

fn parse_item_key(key: &str) -> Option<(u64, Option<u32>, &str, &str)> {
    let mut parts = key.splitn(4, ':');
    let size = parts.next()?.trim().parse::<u64>().ok()?;
    let bitrate = parts.next()?.trim().parse::<u32>().ok()?;
    let username = parts.next()?;
    let filename = parts.next()?;
    (!username.is_empty() && !filename.is_empty()).then_some((
        size,
        (bitrate > 0).then_some(bitrate),
        username,
        filename,
    ))
}

/// A batch id derived from the offer, so re-fetching the same file attaches to
/// the transfer already queued instead of starting a second one.
fn batch_id(username: &str, filename: &str, attempt: u32) -> String {
    // Attempt 0 hashes the bare pair, so a batch queued by an earlier build is
    // still recognised and attached to rather than orphaned.
    let name = if attempt == 0 {
        format!("{username}\u{1f}{filename}")
    } else {
        format!("{username}\u{1f}{filename}\u{1f}{attempt}")
    };
    uuid::Uuid::new_v5(&BATCH_NAMESPACE, name.as_bytes()).to_string()
}

// ---------------------------------------------------------------------------
// Paths: all a search gives us is the peer's own filename
// ---------------------------------------------------------------------------

/// The last component, splitting on either separator — a shared folder on
/// Windows produces backslashes, one on Linux produces forward slashes, and both
/// turn up in the same search.
fn basename(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or(path)
}

/// The containing directory's name, which usually carries the release.
fn parent_name(path: &str) -> Option<&str> {
    let mut parts = path.rsplit(['\\', '/']);
    parts.next()?;
    parts.next().filter(|p| !p.is_empty())
}

fn stem_and_ext(name: &str) -> (&str, Option<&str>) {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() => (stem, Some(ext)),
        _ => (name, None),
    }
}

/// Drop a leading track number: `03 - Title`, `03. Title`, `03 Title`.
///
/// Capped at three digits so a leading year (`1979 - Title`) is left alone.
fn strip_track_number(stem: &str) -> &str {
    let digits = stem.len() - stem.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if digits == 0 || digits > 3 {
        return stem;
    }
    let rest = &stem[digits..];
    let trimmed = rest.trim_start_matches([' ', '-', '.', '_', ')']);
    // Only if a separator actually followed; "01" alone, or "2007remaster",
    // is not a track number we can strip safely.
    if trimmed.len() == rest.len() || trimmed.is_empty() {
        stem
    } else {
        trimmed
    }
}

/// Artist, title and album from a path, best effort.
///
/// There is no metadata in a search result — only the peer's path — so this is
/// heuristic by necessity. It feeds the offer table and the similarity score, so
/// being roughly right matters more than being provably right.
fn describe(path: &str) -> (String, String, Option<String>) {
    let (stem, _) = stem_and_ext(basename(path));
    let stem = strip_track_number(stem.trim());
    let dir = parent_name(path).map(str::trim).filter(|d| !d.is_empty());
    // A release folder is usually "Artist - Album", sometimes with a trailing
    // "(2007) [FLAC]" that we leave alone rather than guess at.
    let (dir_artist, dir_album) = match dir.and_then(|d| d.split_once(" - ")) {
        Some((a, b)) => (Some(a.trim()), Some(b.trim())),
        None => (None, dir),
    };

    match stem.split_once(" - ") {
        Some((a, t)) if !a.trim().is_empty() && !t.trim().is_empty() => (
            a.trim().to_string(),
            t.trim().to_string(),
            dir_album.map(str::to_string),
        ),
        _ => (
            dir_artist.unwrap_or("").to_string(),
            stem.to_string(),
            dir_album.map(str::to_string),
        ),
    }
}

/// The encoding, from slskd's parsed extension plus its bitrate fields.
///
/// slskd hands us `extension` already separated out, and — unlike the raw
/// protocol — tells us whether a lossy encode is variable bitrate, which is the
/// only honest way to report LAME V0 rather than guessing a number.
fn format_of(
    extension: Option<&str>,
    path: &str,
    bitrate: Option<u32>,
    vbr: bool,
) -> Option<AudioFormat> {
    let ext = extension
        .map(|e| e.trim().trim_start_matches('.'))
        .filter(|e| !e.is_empty())
        .or_else(|| stem_and_ext(basename(path)).1)?;
    let base = ext.parse::<AudioFormat>().ok()?;
    Some(match (base, bitrate) {
        // A VBR MP3 has no honest single number, which is exactly what Mp3V0 is
        // for. Only for MP3: `Mp3V0` names a LAME preset.
        (AudioFormat::Mp3(_), _) if vbr => AudioFormat::Mp3V0,
        // Only lossy formats carry a meaningful bitrate; a FLAC's is an artefact
        // of the encode, not a quality tier.
        (AudioFormat::Mp3(_), Some(k)) if k > 0 => {
            AudioFormat::Mp3(Some(k.min(u16::MAX as u32) as u16))
        }
        (AudioFormat::Aac(_), Some(k)) if k > 0 => {
            AudioFormat::Aac(Some(k.min(u16::MAX as u32) as u16))
        }
        _ => base,
    })
}

/// Whether `got` satisfies a preference list.
///
/// A peer's bitrate is whatever they happened to encode at, so an exact match
/// against a preference of `mp3-320` would reject a 256kbps file that the user
/// would plainly have taken. The rule is "at least as good as something you
/// asked for, in the same family": with the default preference list a 256k MP3
/// is accepted through `mp3-v0`, and a 128k one is still refused.
fn satisfies(got: AudioFormat, pref: &[AudioFormat]) -> bool {
    pref.iter().any(|p| {
        same_family(*p, got)
            && match got {
                // No bitrate to compare against: the peer did not report one.
                // Refusing every such file would be worse than taking the family
                // preference at face value, since there is no way to learn more.
                AudioFormat::Mp3(None) | AudioFormat::Aac(None) => true,
                _ => got.quality_rank() >= p.quality_rank(),
            }
    })
}

fn same_family(a: AudioFormat, b: AudioFormat) -> bool {
    use AudioFormat::*;
    match (a, b) {
        // MP3 at any bitrate, V0 included, is one family.
        (Mp3(_) | Mp3V0, Mp3(_) | Mp3V0) => true,
        (Aac(_), Aac(_)) => true,
        // Everything else matches only itself — ALAC and AAC share a container
        // but not a quality story, so a preference for one is not the other.
        _ => a == b,
    }
}

// ---------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------

/// One file from one peer, with the peer facts needed to rank it.
struct Hit {
    offer: Offer,
    rank: u16,
    has_slot: bool,
    queue_length: u64,
    speed: u32,
}

fn hits_from(responses: &[api::Response]) -> Vec<Hit> {
    let mut out = Vec::new();
    for peer in responses {
        // A colon in a username would break the item ref, and there is nothing
        // useful to do about it but skip the peer.
        if peer.username.contains(':') || peer.username.is_empty() {
            continue;
        }
        // `locked_files` are deliberately ignored: they are visible but gated
        // behind the peer's sharing rules, so offering one would be offering
        // something a fetch cannot deliver.
        for file in &peer.files {
            if file.is_locked {
                continue;
            }
            let Some(format) = format_of(
                file.extension.as_deref(),
                &file.filename,
                file.bit_rate,
                file.is_variable_bit_rate.unwrap_or(false),
            ) else {
                continue; // not audio we can use
            };
            let (artist, title, album) = describe(&file.filename);
            let mut offer = Offer::new(
                ItemRef::new(
                    ID,
                    item_key(
                        file.size,
                        file.bit_rate.unwrap_or(0),
                        &peer.username,
                        &file.filename,
                    ),
                ),
                ItemKind::Track,
                artist,
                title,
                format!("slsk://{}/{}", peer.username, file.filename),
            );
            offer.album = album;
            offer.duration_secs = file.length;
            // Everything is known here, so there is nothing for `enrich` to do:
            // the format comes from the response and nothing on Soulseek is for
            // sale.
            offer.formats = Some(vec![format]);
            offer.pricing = Pricing::Free;
            offer.ownership = Ownership::NotApplicable;
            out.push(Hit {
                offer,
                rank: format.quality_rank(),
                has_slot: peer.has_free_upload_slot,
                queue_length: peer.queue_length,
                speed: peer.upload_speed,
            });
        }
    }
    out
}

/// Best first, so truncating to `limit` keeps the best rather than the first
/// peer that happened to answer.
///
/// Quality leads, then whether the transfer can start at all: a free slot and a
/// short queue are the difference between a download and an overnight wait.
fn rank_and_truncate(mut hits: Vec<Hit>, limit: usize) -> Vec<Offer> {
    hits.sort_by(|a, b| {
        b.rank
            .cmp(&a.rank)
            .then(b.has_slot.cmp(&a.has_slot))
            .then(a.queue_length.cmp(&b.queue_length))
            .then(b.speed.cmp(&a.speed))
            // Keep it deterministic when peers are indistinguishable.
            .then(a.offer.item_ref.key.cmp(&b.offer.item_ref.key))
    });
    hits.truncate(limit);
    hits.into_iter().map(|h| h.offer).collect()
}

// ---------------------------------------------------------------------------
// Fetch
// ---------------------------------------------------------------------------

/// A collected file has to be the size slskd said it finished at.
///
/// "Succeeded" plus a short file means we copied the wrong thing, and a
/// truncated FLAC that rekordbox imports and analyses produces a
/// plausible-looking but wrong beat grid.
fn check_size(got: u64, expected: u64) -> Result<()> {
    if expected > 0 && got != expected {
        return Err(BackendError::Other(anyhow::anyhow!(
            "slskd reported {expected} bytes but {got} arrived — refusing a partial file"
        )));
    }
    Ok(())
}

/// Turn a 404 from the file route into the diagnosis it actually is.
///
/// The mistake this exists for: `files_url` serving a *different* directory
/// than the one slskd downloads into. slskd reports the download as succeeded,
/// the file genuinely exists, and the bare 404 gives no hint that two paths
/// need to agree — so say which two.
fn explain_missing(
    client: &api::Client,
    e: BackendError,
    staging: &str,
    name: &str,
) -> BackendError {
    if !matches!(e, BackendError::Http { status: 404, .. }) {
        return e;
    }
    let dir = client
        .options()
        .map(|o| o.directories.downloads)
        .unwrap_or_default();
    let dir = if dir.is_empty() {
        "its download directory".to_string()
    } else {
        format!("its download directory ({dir})")
    };
    BackendError::Other(anyhow::anyhow!(
        "slskd has this file at {staging}/{name} inside {dir}, but files_url answered 404 \
         for it. files_url has to serve that same directory — check that the web route's \
         root and slskd's own `directories.downloads` are the same place."
    ))
}

/// One progress line per change, so an hour in a queue is legible without being
/// thousands of identical lines.
fn progress_line(t: &api::Transfer) -> String {
    if t.state.is_queued() {
        return match t.place_in_queue {
            Some(p) => format!("queued at position {p}"),
            None => "queued".to_string(),
        };
    }
    if t.state.in_progress() {
        let pct = t.bytes_transferred.checked_mul(100).unwrap_or(0) / t.size.max(1);
        return format!(
            "{pct}% ({:.1} of {:.1} MiB) at {:.0} KiB/s",
            mib(t.bytes_transferred),
            mib(t.size),
            t.average_speed / 1024.0
        );
    }
    if t.state.succeeded() {
        return "complete".to_string();
    }
    match t.exception.as_deref() {
        Some(e) => format!("{} — {e}", t.state.raw()),
        None => t.state.raw().to_string(),
    }
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

impl Soulseek {
    /// Start the transfer, or attach to one slskd already has.
    ///
    /// The batch id is derived from the offer, so a second attempt collides on
    /// purpose: slskd answers 409 and we attach rather than queueing the same
    /// file twice and losing the place already waited for.
    /// Start the transfer, or attach to one slskd already has, and return the
    /// batch id that ended up in play.
    ///
    /// The batch id is derived from the offer, so a second fetch of the same
    /// file collides on purpose and slskd answers 409 — that is how we inherit a
    /// queue position instead of joining the back of the queue again.
    ///
    /// The wrinkle is a *spent* batch: slskd remembers a transfer as succeeded
    /// long after the file has gone, because a successful fetch takes it away.
    /// Attaching to one of those would find nothing to collect and dead-end, so
    /// a spent id is skipped and the next attempt gets a fresh one.
    fn start_or_attach(
        &self,
        client: &api::Client,
        username: &str,
        filename: &str,
        size: u64,
    ) -> Result<String> {
        for attempt in 0..ATTACH_ATTEMPTS {
            let id = batch_id(username, filename, attempt);
            let staging = Self::staging(&id);
            let req = api::BatchRequest {
                id: id.clone(),
                username,
                files: vec![api::BatchItem { filename, size }],
                options: api::BatchOptions {
                    destination: &staging,
                },
            };

            match client.enqueue(&req) {
                Ok(resp) => {
                    // We only ever enqueue one file, so any failure is total.
                    if let Some(f) = resp.failures.first() {
                        let msg = f.message.clone().unwrap_or_else(|| "rejected".into());
                        return Err(BackendError::Other(anyhow::anyhow!(
                            "slskd refused this download: {msg}"
                        )));
                    }
                    return Ok(id);
                }
                Err(e) if api::is_conflict(&e) => {
                    if self.is_reusable(client, &id, &staging)? {
                        eprintln!("soulseek: attaching to a transfer slskd already has");
                        return Ok(id);
                    }
                    // Spent: finished, with nothing left behind. Queue it again.
                    eprintln!("soulseek: the earlier copy is gone; downloading it again");
                }
                Err(e) => return Err(e),
            }
        }
        Err(BackendError::Other(anyhow::anyhow!(
            "slskd already holds {ATTACH_ATTEMPTS} finished-but-empty downloads of this file; \
             clear them from its transfer list and try again"
        )))
    }

    /// Whether an existing batch can still produce the file.
    ///
    /// Either it is still working — in which case waiting is exactly right — or
    /// it succeeded and the file is still in its staging directory.
    fn is_reusable(&self, client: &api::Client, id: &str, staging: &str) -> Result<bool> {
        let batch = client.batch(id)?;
        if batch.transfers.is_empty() {
            return Ok(false);
        }
        if batch.transfers.iter().any(|t| !t.state.is_terminal()) {
            return Ok(true);
        }
        if !batch.transfers.iter().all(|t| t.state.succeeded()) {
            return Ok(false);
        }
        // A missing staging directory answers 404, which is the common case
        // here rather than an error worth propagating.
        Ok(client
            .list_downloads(staging)
            .map(|l| !l.walk().is_empty())
            .unwrap_or(false))
    }

    /// Poll until the transfer finishes, fails, or runs out of time.
    fn await_transfer(
        client: &api::Client,
        id: &str,
        filename: &str,
        deadline: Instant,
    ) -> Result<api::Transfer> {
        let started = Instant::now();
        let mut last = String::new();
        let want = basename(filename);
        loop {
            let batch = client.batch(id)?;
            let Some(t) = batch
                .transfers
                .iter()
                .find(|t| basename(&t.filename) == want)
                .or_else(|| batch.transfers.first())
            else {
                return Err(BackendError::Other(anyhow::anyhow!(
                    "slskd has no transfer in batch {id} — another client may have cleared it"
                )));
            };

            let line = progress_line(t);
            if line != last {
                eprintln!("soulseek: {line}");
                last = line;
            }

            if t.state.succeeded() {
                return Ok(t.clone());
            }
            if t.state.failed() {
                let detail = t
                    .exception
                    .clone()
                    .unwrap_or_else(|| t.state.raw().to_string());
                // Retryable: slskd keeps the record, so another attempt can
                // pick the same file up from a different peer or later queue.
                return Err(BackendError::Network {
                    backend: ID,
                    detail,
                });
            }

            if Instant::now() >= deadline {
                // Deliberately left running. It is queued on a peer that may be
                // hours away, and cancelling would throw away a wait already
                // paid for; the next fetch attaches to it instead.
                eprintln!(
                    "soulseek: giving up waiting — the transfer is still queued in slskd, \
                     and fetching this offer again will attach to it"
                );
                return Err(BackendError::Timeout {
                    backend: ID,
                    op: "download",
                    elapsed: started.elapsed(),
                });
            }
            std::thread::sleep(POLL_EVERY.min(deadline.saturating_duration_since(Instant::now())));
        }
    }

    /// Find the finished file in slskd's staging subdirectory.
    fn locate(
        client: &api::Client,
        staging: &str,
        transfer: &api::Transfer,
    ) -> Result<api::FilesystemFile> {
        let listing = client.list_downloads(staging)?;
        let files = listing.walk();
        let want = basename(&transfer.filename);
        // Name first, then size, then whatever single file is there — slskd
        // sanitises names on the way to disk, so an exact match is likely but
        // not guaranteed, and the staging directory holds exactly one download.
        let found = files
            .iter()
            .find(|f| f.name == want)
            .or_else(|| files.iter().find(|f| f.length == transfer.size))
            .or_else(|| files.first())
            .ok_or_else(|| {
                BackendError::Other(anyhow::anyhow!(
                    "slskd reported {want} complete but its download directory {staging} is empty"
                ))
            })?;
        Ok((*found).clone())
    }

    /// Pull the bytes over HTTP from `files_url`.
    fn http_collect(
        &self,
        staging: &str,
        name: &str,
        target: &Path,
        deadline: Instant,
    ) -> Result<u64> {
        let url = format!(
            "{}/{}/{}",
            self.files_url,
            staging
                .split('/')
                .map(api::encode_path_segment)
                .collect::<Vec<_>>()
                .join("/"),
            api::encode_path_segment(name)
        );
        let budget = deadline
            .saturating_duration_since(Instant::now())
            .max(Duration::from_secs(1));
        let agent = super::http::download_agent(budget);

        let mut req = agent.get(&url);
        if !self.files_user.is_empty() {
            let pw = self
                .files_password
                .as_ref()
                .map(|(s, _)| s.expose().to_string())
                .unwrap_or_default();
            req = req.header("Authorization", &api::basic_auth(&self.files_user, &pw));
        }
        let mut resp = req.call().map_err(|e| super::http::map_err(ID, &url, e))?;
        let mut reader = resp.body_mut().as_reader();
        super::fs::write_audio_atomically(target, &mut reader)
    }
}

impl super::AcquisitionBackend for Soulseek {
    fn id(&self) -> BackendId {
        ID
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            search: true,
            // A search response already carries the format and nothing is for
            // sale, so there is nothing left for `enrich` to ask about.
            price_quotes: false,
            ownership_check: false,
            requires_purchase: false,
            fetch: true,
            lossless_capable: true,
        }
    }

    fn credentials(&self) -> CredentialState {
        // Configuration only — no request. Liveness surfaces later as a network
        // error from a real call.
        if self.url.is_empty() {
            return CredentialState::Missing {
                how_to_fix: "set [soulseek] url in config.toml to your slskd address \
                             (and api_key in credentials.toml)"
                    .into(),
            };
        }
        let Some((_, src)) = self.api_key.as_ref() else {
            return CredentialState::Missing {
                how_to_fix: format!(
                    "slskd at {} needs an API key in credentials.toml as [soulseek] api_key",
                    self.url
                ),
            };
        };
        if self.files_url.is_empty() {
            CredentialState::Present {
                hint: format!("{} (key from {src}); downloads read from disk", self.url),
            }
        } else {
            CredentialState::Present {
                hint: format!(
                    "{} (key from {src}); files via {}",
                    self.url, self.files_url
                ),
            }
        }
    }

    fn claim_url(&self, url: &str) -> Option<ItemRef> {
        // Our own scheme only. `Registry::claim_url` is first-match-wins, so the
        // predicate stays narrow.
        let rest = url.strip_prefix("slsk://")?;
        let (username, filename) = rest.split_once('/')?;
        if username.is_empty() || filename.is_empty() || username.contains(':') {
            return None;
        }
        // A URL carries neither size nor bitrate; slskd accepts an enqueue
        // without a size, and an unknown bitrate is handled by `satisfies`.
        Some(ItemRef::new(ID, item_key(0, 0, username, filename)))
    }

    fn search(&self, query: &SearchQuery) -> Result<Vec<Offer>> {
        let text = query.search_text();
        if text.is_empty() || query.limit == 0 {
            return Ok(Vec::new());
        }
        let client = self.client()?;
        let id = uuid::Uuid::new_v4().to_string();

        let result = client.search(
            &api::SearchRequest {
                id: id.clone(),
                search_text: &text,
                // Milliseconds on the wire, whatever slskd's docs claim.
                search_timeout: self.search_window.as_millis() as u64,
                // Bounding the peers is what bounds the wall time: slskd's
                // timeout restarts on every response, so a popular query would
                // otherwise run far past it. Deliberately no file limit — that
                // ends the search early and takes whoever was quickest rather
                // than whoever has the best copy.
                response_limit: self.search_limit,
                filter_responses: true,
            },
            // The whole budget, because the idle window is not the wall time:
            // slskd restarts it on every response, so a search that finds
            // anything finishes at *last answer plus the window*, and it hands
            // over nothing at all until it has finished. Waiting one window and
            // a little slack is a race that mostly loses — the symptom being a
            // query that plainly matches returning no offers. A scoped thread
            // cannot be abandoned, so the budget is still the hard ceiling.
            Instant::now() + self.budget,
        );

        // slskd keeps searches in its history for the web UI; ours are noise.
        // Before the `?`, so an abandoned search is tidied up too.
        client.forget_search(&id);

        Ok(rank_and_truncate(hits_from(&result?), query.limit))
    }

    fn purchase(&self, _item: &ItemRef) -> Result<PurchaseFlow> {
        Ok(PurchaseFlow::NotRequired)
    }

    fn fetch(&self, item: &ItemRef, opts: &FetchOpts) -> Result<Vec<AcquiredFile>> {
        let (size, bitrate, username, filename) =
            parse_item_key(&item.key).ok_or_else(|| BackendError::NotFound {
                backend: ID,
                item: item.key.clone(),
            })?;

        // The format is known before a single byte moves, so unlike a backend
        // that has to download first, this can honour the preference exactly.
        let format = format_of(None, filename, bitrate, false).ok_or_else(|| {
            BackendError::NoAcceptableFormat {
                available: Vec::new(),
                wanted: opts.format_pref.clone(),
            }
        })?;
        if !satisfies(format, &opts.format_pref) {
            return Err(BackendError::NoAcceptableFormat {
                available: vec![format],
                wanted: opts.format_pref.clone(),
            });
        }

        let client = self.client()?;
        let deadline = opts.deadline.min(Instant::now() + self.fetch_timeout);
        let id = self.start_or_attach(&client, username, filename, size)?;
        let staging = Self::staging(&id);
        let done = Self::await_transfer(&client, &id, filename, deadline)?;
        let found = Self::locate(&client, &staging, &done)?;

        std::fs::create_dir_all(&opts.dest_dir)?;
        let (artist, title, album) = describe(filename);
        let (path, bytes) = if self.files_url.is_empty() {
            // slskd's download directory is reachable as a path: a local slskd,
            // or a mounted share. `place` already falls back to copy-then-delete
            // across filesystems, which is exactly the mounted case.
            //
            // Composed from the configured directory rather than the listing's
            // `fullName`, which is only a leaf name when a subdirectory was
            // listed and so is not a usable path.
            let downloads = client.options()?.directories.downloads;
            let produced = Path::new(&downloads).join(&staging).join(&found.name);
            if !produced.is_file() {
                return Err(BackendError::Other(anyhow::anyhow!(
                    "slskd downloaded the file to {} on its own host, which is not readable \
                     from here — set [soulseek] files_url if slskd is on another machine",
                    produced.display()
                )));
            }
            let bytes = std::fs::metadata(&produced)?.len();
            check_size(bytes, done.size)?;
            (
                super::fs::place(&produced, &opts.dest_dir, opts.overwrite)?,
                bytes,
            )
        } else {
            let (stem, _) = stem_and_ext(&found.name);
            let desired = opts.dest_dir.join(super::fs::track_filename(
                None,
                Some(stem),
                format.extension(),
            ));
            let target = if opts.overwrite {
                desired
            } else {
                super::fs::unique_path(&desired)
            };
            let bytes = self
                .http_collect(&staging, &found.name, &target, deadline)
                .map_err(|e| explain_missing(&client, e, &staging, &found.name))?;
            if let Err(e) = check_size(bytes, done.size) {
                // A short file under a .flac name is exactly what the
                // atomic-write rule exists to keep out of the library.
                let _ = std::fs::remove_file(&target);
                return Err(e);
            }
            (target, bytes)
        };

        // Only once the bytes are here and verified. Quiet on failure: slskd
        // answers 403 unless `remote_file_management` is on, and tidying up is
        // not worth failing a finished download over.
        if self.clean_up_remote {
            client.delete_download_subdirectory(&staging);
        }

        Ok(vec![AcquiredFile {
            path,
            format,
            bytes,
            retention: opts.retention,
            source: item.clone(),
            source_url: format!("slsk://{username}/{filename}"),
            artist: (!artist.is_empty()).then_some(artist),
            title: (!title.is_empty()).then_some(title),
            album,
            track_number: None,
        }])
    }
}

#[cfg(test)]
mod tests {
    use super::super::AcquisitionBackend;
    use super::*;

    fn backend() -> Soulseek {
        Soulseek::new(
            &config::Soulseek::default(),
            &Credentials::default(),
            Duration::from_secs(5),
        )
    }

    #[test]
    fn item_keys_round_trip_through_a_shell_argument() {
        // A Soulseek path is the worst case: backslashes, spaces, brackets, and
        // a colon that must survive because ItemRef only splits on the first one.
        let name = r"@@abc\Music\Aphex Twin - Selected Ambient Works [1992]\03 - Xtal: live.flac";
        let key = item_key(41_237_884, 1006, "user_1", name);
        let r = ItemRef::new(ID, key);
        let back: ItemRef = r.to_string().parse().unwrap();
        assert_eq!(back, r);

        let (size, bitrate, user, file) = parse_item_key(&back.key).unwrap();
        assert_eq!(size, 41_237_884);
        assert_eq!(bitrate, Some(1006));
        assert_eq!(user, "user_1");
        assert_eq!(file, name);
    }

    #[test]
    fn a_zero_bitrate_in_a_key_reads_as_unknown() {
        let (_, bitrate, _, _) = parse_item_key(r"0:0:peer:a.mp3").unwrap();
        assert_eq!(bitrate, None, "0 is the absent marker, not a real bitrate");
    }

    #[test]
    fn a_key_without_all_four_parts_is_rejected() {
        assert!(parse_item_key("123:320:user").is_none());
        assert!(parse_item_key("123:user:file.flac").is_none());
        assert!(parse_item_key("notanumber:0:user:file.flac").is_none());
        assert!(parse_item_key("123:0::file.flac").is_none());
        assert!(parse_item_key("123:0:user:").is_none());
    }

    #[test]
    fn a_batch_id_is_stable_for_the_same_file_and_differs_across_files() {
        // This is what makes a re-fetch attach instead of queueing a duplicate.
        let a = batch_id("peer", r"@@x\a - b.flac", 0);
        assert_eq!(a, batch_id("peer", r"@@x\a - b.flac", 0));
        assert_ne!(a, batch_id("peer2", r"@@x\a - b.flac", 0));
        assert_ne!(a, batch_id("peer", r"@@x\a - c.flac", 0));
        // A valid uuid, since slskd parses it as one.
        assert!(uuid::Uuid::parse_str(&a).is_ok());
    }

    #[test]
    fn each_attempt_gets_its_own_id_but_the_first_stays_put() {
        // Attempt 0 must keep hashing the bare pair, or a batch queued by an
        // earlier build would be orphaned instead of attached to.
        let base = batch_id("peer", "f.flac", 0);
        assert_eq!(
            base,
            uuid::Uuid::new_v5(&BATCH_NAMESPACE, "peer\u{1f}f.flac".as_bytes()).to_string()
        );
        let ids: Vec<String> = (0..4).map(|n| batch_id("peer", "f.flac", n)).collect();
        let mut uniq = ids.clone();
        uniq.sort();
        uniq.dedup();
        assert_eq!(uniq.len(), 4, "every attempt needs a distinct id: {ids:?}");
    }

    #[test]
    fn the_separator_stops_a_username_and_filename_from_colliding() {
        // "ab" + "c" must not hash the same as "a" + "bc".
        assert_ne!(batch_id("ab", "c", 0), batch_id("a", "bc", 0));
    }

    #[test]
    fn windows_and_unix_separators_both_yield_a_basename() {
        assert_eq!(basename(r"a\b\c.flac"), "c.flac");
        assert_eq!(basename("a/b/c.flac"), "c.flac");
        assert_eq!(
            basename(r"share\Artist - Album/02 Track.mp3"),
            "02 Track.mp3"
        );
        assert_eq!(basename("bare.flac"), "bare.flac");
    }

    #[test]
    fn slskd_gives_us_the_extension_so_we_do_not_parse_paths_for_it() {
        assert_eq!(
            format_of(Some(".flac"), r"x\y.whatever", None, false),
            Some(AudioFormat::Flac)
        );
        // Bare, and with the dot, both work.
        assert_eq!(
            format_of(Some("mp3"), "x", Some(320), false),
            Some(AudioFormat::Mp3(Some(320)))
        );
        // Falls back to the path when slskd omitted it.
        assert_eq!(
            format_of(None, r"x\y.flac", None, false),
            Some(AudioFormat::Flac)
        );
        assert_eq!(
            format_of(Some(""), r"x\y.aiff", None, false),
            Some(AudioFormat::Aiff)
        );
    }

    #[test]
    fn a_variable_bitrate_mp3_is_reported_as_v0_not_a_made_up_number() {
        // The raw protocol cannot express this; slskd can, so we should.
        assert_eq!(
            format_of(Some("mp3"), "x", Some(245), true),
            Some(AudioFormat::Mp3V0)
        );
        // VBR only reshapes MP3 — it is not a claim about lossless.
        assert_eq!(
            format_of(Some("flac"), "x", Some(1006), true),
            Some(AudioFormat::Flac)
        );
    }

    #[test]
    fn a_lossless_bitrate_never_turns_it_into_a_lossy_format() {
        assert_eq!(
            format_of(Some("flac"), "x", Some(1006), false),
            Some(AudioFormat::Flac)
        );
    }

    #[test]
    fn things_that_are_not_audio_are_not_offers() {
        assert_eq!(format_of(Some("jpg"), "cover.jpg", None, false), None);
        assert_eq!(format_of(Some("nfo"), "readme.nfo", None, false), None);
        assert_eq!(format_of(None, "noextension", None, false), None);
    }

    #[test]
    fn track_numbers_are_stripped_but_years_are_not() {
        assert_eq!(strip_track_number("03 - Xtal"), "Xtal");
        assert_eq!(strip_track_number("03. Xtal"), "Xtal");
        assert_eq!(strip_track_number("03 Xtal"), "Xtal");
        assert_eq!(strip_track_number("3) Xtal"), "Xtal");
        // Four digits is a year, not a track number.
        assert_eq!(strip_track_number("1979 - Untitled"), "1979 - Untitled");
        assert_eq!(strip_track_number("Xtal"), "Xtal");
        assert_eq!(strip_track_number("01"), "01");
    }

    #[test]
    fn artist_and_title_come_from_the_filename_when_it_has_them() {
        let (a, t, al) = describe(r"@@x\Burial - Untrue\03 - Burial - Archangel.flac");
        assert_eq!((a.as_str(), t.as_str()), ("Burial", "Archangel"));
        assert_eq!(al.as_deref(), Some("Untrue"));
    }

    #[test]
    fn artist_falls_back_to_the_release_folder() {
        let (a, t, al) = describe(r"@@x\Burial - Untrue\03 Archangel.flac");
        assert_eq!((a.as_str(), t.as_str()), ("Burial", "Archangel"));
        assert_eq!(al.as_deref(), Some("Untrue"));
    }

    #[test]
    fn a_bare_filename_still_produces_a_usable_title() {
        let (a, t, al) = describe("Archangel.flac");
        assert_eq!(a, "");
        assert_eq!(t, "Archangel");
        assert_eq!(al, None);
    }

    #[test]
    fn format_preference_accepts_an_equal_or_better_bitrate_in_the_same_family() {
        let pref = vec![
            AudioFormat::Flac,
            AudioFormat::Aiff,
            AudioFormat::Mp3(Some(320)),
            AudioFormat::Mp3V0,
        ];
        assert!(satisfies(AudioFormat::Flac, &pref));
        assert!(satisfies(AudioFormat::Mp3(Some(320)), &pref));
        assert!(satisfies(AudioFormat::Mp3(Some(330)), &pref));
        // 256 clears mp3-v0's ~245 floor, which is the point of the rule.
        assert!(satisfies(AudioFormat::Mp3(Some(256)), &pref));
        // A 128k rip is exactly what this project refuses to call an upgrade.
        assert!(!satisfies(AudioFormat::Mp3(Some(128)), &pref));
        assert!(!satisfies(AudioFormat::Wav, &pref));
        assert!(!satisfies(AudioFormat::Opus, &pref));
        // V0 itself is in the list, so a VBR file is acceptable.
        assert!(satisfies(AudioFormat::Mp3V0, &pref));
    }

    #[test]
    fn an_unknown_bitrate_is_taken_on_trust_rather_than_refused() {
        let pref = vec![AudioFormat::Flac, AudioFormat::Mp3(Some(320))];
        assert!(satisfies(AudioFormat::Mp3(None), &pref));
        assert!(!satisfies(AudioFormat::Aac(None), &pref));
    }

    #[test]
    fn lossless_families_do_not_cross_over() {
        let pref = vec![AudioFormat::Flac];
        assert!(!satisfies(AudioFormat::Wav, &pref));
        assert!(!satisfies(AudioFormat::Alac, &pref));
        assert!(!satisfies(
            AudioFormat::Aac(Some(256)),
            &[AudioFormat::Alac]
        ));
    }

    #[test]
    fn a_searched_bitrate_survives_into_the_fetch() {
        // With the bitrate dropped from the item ref, a 320kbps MP3 would arrive
        // at fetch as a bare Mp3(None) — rank 1 — and be refused by its own
        // preference list.
        let responses: Vec<api::Response> = serde_json::from_str(
            r#"[{"username":"peer","hasFreeUploadSlot":true,"uploadSpeed":1,"queueLength":0,
                 "files":[{"filename":"a - x.mp3","size":9,"extension":".mp3","bitRate":320}]}]"#,
        )
        .unwrap();
        let offers = rank_and_truncate(hits_from(&responses), 5);
        let offered = offers[0].formats.as_deref().unwrap()[0];
        assert_eq!(offered, AudioFormat::Mp3(Some(320)));

        let (_, bitrate, _, filename) = parse_item_key(&offers[0].item_ref.key).unwrap();
        assert_eq!(format_of(None, filename, bitrate, false), Some(offered));
    }

    #[test]
    fn offers_carry_everything_a_search_can_know() {
        let responses: Vec<api::Response> = serde_json::from_str(
            r#"[{"username":"peer","hasFreeUploadSlot":true,"uploadSpeed":900,"queueLength":2,
                 "files":[
                   {"filename":"@@a\\Burial - Untrue\\02 - Archangel.flac","size":40000000,
                    "extension":".flac","bitRate":1006,"bitDepth":16,"sampleRate":44100,
                    "length":238},
                   {"filename":"@@a\\Burial - Untrue\\folder.jpg","size":10,"extension":".jpg"}
                 ]}]"#,
        )
        .unwrap();
        let offers = rank_and_truncate(hits_from(&responses), 10);

        assert_eq!(offers.len(), 1, "the jpg is not an audio offer");
        let o = &offers[0];
        assert_eq!(o.backend(), ID);
        assert_eq!(o.artist, "Burial");
        assert_eq!(o.title, "Archangel");
        assert_eq!(o.album.as_deref(), Some("Untrue"));
        assert_eq!(o.duration_secs, Some(238));
        assert_eq!(o.formats.as_deref(), Some(&[AudioFormat::Flac][..]));
        assert_eq!(o.pricing, Pricing::Free);
        assert_eq!(o.ownership, Ownership::NotApplicable);
        assert!(!o.requires_purchase());
        assert_eq!(o.cost_class(), CostClass::Free);
        assert_eq!(o.has_lossless(), Some(true));
        assert!(o.url.starts_with("slsk://peer/"));
    }

    #[test]
    fn a_locked_file_is_never_offered() {
        // Visible but gated behind the peer's sharing rules, so a fetch could
        // not deliver it.
        let responses: Vec<api::Response> = serde_json::from_str(
            r#"[{"username":"peer","hasFreeUploadSlot":true,"uploadSpeed":1,"queueLength":0,
                 "files":[{"filename":"a - x.flac","size":1,"extension":".flac","isLocked":true}],
                 "lockedFiles":[{"filename":"a - y.flac","size":1,"extension":".flac"}]}]"#,
        )
        .unwrap();
        assert!(hits_from(&responses).is_empty());
    }

    #[test]
    fn a_peer_whose_name_would_break_an_item_ref_is_skipped() {
        let responses: Vec<api::Response> = serde_json::from_str(
            r#"[{"username":"we:ird","hasFreeUploadSlot":true,"uploadSpeed":1,"queueLength":0,
                 "files":[{"filename":"a.flac","size":1,"extension":".flac"}]}]"#,
        )
        .unwrap();
        assert!(hits_from(&responses).is_empty());
    }

    #[test]
    fn a_short_queue_wins_between_otherwise_equal_peers() {
        // The peer fact that best predicts whether a download ever starts.
        let responses: Vec<api::Response> = serde_json::from_str(
            r#"[{"username":"busy","hasFreeUploadSlot":true,"uploadSpeed":9999,"queueLength":400,
                 "files":[{"filename":"a - x.flac","size":1,"extension":".flac"}]},
                {"username":"quiet","hasFreeUploadSlot":true,"uploadSpeed":50,"queueLength":0,
                 "files":[{"filename":"a - x.flac","size":2,"extension":".flac"}]}]"#,
        )
        .unwrap();
        let offers = rank_and_truncate(hits_from(&responses), 10);
        assert_eq!(offers[0].item_ref.key.split(':').nth(2), Some("quiet"));
    }

    #[test]
    fn quality_still_outranks_availability() {
        let responses: Vec<api::Response> = serde_json::from_str(
            r#"[{"username":"lossy","hasFreeUploadSlot":true,"uploadSpeed":9999,"queueLength":0,
                 "files":[{"filename":"a - x.mp3","size":1,"extension":".mp3","bitRate":320}]},
                {"username":"lossless","hasFreeUploadSlot":false,"uploadSpeed":10,"queueLength":90,
                 "files":[{"filename":"a - x.flac","size":2,"extension":".flac"}]}]"#,
        )
        .unwrap();
        let offers = rank_and_truncate(hits_from(&responses), 10);
        assert_eq!(offers[0].formats.as_deref(), Some(&[AudioFormat::Flac][..]));
    }

    #[test]
    fn truncation_keeps_the_best_not_the_first_to_answer() {
        let responses: Vec<api::Response> = serde_json::from_str(
            r#"[{"username":"first","hasFreeUploadSlot":true,"uploadSpeed":1,"queueLength":0,
                 "files":[{"filename":"a - x.mp3","size":1,"extension":".mp3","bitRate":128}]},
                {"username":"second","hasFreeUploadSlot":true,"uploadSpeed":1,"queueLength":0,
                 "files":[{"filename":"a - x.flac","size":2,"extension":".flac"}]}]"#,
        )
        .unwrap();
        let offers = rank_and_truncate(hits_from(&responses), 1);
        assert_eq!(offers.len(), 1);
        assert_eq!(offers[0].formats.as_deref(), Some(&[AudioFormat::Flac][..]));
    }

    #[test]
    fn an_empty_query_never_touches_the_network() {
        // The TUI worker tests lean on this: they spawn a real registry with an
        // empty query and must not reach slskd.
        let b = backend();
        assert!(b.search(&SearchQuery::from_text("", 5)).unwrap().is_empty());
        assert!(
            b.search(&SearchQuery::from_text("   ", 5))
                .unwrap()
                .is_empty()
        );
        assert!(
            b.search(&SearchQuery::from_text("real", 0))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn only_our_own_scheme_is_claimed() {
        let b = backend();
        assert!(
            b.claim_url("https://burial.bandcamp.com/album/untrue")
                .is_none()
        );
        assert!(b.claim_url("https://soundcloud.com/x/y").is_none());
        assert!(b.claim_url("slsk://").is_none());
        assert!(b.claim_url("slsk://peer").is_none());

        let r = b.claim_url(r"slsk://peer/@@a\b - c.flac").unwrap();
        assert_eq!(r.backend, ID);
        let (size, bitrate, user, file) = parse_item_key(&r.key).unwrap();
        assert_eq!((size, bitrate, user), (0, None, "peer"));
        assert_eq!(file, r"@@a\b - c.flac");
    }

    #[test]
    fn an_unconfigured_backend_says_what_is_missing_without_connecting() {
        let b = backend();
        match b.credentials() {
            CredentialState::Missing { how_to_fix } => {
                assert!(how_to_fix.contains("url"), "got: {how_to_fix}");
            }
            other => panic!("expected Missing, got {other:?}"),
        }
        // And a real call fails as a credential problem, not a network one.
        assert!(matches!(
            b.client(),
            Err(BackendError::NoCredentials { .. })
        ));
    }

    #[test]
    fn a_url_without_an_api_key_names_the_key_as_the_gap() {
        let cfg = config::Soulseek {
            url: "https://slskd.example.com:5030".into(),
            ..Default::default()
        };
        let b = Soulseek::new(&cfg, &Credentials::default(), Duration::from_secs(5));
        if std::env::var("SLSKD_API_KEY").is_ok() {
            return; // a stray env var in the developer's shell
        }
        match b.credentials() {
            CredentialState::Missing { how_to_fix } => {
                assert!(how_to_fix.contains("api_key"), "got: {how_to_fix}");
                assert!(
                    how_to_fix.contains("slskd.example.com"),
                    "got: {how_to_fix}"
                );
            }
            other => panic!("expected Missing, got {other:?}"),
        }
    }

    #[test]
    fn the_search_window_is_clamped_to_what_slskd_accepts() {
        // slskd 400s a searchTimeout below 5, so a smaller config value would
        // make every search fail rather than merely be brief.
        let cfg = config::Soulseek {
            search_window_secs: 1,
            ..Default::default()
        };
        let b = Soulseek::new(&cfg, &Credentials::default(), Duration::from_secs(5));
        assert_eq!(b.search_window.as_secs(), api::MIN_SEARCH_WINDOW_SECS);
    }

    #[test]
    fn staging_is_namespaced_under_our_own_directory() {
        let s = Soulseek::staging("abc-123");
        assert_eq!(s, "rekord-ripper/abc-123");
        assert!(s.starts_with(STAGING_ROOT));
    }

    #[test]
    fn progress_lines_name_the_queue_position() {
        let queued: api::Transfer = serde_json::from_str(
            r#"{"id":"a","username":"p","filename":"f.flac","size":10,
                "state":"Queued, Remotely","placeInQueue":12}"#,
        )
        .unwrap();
        // A multi-hour queue looks hung without this.
        assert_eq!(progress_line(&queued), "queued at position 12");

        let running: api::Transfer = serde_json::from_str(
            r#"{"id":"a","username":"p","filename":"f.flac","size":10485760,
                "state":"InProgress","bytesTransferred":5242880,"averageSpeed":102400}"#,
        )
        .unwrap();
        let line = progress_line(&running);
        assert!(line.starts_with("50%"), "got {line}");
        assert!(line.contains("MiB"), "got {line}");

        let failed: api::Transfer = serde_json::from_str(
            r#"{"id":"a","username":"p","filename":"f.flac","size":10,
                "state":"Completed, Errored","exception":"peer went away"}"#,
        )
        .unwrap();
        assert!(progress_line(&failed).contains("peer went away"));
    }

    #[test]
    fn a_partial_file_is_refused() {
        assert!(check_size(10, 10).is_ok());
        // Unknown expected size cannot be checked against.
        assert!(check_size(10, 0).is_ok());
        let e = check_size(4, 41_000_000).unwrap_err().to_string();
        assert!(e.contains("partial file"), "got {e}");
    }
}

/// End-to-end tests against a scripted stand-in for slskd, over real HTTP on a
/// real socket. No slskd, no Soulseek account, no network — but the requests,
/// the JSON, the polling and the file collection are all the real code paths.
#[cfg(test)]
mod server_tests {
    use super::super::AcquisitionBackend;
    use super::*;
    use serde_json::json;
    use std::io::{Read, Write};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    /// What the fake answers with.
    struct Reply {
        status: u16,
        body: Vec<u8>,
        json: bool,
    }

    fn json_reply(status: u16, v: serde_json::Value) -> Reply {
        Reply {
            status,
            body: v.to_string().into_bytes(),
            json: true,
        }
    }

    fn bytes_reply(body: &[u8]) -> Reply {
        Reply {
            status: 200,
            body: body.to_vec(),
            json: false,
        }
    }

    fn empty_reply(status: u16) -> Reply {
        Reply {
            status,
            body: Vec::new(),
            json: false,
        }
    }

    struct Fake {
        base: String,
        seen: Arc<Mutex<Vec<String>>>,
        /// (path, request body) for every request that carried one.
        sent: Arc<Mutex<Vec<(String, String)>>>,
        stop: Arc<AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl Fake {
        /// `handler` is called with `"METHOD /path"` and returns the reply.
        fn start<H>(handler: H) -> Self
        where
            H: FnMut(&str, &str) -> Reply + Send + 'static,
        {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            listener.set_nonblocking(true).unwrap();

            let seen = Arc::new(Mutex::new(Vec::new()));
            let sent = Arc::new(Mutex::new(Vec::new()));
            let stop = Arc::new(AtomicBool::new(false));
            let (log, halt) = (Arc::clone(&seen), Arc::clone(&stop));
            let bodies = Arc::clone(&sent);
            let mut handler = handler;

            let handle = std::thread::spawn(move || {
                while !halt.load(Ordering::Relaxed) {
                    let Ok((mut sock, _)) = listener.accept() else {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    };
                    sock.set_nonblocking(false).ok();

                    // Read the head, then exactly as much body as declared.
                    let mut buf = Vec::new();
                    let mut byte = [0u8; 1];
                    while !buf.ends_with(b"\r\n\r\n") {
                        match sock.read(&mut byte) {
                            Ok(0) => break,
                            Ok(_) => buf.push(byte[0]),
                            Err(_) => break,
                        }
                    }
                    let head = String::from_utf8_lossy(&buf).into_owned();
                    let len: usize = head
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.trim()
                                .eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse().ok())?
                        })
                        .unwrap_or(0);
                    let mut body = String::new();
                    if len > 0 {
                        let mut raw = vec![0u8; len];
                        if sock.read_exact(&mut raw).is_ok() {
                            body = String::from_utf8_lossy(&raw).into_owned();
                        }
                    }

                    let request = head.lines().next().unwrap_or_default().to_string();
                    let mut parts = request.split_whitespace();
                    let method = parts.next().unwrap_or_default().to_string();
                    let target = parts.next().unwrap_or_default().to_string();
                    // Path only; the handler matches on that.
                    let path = target.split('?').next().unwrap_or_default().to_string();
                    log.lock().unwrap().push(format!("{method} {path}"));
                    if !body.is_empty() {
                        bodies.lock().unwrap().push((path.clone(), body));
                    }

                    let reply = handler(&method, &path);
                    let ctype = if reply.json {
                        "application/json"
                    } else {
                        "application/octet-stream"
                    };
                    let _ = write!(
                        sock,
                        "HTTP/1.1 {} X\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n\
                         Connection: close\r\n\r\n",
                        reply.status,
                        reply.body.len()
                    );
                    let _ = sock.write_all(&reply.body);
                    let _ = sock.flush();
                }
            });

            Self {
                base,
                seen,
                sent,
                stop,
                handle: Some(handle),
            }
        }

        fn backend(&self, tweak: impl FnOnce(&mut config::Soulseek)) -> Soulseek {
            self.backend_with_budget(Duration::from_secs(20), tweak)
        }

        fn backend_with_budget(
            &self,
            budget: Duration,
            tweak: impl FnOnce(&mut config::Soulseek),
        ) -> Soulseek {
            let mut cfg = config::Soulseek {
                url: self.base.clone(),
                files_url: format!("{}/files", self.base),
                search_window_secs: 5,
                fetch_timeout_secs: 30,
                ..Default::default()
            };
            tweak(&mut cfg);
            let creds = Credentials {
                soulseek: config::SoulseekCredentials {
                    api_key: Secret::new("test-key"),
                    ..Default::default()
                },
                ..Default::default()
            };
            Soulseek::new(&cfg, &creds, budget)
        }

        fn asked(&self) -> Vec<String> {
            self.seen.lock().unwrap().clone()
        }

        /// The body sent to the first request whose path contains `needle`.
        fn body_for(&self, needle: &str) -> Option<serde_json::Value> {
            self.sent
                .lock()
                .unwrap()
                .iter()
                .find(|(p, _)| p.contains(needle))
                .and_then(|(_, b)| serde_json::from_str(b).ok())
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }

    const TRACK: &str = r"@@peer\Burial - Untrue\02 - Archangel.flac";
    const AUDIO: &[u8] = b"fLaCnot really a flac, but the right length";
    const SIZE: u64 = AUDIO.len() as u64;

    fn responses() -> serde_json::Value {
        json!([{
            "username": "peer",
            "hasFreeUploadSlot": true,
            "uploadSpeed": 90000,
            "queueLength": 1,
            "files": [
                {"filename": TRACK, "size": SIZE, "extension": ".flac",
                 "bitRate": 1006, "bitDepth": 16, "sampleRate": 44100, "length": 238},
                {"filename": r"@@peer\Burial - Untrue\folder.jpg", "size": 90,
                 "extension": ".jpg"}
            ]
        }])
    }

    fn transfer(state: &str, got: u64) -> serde_json::Value {
        json!({
            "id": "t-1", "username": "peer", "filename": TRACK, "size": SIZE,
            "state": state, "bytesTransferred": got, "averageSpeed": 1048576.0,
            "placeInQueue": 3
        })
    }

    fn listing() -> serde_json::Value {
        json!({
            "files": [{"name": "02 - Archangel.flac",
                       "fullName": "/downloads/rekord-ripper/x/02 - Archangel.flac",
                       "length": SIZE}],
            "directories": []
        })
    }

    #[test]
    fn a_search_waits_for_completion_then_reads_the_responses_and_tidies_up() {
        let d = Fake::start(|method, path| match (method, path) {
            ("POST", "/api/v0/searches") => {
                json_reply(200, json!({"isComplete": false, "state": "InProgress"}))
            }
            (_, p) if p.ends_with("/responses") => json_reply(200, responses()),
            ("GET", p) if p.starts_with("/api/v0/searches/") => {
                json_reply(200, json!({"isComplete": true, "responseCount": 1}))
            }
            _ => empty_reply(204),
        });

        let offers = d
            .backend(|_| {})
            .search(&SearchQuery::from_text("burial untrue", 10))
            .unwrap();

        assert_eq!(offers.len(), 1, "the jpg is not an audio offer");
        assert_eq!(offers[0].artist, "Burial");
        assert_eq!(offers[0].title, "Archangel");
        assert_eq!(offers[0].formats.as_deref(), Some(&[AudioFormat::Flac][..]));
        assert_eq!(offers[0].pricing, Pricing::Free);

        let asked = d.asked();
        assert_eq!(asked[0], "POST /api/v0/searches");
        // The state poll has to happen before the responses are read: the POST
        // returns while the search is still InProgress with nothing in it.
        let state = asked
            .iter()
            .position(|a| a.starts_with("GET /api/v0/searches/") && !a.ends_with("/responses"));
        let read = asked.iter().position(|a| a.ends_with("/responses"));
        assert!(state.is_some(), "must poll for completion: {asked:?}");
        assert!(state < read, "must poll before reading results: {asked:?}");
        assert!(
            asked
                .iter()
                .any(|a| a.starts_with("DELETE /api/v0/searches/")),
            "our search should not be left in slskd's history: {asked:?}"
        );
    }

    #[test]
    fn the_search_window_goes_on_the_wire_in_milliseconds() {
        // The bug this guards, and it cost hours: slskd documents searchTimeout
        // as seconds but passes it to Soulseek.NET, where it is milliseconds.
        // Sending 8 asked for an 8ms search, which completes instantly with zero
        // responses — indistinguishable from a dead connection.
        let d = Fake::start(|method, path| {
            if method == "POST" {
                return json_reply(200, json!({"isComplete": true}));
            }
            if path.ends_with("/responses") {
                return json_reply(200, responses());
            }
            if method == "GET" && path.starts_with("/api/v0/searches/") {
                return json_reply(200, json!({"isComplete": true}));
            }
            empty_reply(204)
        });

        d.backend(|c| c.search_window_secs = 8)
            .search(&SearchQuery::from_text("burial untrue", 10))
            .unwrap();

        let body = d.body_for("/searches").expect("the search POST had a body");
        assert_eq!(
            body["searchTimeout"], 8000,
            "8 seconds must be sent as 8000ms, got {}",
            body["searchTimeout"]
        );
        // A file limit ends the search early and takes whoever answered first.
        assert!(
            body.get("fileLimit").is_none(),
            "no fileLimit should be sent: {body}"
        );
        assert_eq!(body["responseLimit"], 50);
    }

    #[test]
    fn a_search_still_in_progress_is_waited_out_rather_than_read_early() {
        // The bug this guards, found against a real slskd: POST /searches returns
        // immediately with `state: InProgress` and an empty response list, so
        // reading results straight away returns nothing at all.
        let polls = Arc::new(Mutex::new(0u32));
        let d = Fake::start(move |method, path| {
            if method == "POST" {
                return json_reply(200, json!({"isComplete": false}));
            }
            if path.ends_with("/responses") {
                return json_reply(200, responses());
            }
            if method == "GET" && path.starts_with("/api/v0/searches/") {
                let mut n = polls.lock().unwrap();
                *n += 1;
                // Not settled until the third ask.
                return json_reply(200, json!({"isComplete": *n >= 3}));
            }
            empty_reply(204)
        });

        let offers = d
            .backend(|_| {})
            .search(&SearchQuery::from_text("burial untrue", 10))
            .unwrap();
        assert_eq!(offers.len(), 1, "results are read once the search settles");

        let state_polls = d
            .asked()
            .iter()
            .filter(|a| a.starts_with("GET /api/v0/searches/") && !a.ends_with("/responses"))
            .count();
        assert!(
            state_polls >= 3,
            "should have polled until complete, got {state_polls}"
        );
    }

    #[test]
    fn a_search_that_never_settles_is_a_timeout_not_an_empty_result() {
        // slskd files its responses away only when the search completes, so
        // giving up early and reading them yields nothing — which must not be
        // shown as "the network has no copy of this".
        let d = Fake::start(|method, path| {
            if method == "POST" {
                return json_reply(200, json!({"isComplete": false}));
            }
            if path.ends_with("/responses") {
                return json_reply(200, responses());
            }
            json_reply(200, json!({"isComplete": false, "state": "InProgress"}))
        });

        let err = d
            .backend_with_budget(Duration::from_secs(1), |_| {})
            .search(&SearchQuery::from_text("burial untrue", 10))
            .unwrap_err();
        assert!(
            matches!(err, BackendError::Timeout { op: "search", .. }),
            "got {err:?}"
        );

        let asked = d.asked();
        assert!(
            !asked.iter().any(|a| a.ends_with("/responses")),
            "an unfinished search has nothing to read: {asked:?}"
        );
        assert!(
            asked.iter().any(|a| a.starts_with("DELETE ")),
            "the abandoned search is still tidied up: {asked:?}"
        );
    }

    #[test]
    fn a_fetch_enqueues_polls_and_collects_over_http() {
        let polls = Arc::new(Mutex::new(0u32));
        let d = Fake::start(move |method, path| {
            if method == "POST" && path.ends_with("/batches") {
                return json_reply(201, json!({"batch": {"id": "b", "transfers": []}}));
            }
            if method == "GET" && path.contains("/downloads/batches/") {
                let mut n = polls.lock().unwrap();
                *n += 1;
                let state = if *n <= 1 {
                    "Queued, Remotely"
                } else if *n == 2 {
                    "InProgress"
                } else {
                    "Completed, Succeeded"
                };
                let got = if state == "InProgress" {
                    SIZE / 2
                } else {
                    SIZE
                };
                return json_reply(200, json!({"id": "b", "transfers": [transfer(state, got)]}));
            }
            if method == "GET" && path.contains("/files/downloads/directories") {
                return json_reply(200, listing());
            }
            // The Caddy-style file route.
            if method == "GET" && path.starts_with("/files/") {
                return bytes_reply(AUDIO);
            }
            empty_reply(204)
        });

        let dest = std::env::temp_dir().join(format!("rr-slskd-{}", uuid::Uuid::new_v4()));
        let files = d
            .backend(|_| {})
            .fetch(
                &ItemRef::new(ID, item_key(SIZE, 1006, "peer", TRACK)),
                &FetchOpts {
                    dest_dir: dest.clone(),
                    format_pref: vec![AudioFormat::Flac],
                    retention: Retention::Keep,
                    overwrite: false,
                    deadline: Instant::now() + Duration::from_secs(60),
                },
            )
            .unwrap();

        assert_eq!(files.len(), 1);
        let f = &files[0];
        assert_eq!(f.format, AudioFormat::Flac);
        assert_eq!(f.bytes, SIZE);
        assert_eq!(f.artist.as_deref(), Some("Burial"));
        assert_eq!(f.title.as_deref(), Some("Archangel"));
        assert_eq!(f.album.as_deref(), Some("Untrue"));
        assert_eq!(f.source_url, format!("slsk://peer/{TRACK}"));
        assert!(f.path.starts_with(&dest));
        assert_eq!(std::fs::read(&f.path).unwrap(), AUDIO);

        let asked = d.asked();
        assert!(asked.iter().any(|a| a.ends_with("/batches")));
        // Left in place by default, so a download directory that is in slskd's
        // shares keeps sharing what it just fetched.
        assert!(
            !asked
                .iter()
                .any(|a| a.starts_with("DELETE /api/v0/files/downloads/directories")),
            "the staging directory should be left for resharing: {asked:?}"
        );

        std::fs::remove_dir_all(&dest).ok();
    }

    #[test]
    fn opting_in_to_cleanup_deletes_the_staging_dir_but_only_after_collecting() {
        let d = Fake::start(|method, path| {
            if method == "POST" && path.ends_with("/batches") {
                return json_reply(201, json!({"batch": {"id": "b", "transfers": []}}));
            }
            if method == "GET" && path.contains("/downloads/batches/") {
                return json_reply(
                    200,
                    json!({"id": "b", "transfers": [transfer("Completed, Succeeded", SIZE)]}),
                );
            }
            if method == "GET" && path.contains("/files/downloads/directories") {
                return json_reply(200, listing());
            }
            if method == "GET" && path.starts_with("/files/") {
                return bytes_reply(AUDIO);
            }
            empty_reply(204)
        });

        let dest = std::env::temp_dir().join(format!("rr-slskd-{}", uuid::Uuid::new_v4()));
        d.backend(|c| c.clean_up_remote = true)
            .fetch(
                &ItemRef::new(ID, item_key(SIZE, 1006, "peer", TRACK)),
                &FetchOpts {
                    dest_dir: dest.clone(),
                    format_pref: vec![AudioFormat::Flac],
                    retention: Retention::Keep,
                    overwrite: false,
                    deadline: Instant::now() + Duration::from_secs(60),
                },
            )
            .unwrap();

        let asked = d.asked();
        let del = asked
            .iter()
            .position(|a| a.starts_with("DELETE /api/v0/files/downloads/directories"));
        let got = asked.iter().position(|a| a.starts_with("GET /files/"));
        assert!(del.is_some(), "staging dir should be removed: {asked:?}");
        assert!(
            got < del,
            "never delete before the bytes are here: {asked:?}"
        );

        std::fs::remove_dir_all(&dest).ok();
    }

    #[test]
    fn a_duplicate_enqueue_attaches_instead_of_queueing_twice() {
        // slskd answers 409 for a batch id it already has, which is exactly what
        // a second fetch of the same offer should hit.
        let d = Fake::start(|method, path| {
            if method == "POST" && path.ends_with("/batches") {
                return json_reply(409, json!("a batch with that id already exists"));
            }
            if method == "GET" && path.contains("/downloads/batches/") {
                return json_reply(
                    200,
                    json!({"id": "b", "transfers": [transfer("Completed, Succeeded", SIZE)]}),
                );
            }
            if method == "GET" && path.contains("/files/downloads/directories") {
                return json_reply(200, listing());
            }
            if method == "GET" && path.starts_with("/files/") {
                return bytes_reply(AUDIO);
            }
            empty_reply(204)
        });

        let dest = std::env::temp_dir().join(format!("rr-slskd-{}", uuid::Uuid::new_v4()));
        let files = d
            .backend(|_| {})
            .fetch(
                &ItemRef::new(ID, item_key(SIZE, 1006, "peer", TRACK)),
                &FetchOpts {
                    dest_dir: dest.clone(),
                    format_pref: vec![AudioFormat::Flac],
                    retention: Retention::Keep,
                    overwrite: false,
                    deadline: Instant::now() + Duration::from_secs(60),
                },
            )
            .expect("a 409 means attach, not fail");

        assert_eq!(files.len(), 1);
        assert_eq!(std::fs::read(&files[0].path).unwrap(), AUDIO);
        std::fs::remove_dir_all(&dest).ok();
    }

    #[test]
    fn a_spent_batch_is_stepped_over_and_the_download_queued_again() {
        // slskd remembers a transfer as succeeded long after the file is gone,
        // because a successful fetch takes it away. Attaching to that record
        // would find nothing to collect, so a fresh batch id is used instead.
        let spent = Soulseek::staging(&batch_id("peer", TRACK, 0));
        let fresh = Soulseek::staging(&batch_id("peer", TRACK, 1));
        let enqueued = Arc::new(Mutex::new(Vec::<String>::new()));
        let log = Arc::clone(&enqueued);
        let (spent2, fresh2) = (spent.clone(), fresh.clone());

        let d = Fake::start(move |method, path| {
            if method == "POST" && path.ends_with("/batches") {
                // Only the first id is already taken.
                let n = {
                    let mut g = log.lock().unwrap();
                    g.push(path.to_string());
                    g.len()
                };
                if n == 1 {
                    return json_reply(409, json!("already exists"));
                }
                return json_reply(201, json!({"batch": {"id": "b", "transfers": []}}));
            }
            if method == "GET" && path.contains("/downloads/batches/") {
                return json_reply(
                    200,
                    json!({"id": "b", "transfers": [transfer("Completed, Succeeded", SIZE)]}),
                );
            }
            if method == "GET" && path.contains("/files/downloads/directories") {
                // The spent staging dir is gone; the fresh one has the file.
                let want = api::encode_path_segment(&api::base64_standard(spent2.as_bytes()));
                if path.contains(&want) {
                    return empty_reply(404);
                }
                let ok = api::encode_path_segment(&api::base64_standard(fresh2.as_bytes()));
                if path.contains(&ok) {
                    return json_reply(200, listing());
                }
                return json_reply(200, listing());
            }
            if method == "GET" && path.starts_with("/files/") {
                return bytes_reply(AUDIO);
            }
            empty_reply(204)
        });

        let dest = std::env::temp_dir().join(format!("rr-slskd-{}", uuid::Uuid::new_v4()));
        let files = d
            .backend(|_| {})
            .fetch(
                &ItemRef::new(ID, item_key(SIZE, 1006, "peer", TRACK)),
                &FetchOpts {
                    dest_dir: dest.clone(),
                    format_pref: vec![AudioFormat::Flac],
                    retention: Retention::Keep,
                    overwrite: false,
                    deadline: Instant::now() + Duration::from_secs(60),
                },
            )
            .expect("a spent batch should be re-queued, not a dead end");

        assert_eq!(files.len(), 1);
        assert_eq!(std::fs::read(&files[0].path).unwrap(), AUDIO);
        // Two enqueues: the collision, then the fresh id.
        assert_eq!(enqueued.lock().unwrap().len(), 2);
        // And the file came from the fresh staging directory.
        assert!(
            d.asked()
                .iter()
                .any(|a| a.starts_with("GET /files/")
                    && a.contains(fresh.split('/').nth(1).unwrap())),
            "collected from the new staging dir: {:?}",
            d.asked()
        );

        std::fs::remove_dir_all(&dest).ok();
    }

    #[test]
    fn an_in_progress_batch_is_attached_to_rather_than_requeued() {
        let enqueues = Arc::new(Mutex::new(0u32));
        let log = Arc::clone(&enqueues);
        let d = Fake::start(move |method, path| {
            if method == "POST" && path.ends_with("/batches") {
                *log.lock().unwrap() += 1;
                return json_reply(409, json!("already exists"));
            }
            if method == "GET" && path.contains("/downloads/batches/") {
                // Still working, so waiting is exactly right.
                return json_reply(
                    200,
                    json!({"id": "b", "transfers": [transfer("Completed, Succeeded", SIZE)]}),
                );
            }
            if method == "GET" && path.contains("/files/downloads/directories") {
                return json_reply(200, listing());
            }
            if method == "GET" && path.starts_with("/files/") {
                return bytes_reply(AUDIO);
            }
            empty_reply(204)
        });

        let dest = std::env::temp_dir().join(format!("rr-slskd-{}", uuid::Uuid::new_v4()));
        d.backend(|_| {})
            .fetch(
                &ItemRef::new(ID, item_key(SIZE, 1006, "peer", TRACK)),
                &FetchOpts {
                    dest_dir: dest.clone(),
                    format_pref: vec![AudioFormat::Flac],
                    retention: Retention::Keep,
                    overwrite: false,
                    deadline: Instant::now() + Duration::from_secs(60),
                },
            )
            .unwrap();

        // Succeeded *and* the file is there, so the first id is reused: exactly
        // one enqueue attempt, no stepping to a fresh id.
        assert_eq!(*enqueues.lock().unwrap(), 1);
        std::fs::remove_dir_all(&dest).ok();
    }

    #[test]
    fn a_failed_transfer_is_reported_and_never_collected() {
        let d = Fake::start(|method, path| {
            if method == "POST" && path.ends_with("/batches") {
                return json_reply(201, json!({"batch": {"id": "b", "transfers": []}}));
            }
            if method == "GET" && path.contains("/downloads/batches/") {
                return json_reply(
                    200,
                    json!({"id": "b", "transfers": [{
                        "id": "t", "username": "peer", "filename": TRACK, "size": SIZE,
                        "state": "Completed, Errored", "exception": "peer went away"
                    }]}),
                );
            }
            empty_reply(204)
        });

        let err = d
            .backend(|_| {})
            .fetch(
                &ItemRef::new(ID, item_key(SIZE, 1006, "peer", TRACK)),
                &FetchOpts {
                    dest_dir: std::env::temp_dir().join("rr-slskd-never"),
                    format_pref: vec![AudioFormat::Flac],
                    retention: Retention::Keep,
                    overwrite: false,
                    deadline: Instant::now() + Duration::from_secs(60),
                },
            )
            .unwrap_err();

        assert!(err.to_string().contains("peer went away"), "got {err}");
        // Retryable: slskd keeps the record, so another attempt is meaningful.
        assert!(err.is_retryable());
        let asked = d.asked();
        assert!(
            !asked.iter().any(|a| a.starts_with("GET /files/")),
            "a failed transfer must not be collected: {asked:?}"
        );
    }

    #[test]
    fn a_short_file_is_refused_and_leaves_nothing_behind() {
        let d = Fake::start(|method, path| {
            if method == "POST" && path.ends_with("/batches") {
                return json_reply(201, json!({"batch": {"id": "b", "transfers": []}}));
            }
            if method == "GET" && path.contains("/downloads/batches/") {
                return json_reply(
                    200,
                    json!({"id": "b", "transfers": [transfer("Completed, Succeeded", SIZE)]}),
                );
            }
            if method == "GET" && path.contains("/files/downloads/directories") {
                return json_reply(200, listing());
            }
            // Truncated: exactly what the atomic-write rule exists to catch.
            if method == "GET" && path.starts_with("/files/") {
                return bytes_reply(b"fLaC");
            }
            empty_reply(204)
        });

        let dest = std::env::temp_dir().join(format!("rr-slskd-{}", uuid::Uuid::new_v4()));
        let err = d
            .backend(|_| {})
            .fetch(
                &ItemRef::new(ID, item_key(SIZE, 1006, "peer", TRACK)),
                &FetchOpts {
                    dest_dir: dest.clone(),
                    format_pref: vec![AudioFormat::Flac],
                    retention: Retention::Keep,
                    overwrite: false,
                    deadline: Instant::now() + Duration::from_secs(60),
                },
            )
            .unwrap_err();

        assert!(err.to_string().contains("partial file"), "got {err}");
        let left: Vec<_> = std::fs::read_dir(&dest)
            .map(|d| d.flatten().map(|e| e.file_name()).collect())
            .unwrap_or_default();
        assert!(left.is_empty(), "nothing should be left behind: {left:?}");
        std::fs::remove_dir_all(&dest).ok();
    }

    #[test]
    fn a_timeout_leaves_the_transfer_queued_in_slskd() {
        let d = Fake::start(|method, path| {
            if method == "POST" && path.ends_with("/batches") {
                return json_reply(201, json!({"batch": {"id": "b", "transfers": []}}));
            }
            if method == "GET" && path.contains("/downloads/batches/") {
                // Never progresses: the peer's queue is long.
                return json_reply(
                    200,
                    json!({"id": "b", "transfers": [transfer("Queued, Remotely", 0)]}),
                );
            }
            empty_reply(204)
        });

        let err = d
            .backend(|c| c.fetch_timeout_secs = 1)
            .fetch(
                &ItemRef::new(ID, item_key(SIZE, 1006, "peer", TRACK)),
                &FetchOpts {
                    dest_dir: std::env::temp_dir().join("rr-slskd-queued"),
                    format_pref: vec![AudioFormat::Flac],
                    retention: Retention::Keep,
                    overwrite: false,
                    deadline: Instant::now() + Duration::from_secs(300),
                },
            )
            .unwrap_err();

        assert!(matches!(err, BackendError::Timeout { .. }), "got {err}");
        let asked = d.asked();
        // The queue position is the thing we waited for; cancelling would throw
        // it away and the next fetch could not attach.
        assert!(
            !asked
                .iter()
                .any(|a| a.starts_with("DELETE /api/v0/transfers")),
            "a timeout must not cancel the transfer: {asked:?}"
        );
    }

    #[test]
    fn a_local_slskd_reads_the_file_off_disk_instead_of_over_http() {
        // files_url empty means the download directory is a path we can reach.
        //
        // The file goes exactly where slskd would put it — the configured
        // download directory, plus the staging destination we asked for, plus
        // the name. `fullName` is deliberately the bare leaf here, which is what
        // slskd actually returns when a subdirectory is listed, so a path
        // composed from it would not resolve.
        let dir = std::env::temp_dir().join(format!("rr-slskd-local-{}", uuid::Uuid::new_v4()));
        let staging = Soulseek::staging(&batch_id("peer", TRACK, 0));
        let on_disk = dir.join(&staging).join("02 - Archangel.flac");
        std::fs::create_dir_all(on_disk.parent().unwrap()).unwrap();
        std::fs::write(&on_disk, AUDIO).unwrap();
        let downloads = dir.to_string_lossy().into_owned();

        let d = Fake::start(move |method, path| {
            if method == "POST" && path.ends_with("/batches") {
                return json_reply(201, json!({"batch": {"id": "b", "transfers": []}}));
            }
            if method == "GET" && path.contains("/downloads/batches/") {
                return json_reply(
                    200,
                    json!({"id": "b", "transfers": [transfer("Completed, Succeeded", SIZE)]}),
                );
            }
            if method == "GET" && path.contains("/files/downloads/directories") {
                return json_reply(
                    200,
                    json!({"files": [{"name": "02 - Archangel.flac",
                                      "fullName": "02 - Archangel.flac", "length": SIZE}],
                           "directories": []}),
                );
            }
            if method == "GET" && path == "/api/v0/options" {
                return json_reply(200, json!({"directories": {"downloads": downloads}}));
            }
            empty_reply(204)
        });

        let dest = std::env::temp_dir().join(format!("rr-slskd-{}", uuid::Uuid::new_v4()));
        let files = d
            .backend(|c| c.files_url = String::new())
            .fetch(
                &ItemRef::new(ID, item_key(SIZE, 1006, "peer", TRACK)),
                &FetchOpts {
                    dest_dir: dest.clone(),
                    format_pref: vec![AudioFormat::Flac],
                    retention: Retention::Keep,
                    overwrite: false,
                    deadline: Instant::now() + Duration::from_secs(60),
                },
            )
            .unwrap();

        assert_eq!(std::fs::read(&files[0].path).unwrap(), AUDIO);
        // Moved, not copied and left behind.
        assert!(!on_disk.exists());
        assert!(
            !d.asked().iter().any(|a| a.starts_with("GET /files/")),
            "a local download must not go over HTTP"
        );

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&dest).ok();
    }

    #[test]
    fn a_files_url_serving_the_wrong_directory_says_so() {
        // The mistake a new setup actually makes: the web route's root and
        // slskd's `directories.downloads` are different places. slskd reports
        // success, the file exists, and a bare 404 explains nothing.
        let d = Fake::start(|method, path| {
            if method == "POST" && path.ends_with("/batches") {
                return json_reply(201, json!({"batch": {"id": "b", "transfers": []}}));
            }
            if method == "GET" && path.contains("/downloads/batches/") {
                return json_reply(
                    200,
                    json!({"id": "b", "transfers": [transfer("Completed, Succeeded", SIZE)]}),
                );
            }
            if method == "GET" && path.contains("/files/downloads/directories") {
                return json_reply(200, listing());
            }
            if method == "GET" && path == "/api/v0/options" {
                return json_reply(200, json!({"directories": {"downloads": "/app/downloads"}}));
            }
            // The file route is pointed somewhere else entirely.
            if method == "GET" && path.starts_with("/files/") {
                return empty_reply(404);
            }
            empty_reply(204)
        });

        let err = d
            .backend(|_| {})
            .fetch(
                &ItemRef::new(ID, item_key(SIZE, 1006, "peer", TRACK)),
                &FetchOpts {
                    dest_dir: std::env::temp_dir().join("rr-slskd-404"),
                    format_pref: vec![AudioFormat::Flac],
                    retention: Retention::Keep,
                    overwrite: false,
                    deadline: Instant::now() + Duration::from_secs(60),
                },
            )
            .unwrap_err()
            .to_string();

        assert!(err.contains("/app/downloads"), "names slskd's dir: {err}");
        assert!(err.contains("files_url"), "names the setting: {err}");
        assert!(err.contains("same directory"), "says what to fix: {err}");
    }

    #[test]
    fn the_api_key_is_sent_on_every_request() {
        // Cheap but load-bearing: without the header slskd answers 401 and the
        // failure would look like a network problem.
        let seen_key = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&seen_key);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let mut buf = [0u8; 2048];
                let n = sock.read(&mut buf).unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase();
                if head.contains("x-api-key: test-key") {
                    flag.store(true, Ordering::Relaxed);
                }
                let _ = sock.write_all(
                    b"HTTP/1.1 200 X\r\nContent-Length: 2\r\nConnection: close\r\n\r\n[]",
                );
            }
        });

        let cfg = config::Soulseek {
            url: base,
            ..Default::default()
        };
        let creds = Credentials {
            soulseek: config::SoulseekCredentials {
                api_key: Secret::new("test-key"),
                ..Default::default()
            },
            ..Default::default()
        };
        let b = Soulseek::new(&cfg, &creds, Duration::from_secs(5));
        let _ = b.search(&SearchQuery::from_text("x", 5));
        assert!(seen_key.load(Ordering::Relaxed), "X-API-Key must be sent");
    }
}
