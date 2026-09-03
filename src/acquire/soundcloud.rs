//! The SoundCloud backend, driven by `yt-dlp`.
//!
//! Shelling out rather than reimplementing: SoundCloud's client-id rotation, HLS
//! assembly, and throttling are moving targets that yt-dlp already tracks, and
//! duplicating that here would mean breaking every time SoundCloud changes.
//!
//! Two limitations, both surfaced rather than hidden:
//!
//! * A transcode is all most tracks have — `hls_aac_160k` at best. That is
//!   frequently *worse* than what the user already has, so quality is reported
//!   rather than assumed to be an upgrade.
//! * Go+ tracks fail with `This video is DRM protected`. That is a clean
//!   `Unsupported`, not a bug to work around.
//!
//! What login changes, measured rather than assumed. The ordinary ladder
//! (`hls_mp3`, `hls_aac_96k`, `hls_aac_160k`) is identical signed in or not.
//! Auth adds two things: the *original*, whose endpoint answers 401 with "only
//! available for registered users", and `hls_aac_256k` — 256 kbps, flagged
//! `Premium` — on tracks SoundCloud marks `quality: hq`, which needs Go+ and is
//! absent from an anonymous manifest. Go+ *subscription* tracks are a separate
//! case: anonymously a 30-second preview, signed in a DRM failure.
//!
//! So [`Cookies`] widens what is reachable, and every invocation carries the
//! same setting: a probe and the download it informs must not disagree.
//!
//! Quality is resolved lazily. `--flat-playlist` carries no format list, so
//! knowing whether a track is MP3-128 or an artist-enabled original costs a
//! separate extraction per track — done for the offers the user is actually
//! weighing, never for the whole table.

use std::ffi::OsStr;
use std::process::Command;
use std::time::{Duration, Instant};

use super::error::{BackendError, Result};
use super::types::*;
use crate::proc;

/// A conservative floor, used only when the format list cannot be read.
///
/// Not the anonymous ceiling — anonymous requests also see `hls_aac_160k`. This
/// deliberately under-claims, so an unreadable probe can never make a rip look
/// like an upgrade.
const STREAM_FORMAT: AudioFormat = AudioFormat::Mp3(Some(128));

/// Where yt-dlp gets its SoundCloud cookies, if anywhere.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Cookies {
    /// Anonymous. No artist-enabled originals and no Go+ 256k AAC.
    #[default]
    None,
    /// A local browser profile, e.g. `firefox` or `chrome:Profile 1`.
    Browser(String),
    /// A Netscape-format cookie jar. Validated at construction — see
    /// [`SoundCloud::check_jar`].
    File(String),
    /// Both were configured. yt-dlp would quietly take one, so every call fails
    /// with the fix instead.
    Conflict,
}

impl Cookies {
    pub fn from_config(browser: &str, file: &str) -> Self {
        match (browser.trim(), file.trim()) {
            ("", "") => Self::None,
            (b, "") => Self::Browser(b.to_string()),
            // `~/` like every other path in the config. An unexpandable one is
            // kept verbatim so `check_jar` names the path the user wrote.
            ("", f) => Self::File(
                crate::paths::expand_tilde(f)
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|_| f.to_string()),
            ),
            _ => Self::Conflict,
        }
    }

    fn args(&self) -> Vec<&str> {
        match self {
            Self::Browser(b) => vec!["--cookies-from-browser", b],
            Self::File(f) => vec!["--cookies", f],
            Self::None | Self::Conflict => Vec::new(),
        }
    }

    fn is_configured(&self) -> bool {
        matches!(self, Self::Browser(_) | Self::File(_))
    }
}

/// True for a `document.cookie` dump: `a=b; c=d` on one line.
///
/// Detected in order to *reject* it, not to convert it. It is the obvious thing
/// to reach for — devtools' cookie panel and `console.log(document.cookie)` both
/// produce it — and it cannot see `HttpOnly` cookies, so the export is silently
/// incomplete whenever the session cookie is one. Converting it would work often
/// enough to be trusted and fail invisibly the rest of the time.
///
/// A real Netscape jar is tab-separated, so the absence of tabs is the tell.
fn looks_like_header_string(raw: &str) -> bool {
    let raw = raw.trim();
    !raw.is_empty()
        && !raw.contains('\t')
        && !raw.starts_with('#')
        && raw.lines().count() == 1
        && raw.contains('=')
}

pub struct SoundCloud {
    yt_dlp: String,
    cookies: Cookies,
    /// A `cookies_file` that cannot be used. Held rather than returned because
    /// construction is infallible; every call then reports it instead of
    /// quietly running anonymously.
    file_error: Option<String>,
    extra_args: Vec<String>,
    budget: Duration,
}

impl SoundCloud {
    pub fn new(
        yt_dlp: impl Into<String>,
        cookies: Cookies,
        extra_args: Vec<String>,
        budget: Duration,
    ) -> Self {
        let file_error = match &cookies {
            Cookies::File(path) => Self::check_jar(path),
            _ => None,
        };
        Self {
            yt_dlp: yt_dlp.into(),
            cookies,
            file_error,
            extra_args,
            budget,
        }
    }

    /// Why `cookies_file` cannot be used, if it cannot.
    ///
    /// Checked once at construction: an unusable jar is a configuration mistake,
    /// and finding out per-track would report it once per offer.
    fn check_jar(path: &str) -> Option<String> {
        match std::fs::read_to_string(path) {
            Err(e) => Some(format!("cannot read {path}: {e}")),
            Ok(raw) if raw.trim().is_empty() => Some(format!("{path} is empty")),
            Ok(raw) if looks_like_header_string(&raw) => Some(format!(
                "{path} is a `document.cookie` string, not a Netscape cookie jar"
            )),
            Ok(_) => None,
        }
    }

