//! The SoundCloud backend, driven by `yt-dlp`.
//!
//! Shelling out rather than reimplementing: SoundCloud's client-id rotation, HLS
//! assembly, and throttling are moving targets that yt-dlp already tracks, and
//! duplicating that here would mean breaking every time SoundCloud changes.
//!
//! Two honest limitations, both surfaced rather than hidden:
//!
//! * A free track caps at MP3-128 unless the artist enabled the original file.
//!   That is frequently *worse* than what the user already has, so quality is
//!   reported rather than assumed to be an upgrade.
//! * Go+ tracks fail with `This video is DRM protected`. That is a clean
//!   `Unsupported`, not a bug to work around.
//!
//! Quality is resolved lazily. `--flat-playlist` carries no format list, so
//! knowing whether a track is MP3-128 or an artist-enabled original costs a
//! separate extraction per track — done for the offers the user is actually
//! weighing, never for the whole table.

use std::time::{Duration, Instant};

use super::error::{BackendError, Result};
use super::types::*;
use crate::proc;

/// SoundCloud's streaming ceiling for a track with no artist-enabled download.
const STREAM_FORMAT: AudioFormat = AudioFormat::Mp3(Some(128));

pub struct SoundCloud {
    yt_dlp: String,
    extra_args: Vec<String>,
    budget: Duration,
}

impl SoundCloud {
    pub fn new(yt_dlp: impl Into<String>, extra_args: Vec<String>, budget: Duration) -> Self {
        Self {
            yt_dlp: yt_dlp.into(),
            extra_args,
            budget,
        }
    }

    fn run_json(&self, args: &[&str]) -> Result<String> {
        let mut cmd = proc::capture(&self.yt_dlp);
        cmd.args(args).args(&self.extra_args);
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
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

/// Turn a yt-dlp failure into the right variant, so the caller can tell "you
/// can't have this" apart from "something broke".
fn classify_ytdlp_failure(tool: &str, stderr: &[u8]) -> BackendError {
    let tail = proc::stderr_tail(stderr);
    let lower = tail.to_ascii_lowercase();
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
            // A transcode is never lossless, so --lossless-only can skip this
            // backend without a single request.
            lossless_capable: false,
        }
    }

    fn credentials(&self) -> CredentialState {
        CredentialState::NotRequired
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
        let json = self.run_json(&[
            "--no-warnings",
            "--flat-playlist",
            "--dump-single-json",
            &target,
        ])?;
        parse_search(&json, query.limit)
    }

    fn enrich(&self, offers: &mut [Offer]) -> Result<()> {
        for offer in offers.iter_mut() {
            let Some(id) = track_id(&offer.item_ref.key) else {
                offer.formats = Some(vec![STREAM_FORMAT]);
                continue;
            };
            match self.run_json(&["--no-warnings", "--dump-single-json", &api_url(&id)]) {
                Ok(json) => match parse_formats(&json) {
                    Ok(formats) => offer.formats = Some(formats),
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
            "--no-warnings".into(),
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

        let mut cmd = proc::capture(&self.yt_dlp);
        cmd.args(args.drain(..)).args(&self.extra_args);
        let out = proc::run_with_deadline(cmd, Instant::now() + remaining).map_err(|e| {
            BackendError::ToolFailed {
                tool: self.yt_dlp.clone(),
                detail: e.to_string(),
            }
        })?;
        if !out.status.success() {
            return Err(classify_ytdlp_failure(&self.yt_dlp, &out.stderr));
        }

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
        SoundCloud::new("yt-dlp", vec![], Duration::from_secs(5))
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
    fn capabilities_advertise_that_a_rip_is_never_lossless() {
        let c = sc().capabilities();
        assert!(c.search && c.fetch);
        assert!(!c.lossless_capable);
        assert!(!c.requires_purchase);
        assert!(!c.price_quotes, "everything is free, so there is no price");
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
        let sc = SoundCloud::new(
            "definitely-not-yt-dlp-xyzzy",
            vec![],
            Duration::from_secs(5),
        );
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
        let sc = SoundCloud::new(
            "definitely-not-yt-dlp-xyzzy",
            vec![],
            Duration::from_secs(5),
        );
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