    /// A yt-dlp command carrying the configured auth.
    ///
    /// The single place invocations are built, so a format probe and the fetch
    /// that acts on it always run as the same user.
    ///
    /// Warnings are deliberately *not* suppressed: a cookie failure is only ever
    /// a warning, stderr is captured rather than printed, and [`Self::audit_cookies`]
    /// is what decides which of it the user sees.
    fn ytdlp<I, S>(&self, args: I) -> Result<Command>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        if self.cookies == Cookies::Conflict {
            return Err(BackendError::NoCredentials {
                backend: BackendId::SoundCloud,
                how_to_fix: "cookies_from_browser and cookies_file are both set; \
                             clear one of them"
                    .into(),
            });
        }
        if let Some(detail) = &self.file_error {
            return Err(BackendError::CredentialsUnusable {
                backend: BackendId::SoundCloud,
                detail: detail.clone(),
                how_to_fix: "export a Netscape cookie jar with a cookies.txt \
                             browser extension — a `document.cookie` dump cannot \
                             include HttpOnly cookies and may be missing the \
                             session entirely"
                    .into(),
            });
        }
        let mut cmd = proc::capture(&self.yt_dlp);
        // extra_args last, so a hand-written flag still wins.
        cmd.args(args)
            .args(self.cookies.args())
            .args(&self.extra_args);
        Ok(cmd)
    }

    /// Fail loudly on a cookie problem yt-dlp only *warns* about.
    ///
    /// A keyring mismatch (the usual outcome of borrowing the `chromium` reader
    /// for a fork) leaves the run successful with an empty or half-decrypted
    /// jar. Left alone that is the worst possible outcome: `backends` reports an
    /// authenticated session, `--lossless-only` includes us, and every fetch
    /// quietly returns a transcode. So it is an error, not a note.
    fn audit_cookies(&self, stderr: &[u8]) -> Result<()> {
        if !self.cookies.is_configured() {
            return Ok(());
        }
        let text = String::from_utf8_lossy(stderr);
        let unusable = |detail: &str, how_to_fix: &str| BackendError::CredentialsUnusable {
            backend: BackendId::SoundCloud,
            detail: detail.trim().to_string(),
            how_to_fix: how_to_fix.to_string(),
        };
        for line in text.lines() {
            let lower = line.to_ascii_lowercase();
            if lower.contains("extracted 0 cookies") {
                return Err(unusable(
                    line,
                    "nothing decrypted — check the profile path, and that the \
                     browser has been signed in at least once",
                ));
            }
            // The decisive test, and the only one that is actually about
            // SoundCloud: yt-dlp asked for the original and was told to
            // register, while we believe we are signed in. Whatever the jar
            // contains, this session is anonymous to SoundCloud.
            if missed_original(line) {
                return Err(unusable(
                    line,
                    "cookies loaded but SoundCloud does not treat this session \
                     as signed in — the session cookies are probably among the \
                     ones that failed to decrypt; export a cookie jar and use \
                     cookies_file instead",
                ));
            }
            // A decrypt failure loses the *value* and keeps the name, so a jar
            // can look populated while every session cookie is an empty string
            // — which is silently anonymous. Nothing here can tell which half
            // the session landed in, so a partial failure is fatal too.
            if lower.contains("could not be decrypted") || lower.contains("failed to decrypt") {
                return Err(unusable(
                    line,
                    "the cookie encryption key does not match this browser — a \
                     Chromium fork read via `chromium:<path>` keeps its key under \
                     its own keychain name, and the names survive while the values \
                     do not; export a cookie jar and use cookies_file instead",
                ));
            }
        }
        Ok(())
    }

    /// Runs yt-dlp and returns `(stdout, stderr)`. Callers need the stderr
    /// because yt-dlp reports a *missing* format as a warning beside a
    /// successful extraction — see [`missed_original`].
    fn run_json(&self, args: &[&str]) -> Result<(String, String)> {
        let cmd = self.ytdlp(args)?;
        let out = proc::run_with_deadline(cmd, Instant::now() + self.budget).map_err(|e| {
            let msg = e.to_string();
            if msg.contains("not found") {
                BackendError::ToolMissing {
                    tool: self.yt_dlp.clone(),
                }
            } else if msg.contains("timed out") {
                BackendError::Timeout {
                    backend: BackendId::SoundCloud,
                    op: "yt-dlp",
                    elapsed: self.budget,
                }
            } else {
                BackendError::ToolFailed {
                    tool: self.yt_dlp.clone(),
                    detail: msg,
                }
            }
        })?;

        if !out.status.success() {
            return Err(classify_ytdlp_failure(&self.yt_dlp, &out.stderr));
        }
        self.audit_cookies(&out.stderr)?;
        Ok((
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ))
    }
}

/// True when yt-dlp found an artist-enabled original and was refused it.
///
/// The extraction still succeeds, so the format list looks complete while
/// silently omitting the only lossless option the track has. Worth saying out
/// loud: it is the one case where configuring cookies pays for itself.
fn missed_original(stderr: &str) -> bool {
    stderr.contains("only available for registered users")
}

/// Turn a yt-dlp failure into the right variant, so the caller can tell "you
/// can't have this" apart from "something broke".
fn classify_ytdlp_failure(tool: &str, stderr: &[u8]) -> BackendError {
    let tail = proc::stderr_tail(stderr);
    let lower = tail.to_ascii_lowercase();
    if lower.contains("unsupported browser") {
        // yt-dlp only knows a fixed list of browsers. A fork is still reachable
        // by borrowing a supported reader and giving it an absolute profile path.
        return BackendError::CredentialsUnusable {
            backend: BackendId::SoundCloud,
            detail: tail,
            how_to_fix: "cookies_from_browser must name a browser yt-dlp knows; \
                         for a Chromium fork use `chromium:/absolute/path/to/profile`"
                .into(),
        };
    }
    if lower.contains("drm") {
        // Go+ subscription audio. Nothing to work around.
        return BackendError::Unsupported {
            backend: BackendId::SoundCloud,
            op: "fetch (DRM-protected track)",
        };
    }
    if lower.contains("404") || lower.contains("not found") || lower.contains("unavailable") {
        return BackendError::NotFound {
            backend: BackendId::SoundCloud,
            item: tail,
        };
    }
    if lower.contains("429") || lower.contains("too many requests") {
        return BackendError::RateLimited {
            backend: BackendId::SoundCloud,
            retry_after: None,
        };
    }
    BackendError::ToolFailed {
        tool: tool.to_string(),
        detail: tail,
    }
}

/// Parse `yt-dlp --flat-playlist --dump-single-json scsearchN:...` output.
fn parse_search(json: &str, limit: usize) -> Result<Vec<Offer>> {
    let v: serde_json::Value = serde_json::from_str(json.trim()).map_err(|e| {
        BackendError::parse(BackendId::SoundCloud, "yt-dlp search json", e.to_string())
    })?;
    let entries = v["entries"].as_array().cloned().unwrap_or_default();

    let mut offers = Vec::new();
    for e in entries {
        if offers.len() >= limit {
            break;
        }
        let (Some(id), Some(title)) = (
            e["id"].as_str().map(str::to_string),
            e["title"].as_str().map(str::to_string),
        ) else {
            continue;
        };
        let url = e["webpage_url"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| format!("https://api.soundcloud.com/tracks/{id}"));

        let mut offer = Offer::new(
            ItemRef::new(BackendId::SoundCloud, format!("track/{id}")),
            ItemKind::Track,
            e["uploader"].as_str().unwrap_or_default().to_string(),
            title,
            url,
        );
        offer.duration_secs = e["duration"].as_f64().map(|d| d.round() as i64);
        // SoundCloud has no purchase and no ownership. Stating that up front is
        // free and correct, unlike guessing at a price.
        offer.pricing = Pricing::Free;
        offer.ownership = Ownership::NotApplicable;
        offers.push(offer);
    }
    Ok(offers)
}

/// Available formats for one track, from a full (non-flat) extraction.
///
/// yt-dlp exposes an artist-enabled original as a format whose id is `download`;
/// everything else is a transcode.
fn parse_formats(json: &str) -> Result<Vec<AudioFormat>> {
    let v: serde_json::Value = serde_json::from_str(json.trim()).map_err(|e| {
        BackendError::parse(BackendId::SoundCloud, "yt-dlp format json", e.to_string())
    })?;
    let formats = v["formats"].as_array().cloned().unwrap_or_default();

    let mut out = Vec::new();
    for f in &formats {
        let id = f["format_id"].as_str().unwrap_or_default();
        let ext = f["ext"].as_str().unwrap_or_default();
        if id == "download" {
            // The artist enabled the original file, so the extension is the truth.
            if let Ok(fmt) = ext.parse::<AudioFormat>() {
                out.push(fmt);
                continue;
            }
        }
        let abr = f["abr"].as_f64().map(|b| b.round() as u16);
        match ext {
            "mp3" => out.push(AudioFormat::Mp3(abr)),
            "opus" => out.push(AudioFormat::Opus),
            "m4a" | "aac" => out.push(AudioFormat::Aac(abr)),
            "ogg" => out.push(AudioFormat::Ogg),
            _ => {}
        }
    }
    out.dedup();
    if out.is_empty() {
        // A track we could read but whose formats we could not interpret is
        // still streamable at SoundCloud's baseline.
        out.push(STREAM_FORMAT);
    }
    Ok(out)
}

/// The numeric track id from an item ref or a rekordbox streaming URI.
///
/// rekordbox stores streaming rows as `soundcloud:tracks:<id>` in `FolderPath`,
/// and yt-dlp accepts `api.soundcloud.com/tracks/<id>` — so a streaming row that
/// has no local audio can still be fetched for a fingerprint comparison.
pub fn track_id(s: &str) -> Option<String> {
    let digits =
        |v: &str| (!v.is_empty() && v.bytes().all(|b| b.is_ascii_digit())).then(|| v.to_string());
    if let Some(rest) = s.strip_prefix("soundcloud:tracks:") {
        return digits(rest);
    }
    if let Some(rest) = s.strip_prefix("track/") {
        return digits(rest);
    }
    // api.soundcloud.com/tracks/123 or .../soundcloud%3Atracks%3A123
    if let Some(rest) = s.rsplit("/tracks/").next().filter(|r| *r != s) {
        let rest = rest.rsplit("%3A").next().unwrap_or(rest);
        return digits(rest);
    }
    None
}

/// A URL yt-dlp will accept for `id`.
pub fn api_url(id: &str) -> String {
    format!("https://api.soundcloud.com/tracks/{id}")
}

impl super::AcquisitionBackend for SoundCloud {
    fn id(&self) -> BackendId {
        BackendId::SoundCloud
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            search: true,
            // Everything is free, so there is no price to quote.
            price_quotes: false,
            ownership_check: false,
            requires_purchase: false,
            fetch: true,
            // Only an artist-enabled original is lossless, and that endpoint
            // refuses anonymous requests — so without cookies --lossless-only
            // can skip this backend without a single request.
            lossless_capable: self.cookies.is_configured(),
        }
    }

    fn credentials(&self) -> CredentialState {
        match &self.cookies {
            // Anonymous works; it just caps at MP3-128.
            Cookies::None => CredentialState::NotRequired,
            Cookies::Browser(b) => CredentialState::Present {
                hint: format!("cookies from {b}"),
            },
            Cookies::File(f) => CredentialState::Present {
                hint: format!("cookie jar {f}"),
            },
            Cookies::Conflict => CredentialState::Malformed {
                detail: "cookies_from_browser and cookies_file are both set".into(),
            },
        }
    }

    fn claim_url(&self, url: &str) -> Option<ItemRef> {
        if let Some(id) = track_id(url) {
            return Some(ItemRef::new(BackendId::SoundCloud, format!("track/{id}")));
        }
        // A plain soundcloud.com/artist/track page has no id in the URL; keep the
        // URL and let yt-dlp resolve it.
        (url.contains("soundcloud.com/") && !url.contains("/sets/"))
            .then(|| ItemRef::new(BackendId::SoundCloud, format!("url/{url}")))
    }

    fn search(&self, query: &SearchQuery) -> Result<Vec<Offer>> {
        let text = query.search_text();
        if text.is_empty() || query.limit == 0 {
            return Ok(Vec::new());
        }
        // Ask for exactly as many as we will keep.
        let target = format!("scsearch{}:{text}", query.limit);
        let (json, _) = self.run_json(&["--flat-playlist", "--dump-single-json", &target])?;
        parse_search(&json, query.limit)
    }

    fn enrich(&self, offers: &mut [Offer]) -> Result<()> {
        for offer in offers.iter_mut() {
            let Some(id) = track_id(&offer.item_ref.key) else {
                offer.formats = Some(vec![STREAM_FORMAT]);
                continue;
            };
            match self.run_json(&["--dump-single-json", &api_url(&id)]) {
                Ok((json, stderr)) => match parse_formats(&json) {
                    Ok(formats) => {
                        offer.formats = Some(formats);
                        if missed_original(&stderr) {
                            // The row is not wrong, but it is short a format we
                            // could have had — say so rather than let the user
                            // conclude MP3 is all there is.
                            offer.enrich_error = Some(
                                "this track has an artist-enabled original, which \
                                 needs a signed-in session — set \
                                 soundcloud.cookies_from_browser"
                                    .into(),
                            );
                        }
                    }
                    Err(e) => offer.enrich_error = Some(e.to_string()),
                },
                // Per-offer failure never fails the batch — a DRM track must not
                // cost us the other rows.
                Err(e) => offer.enrich_error = Some(e.to_string()),
            }
        }
        Ok(())
    }

    fn purchase(&self, _item: &ItemRef) -> Result<PurchaseFlow> {
        Ok(PurchaseFlow::NotRequired)
    }

    fn fetch(&self, item: &ItemRef, opts: &FetchOpts) -> Result<Vec<AcquiredFile>> {
        let target = fetch_target(item).ok_or_else(|| BackendError::NotFound {
            backend: BackendId::SoundCloud,
            item: item.key.clone(),
        })?;

        std::fs::create_dir_all(&opts.dest_dir)?;
        // Download into its own directory so the finished file can be identified
        // without guessing at yt-dlp's output template.
        let staging = opts.dest_dir.join(format!(".rr-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&staging)?;
        let staging = StagingDir(staging);

        // yt-dlp names and sanitises the file, so it lands in rekordbox with a
        // readable name instead of a bare track id.
        let out_template = staging.0.join("%(uploader)s - %(title)s.%(ext)s");
        let mut args: Vec<String> = vec![
            "--no-playlist".into(),
            "--no-progress".into(),
            // Prefer an artist-enabled original over any transcode; that is the
            // only case where a soundcloud rip beats what you already have.
            "-f".into(),
            "download/bestaudio/best".into(),
            // Metadata on stdout after the download, so tagging the acquired file
            // costs no extra request.
            "--print-json".into(),
            "-o".into(),
            out_template.to_string_lossy().into_owned(),
            target.clone(),
        ];
        // The deadline matters here too: a stalled download inside a scoped
        // thread would otherwise hang the caller.
        let remaining = opts
            .deadline
            .saturating_duration_since(Instant::now())
            .max(Duration::from_secs(1));

        let cmd = self.ytdlp(args.drain(..))?;
        let out = proc::run_with_deadline(cmd, Instant::now() + remaining).map_err(|e| {
            BackendError::ToolFailed {
                tool: self.yt_dlp.clone(),
                detail: e.to_string(),
            }
        })?;
        if !out.status.success() {
            return Err(classify_ytdlp_failure(&self.yt_dlp, &out.stderr));
        }
        self.audit_cookies(&out.stderr)?;

        let produced = newest_audio_file(&staging.0)?;
        let format = produced
            .extension()
            .and_then(|e| e.to_str())
            .and_then(|e| e.parse::<AudioFormat>().ok())
            .unwrap_or(STREAM_FORMAT);

        let bytes = std::fs::metadata(&produced)?.len();
        if bytes == 0 {
            return Err(BackendError::ToolFailed {
                tool: self.yt_dlp.clone(),
                detail: "produced an empty file".into(),
            });
        }

        let (artist, title) = parse_download_metadata(&String::from_utf8_lossy(&out.stdout));
        let final_path = super::fs::place(&produced, &opts.dest_dir, opts.overwrite)?;
        Ok(vec![AcquiredFile {
            path: final_path,
            format,
            bytes,
            retention: opts.retention,
            source: item.clone(),
            source_url: target,
            artist,
            title,
            album: None,
            track_number: None,
        }])
    }
}

/// A staging directory removed when the fetch finishes or fails.
struct StagingDir(std::path::PathBuf);

impl Drop for StagingDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Artist and title from the info JSON yt-dlp prints after a download.
///
/// Best-effort: missing metadata leaves the acquired file untagged rather than
/// failing a download that otherwise succeeded.
fn parse_download_metadata(stdout: &str) -> (Option<String>, Option<String>) {
    for line in stdout.lines().rev() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            let artist = ["artist", "uploader", "creator"]
                .iter()
                .find_map(|k| v[*k].as_str())
                .map(str::to_string);
            // `track` is the bare title where soundcloud has it; `title` often
            // carries an "Artist - Title" prefix instead.
            let title = ["track", "title"]
                .iter()
                .find_map(|k| v[*k].as_str())
                .map(str::to_string);
            return (artist, title);
        }
    }
    (None, None)
}

/// What to hand yt-dlp for an item ref.
fn fetch_target(item: &ItemRef) -> Option<String> {
    if let Some(url) = item.key.strip_prefix("url/") {
        return Some(url.to_string());
    }
    track_id(&item.key).map(|id| api_url(&id))
}

/// The audio file yt-dlp just wrote.
///
/// Picks the newest, ignoring the `.part` and `.ytdl` files yt-dlp leaves while
/// working, so an interrupted attempt is never mistaken for a finished download.
fn newest_audio_file(dir: &std::path::Path) -> Result<std::path::PathBuf> {
    let mut best: Option<(std::time::SystemTime, std::path::PathBuf)> = None;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if ext.is_empty() || matches!(ext.as_str(), "part" | "ytdl" | "temp" | "json") {
            continue;
        }
        let modified = entry.metadata().and_then(|m| m.modified())?;
        if best.as_ref().map(|(t, _)| modified > *t).unwrap_or(true) {
            best = Some((modified, path));
        }
    }
    best.map(|(_, p)| p)
        .ok_or_else(|| BackendError::ToolFailed {
            tool: "yt-dlp".into(),
            detail: "finished without writing an audio file".into(),
        })
}

#[cfg(test)]
mod tests {
    use super::super::AcquisitionBackend;
    use super::*;

    const SEARCH_FIXTURE: &str = r#"{"entries":[
      {"id":"46289525","title":"Neneh Cherry - Dream Baby Dream (Four Tet remix)",
       "uploader":"Four Tet","duration":530.125,
       "webpage_url":"https://soundcloud.com/four-tet/neneh-cherry-the-thing-dream"},
      {"id":"749301292","title":"Four Tet - Baby","uploader":"sound selector","duration":265.102,
       "webpage_url":"https://soundcloud.com/vybeworld/four-tet-baby-1"}
    ]}"#;

    fn sc() -> SoundCloud {
        SoundCloud::new("yt-dlp", Cookies::None, vec![], Duration::from_secs(5))
    }

    fn missing_tool(cookies: Cookies) -> SoundCloud {
        SoundCloud::new(
            "definitely-not-yt-dlp-xyzzy",
            cookies,
            vec![],
            Duration::from_secs(5),
        )
    }

    #[test]
    fn parses_a_flat_playlist_search() {
        let offers = parse_search(SEARCH_FIXTURE, 10).unwrap();
        assert_eq!(offers.len(), 2);
        assert_eq!(offers[0].artist, "Four Tet");
        assert_eq!(offers[0].duration_secs, Some(530));
        assert_eq!(offers[0].item_ref.to_string(), "soundcloud:track/46289525");
    }

    #[test]
    fn soundcloud_offers_are_free_and_ownership_does_not_apply() {
        // Known for free, so unlike bandcamp these are not left Unprobed.
        let o = &parse_search(SEARCH_FIXTURE, 1).unwrap()[0];
        assert_eq!(o.pricing, Pricing::Free);
        assert_eq!(o.ownership, Ownership::NotApplicable);
        assert!(!o.requires_purchase());
        assert_eq!(o.cost_class(), CostClass::Free);
    }

    #[test]
    fn search_leaves_formats_unprobed() {
        // --flat-playlist carries no format list, so claiming one would be a lie.
        assert_eq!(parse_search(SEARCH_FIXTURE, 1).unwrap()[0].formats, None);
    }

    #[test]
    fn search_respects_the_limit() {
        assert_eq!(parse_search(SEARCH_FIXTURE, 1).unwrap().len(), 1);
        assert_eq!(parse_search(SEARCH_FIXTURE, 0).unwrap().len(), 0);
    }

    #[test]
    fn entries_missing_an_id_or_title_are_skipped() {
        let json = r#"{"entries":[{"id":"1"},{"id":"2","title":"ok","uploader":"u"}]}"#;
        let offers = parse_search(json, 10).unwrap();
        assert_eq!(offers.len(), 1);
        assert_eq!(offers[0].title, "ok");
    }

    #[test]
    fn an_empty_search_is_success_not_failure() {
        assert!(parse_search(r#"{"entries":[]}"#, 5).unwrap().is_empty());
        assert!(parse_search(r#"{}"#, 5).unwrap().is_empty());
    }

    #[test]
    fn malformed_json_reports_a_shape_change() {
        let err = parse_search("not json", 5).unwrap_err();
        assert!(matches!(err, BackendError::Parse { .. }));
        assert!(!err.is_retryable());
    }

    #[test]
    fn a_transcode_only_track_is_reported_as_lossy() {
        let json = r#"{"formats":[
          {"format_id":"hls_mp3_0_0","ext":"mp3","abr":128.0},
          {"format_id":"http_mp3_0_0","ext":"mp3","abr":128.0},
          {"format_id":"hls_opus_0_0","ext":"opus","abr":64.0}]}"#;
        let f = parse_formats(json).unwrap();
        assert!(f.contains(&AudioFormat::Mp3(Some(128))));
        assert!(
            !f.iter().any(|x| x.is_lossless()),
            "a transcode is never lossless"
        );
    }

    #[test]
    fn an_artist_enabled_original_is_detected_as_lossless() {
        // This is the one case where a soundcloud rip is actually worth having.
        let json = r#"{"formats":[
          {"format_id":"hls_mp3_0_0","ext":"mp3","abr":128.0},
          {"format_id":"download","ext":"flac"}]}"#;
        let f = parse_formats(json).unwrap();
        assert!(f.contains(&AudioFormat::Flac));
        assert!(f.iter().any(|x| x.is_lossless()));
    }

    #[test]
    fn unreadable_formats_fall_back_to_the_streaming_baseline() {
        // Better to report soundcloud's floor than to claim we know nothing.
        assert_eq!(
            parse_formats(r#"{"formats":[]}"#).unwrap(),
            vec![STREAM_FORMAT]
        );
        assert_eq!(parse_formats(r#"{}"#).unwrap(), vec![STREAM_FORMAT]);
    }

    #[test]
    fn drm_protected_tracks_are_unsupported_not_broken() {
        let err = classify_ytdlp_failure(
            "yt-dlp",
            b"ERROR: [soundcloud] 271226416: This video is DRM protected",
        );
        assert!(
            matches!(err, BackendError::Unsupported { .. }),
            "got {err:?}"
        );
        // A fan-out should skip it quietly rather than reporting a failure.
        assert!(err.is_silently_skippable());
    }

    #[test]
    fn missing_tracks_and_rate_limits_are_classified_separately() {
        assert!(matches!(
            classify_ytdlp_failure("yt-dlp", b"ERROR: HTTP Error 404: Not Found"),
            BackendError::NotFound { .. }
        ));
        let rl = classify_ytdlp_failure("yt-dlp", b"ERROR: HTTP Error 429: Too Many Requests");
        assert!(matches!(rl, BackendError::RateLimited { .. }));
        assert!(
            rl.is_retryable(),
            "a rate limit should back off, not give up"
        );
    }

    #[test]
    fn an_unrecognised_failure_keeps_the_tools_own_message() {
        match classify_ytdlp_failure("yt-dlp", b"ERROR: something entirely new") {
            BackendError::ToolFailed { detail, .. } => {
                assert!(detail.contains("something entirely new"), "got: {detail}")
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn extracts_track_ids_from_every_form_we_encounter() {
        // The rekordbox streaming-row form is the one that makes FileType 19
        // sources fingerprintable at all.
        assert_eq!(
            track_id("soundcloud:tracks:1803453465").as_deref(),
            Some("1803453465")
        );
        assert_eq!(track_id("track/46289525").as_deref(), Some("46289525"));
        assert_eq!(
            track_id("https://api.soundcloud.com/tracks/46289525").as_deref(),
            Some("46289525")
        );
        assert_eq!(
            track_id("https://api.soundcloud.com/tracks/soundcloud%3Atracks%3A46289525").as_deref(),
            Some("46289525")
        );
    }

    #[test]
    fn rejects_things_that_are_not_track_ids() {
        assert_eq!(track_id("apple-music:tracks:123"), None);
        assert_eq!(track_id("soundcloud:tracks:notanumber"), None);
        assert_eq!(track_id("soundcloud:tracks:"), None);
        assert_eq!(track_id("https://soundcloud.com/artist/song"), None);
    }

    #[test]
    fn api_url_is_the_form_yt_dlp_accepts() {
        assert_eq!(
            api_url("1803453465"),
            "https://api.soundcloud.com/tracks/1803453465"
        );
    }

    #[test]
    fn claims_track_urls_and_ids_but_not_playlists() {
        let sc = sc();
        assert_eq!(
            sc.claim_url("https://api.soundcloud.com/tracks/46289525")
                .unwrap()
                .to_string(),
            "soundcloud:track/46289525"
        );
        assert!(
            sc.claim_url("https://soundcloud.com/four-tet/song")
                .is_some()
        );
        // A set is many tracks; that is not this feature.
        assert!(
            sc.claim_url("https://soundcloud.com/four-tet/sets/mix")
                .is_none()
        );
        assert!(
            sc.claim_url("https://burial.bandcamp.com/album/untrue")
                .is_none()
        );
    }

    #[test]
    fn capabilities_advertise_that_an_anonymous_rip_is_never_lossless() {
        let c = sc().capabilities();
        assert!(c.search && c.fetch);
        assert!(!c.lossless_capable);
        assert!(!c.requires_purchase);
        assert!(!c.price_quotes, "everything is free, so there is no price");
    }

    #[test]
    fn cookies_make_the_backend_worth_searching_for_lossless() {
        // An artist-enabled original is only reachable signed in, so
        // --lossless-only should stop skipping us once cookies are configured.
        assert!(
            missing_tool(Cookies::Browser("firefox".into()))
                .capabilities()
                .lossless_capable
        );
        assert!(
            missing_tool(Cookies::File("/tmp/cookies.txt".into()))
                .capabilities()
                .lossless_capable
        );
    }

    #[test]
    fn cookie_config_maps_to_the_right_yt_dlp_flags() {
        assert_eq!(Cookies::from_config("", ""), Cookies::None);
        assert_eq!(Cookies::from_config("  ", ""), Cookies::None);
        assert_eq!(
            Cookies::from_config("firefox", "").args(),
            ["--cookies-from-browser", "firefox"]
        );
        assert_eq!(
            Cookies::from_config("", " /tmp/c.txt ").args(),
            ["--cookies", "/tmp/c.txt"]
        );
        assert!(Cookies::None.args().is_empty());
    }

    #[test]
    fn a_cookie_jar_path_expands_a_leading_tilde() {
        // Every other path in the config does, and yt-dlp is not a shell.
        let home = std::env::var("HOME").unwrap();
        assert_eq!(
            Cookies::from_config("", "~/sc_cookies.txt"),
            Cookies::File(format!("{home}/sc_cookies.txt"))
        );
    }

    #[test]
    fn setting_both_cookie_sources_is_refused_not_silently_resolved() {
        // yt-dlp would take one of them; which one is not something to guess at.
        let both = Cookies::from_config("firefox", "/tmp/c.txt");
        assert_eq!(both, Cookies::Conflict);

        // The tool name is deliberately bogus: a conflict must fail before we
        // ever spawn anything.
        let sc = missing_tool(both);
        let err = sc
            .search(&SearchQuery::from_text("anything", 1))
            .unwrap_err();
        assert!(
            matches!(err, BackendError::NoCredentials { .. }),
            "got {err:?}"
        );
        assert!(err.needs_user_action());
        assert!(matches!(
            sc.credentials(),
            CredentialState::Malformed { .. }
        ));
        assert!(!sc.credentials().is_usable());
    }

    #[test]
    fn every_invocation_carries_the_same_auth() {
        // The probe reports what the fetch will get only if both run signed in.
        let sc = SoundCloud::new(
            "yt-dlp",
            Cookies::Browser("firefox".into()),
            vec!["--sleep-requests".into(), "1".into()],
            Duration::from_secs(5),
        );
        let args = |c: Command| {
            c.get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };
        let probe = args(sc.ytdlp(["--dump-single-json", "url"]).unwrap());
        assert_eq!(
            probe,
            [
                "--dump-single-json",
                "url",
                "--cookies-from-browser",
                "firefox",
                "--sleep-requests",
                "1"
            ]
        );
        assert_eq!(
            &args(sc.ytdlp(["-f", "download"]).unwrap())[2..],
            &probe[2..]
        );
    }

    #[test]
    fn a_partly_decrypted_jar_is_fatal_because_the_names_outlive_the_values() {
        // Observed borrowing the `chromium` reader for Helium: the jar looks
        // populated, every soundcloud.com value is the empty string, and
        // yt-dlp never even attempts a login. Tolerating this is what "silently
        // anonymous" looks like.
        let sc = missing_tool(Cookies::Browser("chromium:/tmp/helium".into()));
        let err = sc
            .audit_cookies(
                b"WARNING: failed to decrypt cookie (AES-CBC) because UTF-8 decoding failed\n\
                  Extracted 2192 cookies from chromium (1537 could not be decrypted)",
            )
            .unwrap_err();
        match &err {
            BackendError::CredentialsUnusable { how_to_fix, .. } => {
                assert!(how_to_fix.contains("cookies_file"), "got {how_to_fix}")
            }
            other => panic!("got {other:?}"),
        }
        assert!(err.needs_user_action());
        assert!(!err.is_retryable(), "a wrong key is wrong on the retry too");
        assert!(!err.is_silently_skippable(), "this must not be swallowed");
    }

    #[test]
    fn a_session_soundcloud_does_not_accept_is_an_error_not_a_quiet_downgrade() {
        // The decisive signal: we believe we are signed in and SoundCloud is
        // still refusing the original. Every fetch would silently be anonymous.
        let sc = missing_tool(Cookies::Browser("chromium:/tmp/helium".into()));
        let err = sc
            .audit_cookies(
                b"WARNING: Original download format is only available for registered users.",
            )
            .unwrap_err();
        match &err {
            BackendError::CredentialsUnusable { how_to_fix, .. } => {
                assert!(how_to_fix.contains("cookies_file"), "got {how_to_fix}")
            }
            other => panic!("got {other:?}"),
        }
        assert!(err.needs_user_action());
        assert!(!err.is_retryable(), "a wrong key is wrong on the retry too");
        assert!(!err.is_silently_skippable(), "this must not be swallowed");
        assert_eq!(err.backend(), Some(BackendId::SoundCloud));
    }

    #[test]
    fn a_document_cookie_dump_is_rejected_with_the_real_instructions() {
        // The obvious thing to reach for, and it cannot see HttpOnly cookies —
        // so accepting it would work by luck and fail invisibly.
        let dir = std::env::temp_dir().join(format!("rr-sc-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("header.txt");
        std::fs::write(
            &path,
            "sc_theme=dark; oauth_token=2-314159; sc_session=abc\n",
        )
        .unwrap();

        let sc = SoundCloud::new(
            "yt-dlp",
            Cookies::File(path.to_string_lossy().into_owned()),
            vec![],
            Duration::from_secs(5),
        );
        let err = sc.ytdlp(["--version"]).unwrap_err();
        match &err {
            BackendError::CredentialsUnusable {
                detail, how_to_fix, ..
            } => {
                assert!(detail.contains("document.cookie"), "got {detail}");
                assert!(how_to_fix.contains("HttpOnly"), "got {how_to_fix}");
            }
            other => panic!("got {other:?}"),
        }
        assert!(err.needs_user_action());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_real_netscape_jar_is_accepted_and_an_unreadable_one_is_not() {
        let dir = std::env::temp_dir().join(format!("rr-sc-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("jar.txt");
        std::fs::write(
            &path,
            "# Netscape HTTP Cookie File\n\
             .soundcloud.com\tTRUE\t/\tTRUE\t2000000000\toauth_token\t2-314159\n",
        )
        .unwrap();
        let jar = |p: String| SoundCloud::new("yt-dlp", Cookies::File(p), vec![], Duration::ZERO);
        assert!(
            jar(path.to_string_lossy().into_owned())
                .ytdlp(["--version"])
                .is_ok()
        );
        // Tabs are the tell, so a jar is never mistaken for a header string.
        assert!(!looks_like_header_string(
            ".soundcloud.com\tTRUE\t/\tTRUE\t1\ta\tb"
        ));
        assert!(looks_like_header_string("a=b; c=d"));
        assert!(!looks_like_header_string("   "));

        let missing = dir.join("nope.txt").to_string_lossy().into_owned();
        assert!(matches!(
            jar(missing).ytdlp(["--version"]),
            Err(BackendError::CredentialsUnusable { .. })
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_jar_is_caught() {
        let sc = missing_tool(Cookies::File("/tmp/c.txt".into()));
        assert!(
            sc.audit_cookies(b"Extracted 0 cookies from chromium")
                .is_err()
        );
    }

    #[test]
    fn anonymous_runs_are_never_audited_for_cookie_problems() {
        // Nothing was configured, so neither a stray warning nor a refused
        // original is a credentials failure — the latter is an offer note.
        let sc = sc();
        assert!(
            sc.audit_cookies(b"Extracted 0 cookies from chromium")
                .is_ok()
        );
        assert!(
            sc.audit_cookies(b"Original download format is only available for registered users")
                .is_ok()
        );
    }

    #[test]
    fn a_clean_signed_in_run_passes_the_audit() {
        let sc = missing_tool(Cookies::Browser("firefox".into()));
        assert!(
            sc.audit_cookies(b"Extracted 2192 cookies from firefox")
                .is_ok()
        );
        assert!(sc.audit_cookies(b"").is_ok());
    }

    #[test]
    fn an_unsupported_browser_names_the_fix_rather_than_leaking_stderr() {
        let err = classify_ytdlp_failure(
            "yt-dlp",
            b"yt-dlp: error: unsupported browser specified for cookies: \"helium\". \
              Supported browsers are: brave, chrome, chromium, edge, firefox, opera, \
              safari, vivaldi, whale",
        );
        match &err {
            BackendError::CredentialsUnusable { how_to_fix, .. } => {
                assert!(how_to_fix.contains("chromium:"), "got {how_to_fix}")
            }
            other => panic!("got {other:?}"),
        }
        assert!(err.needs_user_action());
    }

    #[test]
    fn a_refused_original_is_reported_on_the_offer() {
        // yt-dlp warns and carries on, so the format list looks complete while
        // missing the only lossless option the track has.
        assert!(missed_original(
            "WARNING: Original download format is only available for registered users. \
             Use --cookies-from-browser"
        ));
        assert!(!missed_original("Extracted 12 cookies from firefox"));
    }

    #[test]
    fn configured_cookies_are_reported_as_present() {
        assert!(matches!(sc().credentials(), CredentialState::NotRequired));
        match missing_tool(Cookies::Browser("firefox".into())).credentials() {
            CredentialState::Present { hint } => assert!(hint.contains("firefox"), "got {hint}"),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn nothing_needs_buying() {
        assert!(matches!(
            sc().purchase(&ItemRef::new(BackendId::SoundCloud, "track/1"))
                .unwrap(),
            PurchaseFlow::NotRequired
        ));
        assert!(matches!(sc().credentials(), CredentialState::NotRequired));
    }

    #[test]
    fn a_missing_yt_dlp_is_reported_by_name() {
        let sc = missing_tool(Cookies::None);
        let err = sc
            .search(&SearchQuery::from_text("anything", 1))
            .unwrap_err();
        assert!(
            matches!(err, BackendError::ToolMissing { .. }),
            "got {err:?}"
        );
        assert!(err.needs_user_action());
    }

    #[test]
    fn an_empty_query_makes_no_subprocess_call() {
        // Guards against spawning yt-dlp with a meaningless search term.
        let sc = missing_tool(Cookies::None);
        assert!(
            sc.search(&SearchQuery::from_text("   ", 5))
                .unwrap()
                .is_empty()
        );
        assert!(
            sc.search(&SearchQuery::from_text("x", 0))
                .unwrap()
                .is_empty()
        );
    }
}
