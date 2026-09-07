//! Pluggable acquisition backends: search several music sources at once, compare
//! the offers, buy or rip, and hand the file to the analysis-copy pipeline.

pub mod backend;
pub mod bandcamp;
pub mod blob;
pub mod cmd;
pub mod error;
pub mod fs;
pub mod http;
pub mod pick;
pub mod render;
pub mod report;
pub mod shop;
pub mod soulseek;
pub mod soundcloud;
pub mod types;

pub use backend::AcquisitionBackend;
pub use error::{BackendError, Result};
pub use types::*;

use std::sync::Mutex;

use crate::config::{Config, Credentials};

/// Where a backend's progress lines go. Unset means stderr.
type Sink = Box<dyn Fn(&str) + Send + Sync>;

static SINK: Mutex<Option<Sink>> = Mutex::new(None);

/// Send backend progress somewhere other than stderr.
///
/// A queue position that moves twice an hour is the whole story of a Soulseek
/// download, so it has to reach the user somehow — but under the TUI stderr *is*
/// the alternate screen, where an `eprintln!` lands wherever the cursor happens
/// to sit, mid-frame, and stays until something repaints that cell. Whoever owns
/// the screen takes the lines instead and renders them where they belong.
pub fn route_progress(sink: Sink) {
    *SINK.lock().unwrap_or_else(|e| e.into_inner()) = Some(sink);
}

/// One line of backend progress. Prefer the `note!` macro.
pub fn note_line(line: &str) {
    match SINK.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        Some(sink) => sink(line),
        None => eprintln!("{line}"),
    }
}

/// `eprintln!` for backend progress and warnings — the chatter a CLI run wants
/// in its scrollback and a full-screen UI needs somewhere of its own.
macro_rules! note {
    ($($arg:tt)*) => {
        $crate::acquire::note_line(&format!($($arg)*))
    };
}
pub(crate) use note;

/// The enabled backends, in config order.
///
/// `Box<dyn>` rather than an enum: the primary operation is "walk a
/// config-determined list and call the same method on each", which is what `dyn`
/// is for, and a vtable hop is noise next to a TLS handshake. Identity stays a
/// closed enum (`BackendId`) so offers can be serialized and routed back.
pub struct Registry {
    backends: Vec<Box<dyn AcquisitionBackend>>,
}

impl Registry {
    /// Build from config. Disabled backends are absent entirely; a backend
    /// missing its credentials is still present, so `backends` can report *why*
    /// it is unusable rather than silently omitting it.
    pub fn from_config(cfg: &Config, creds: &Credentials) -> Self {
        let budget = std::time::Duration::from_secs(cfg.search.timeout_secs.max(1));
        let mut backends: Vec<Box<dyn AcquisitionBackend>> = Vec::new();
        if cfg.bandcamp.enabled {
            backends.push(Box::new(bandcamp::Bandcamp::new(creds, budget)));
        }
        if cfg.soundcloud.enabled {
            backends.push(Box::new(soundcloud::SoundCloud::new(
                &cfg.soundcloud.yt_dlp_path,
                soundcloud::Cookies::from_config(
                    &cfg.soundcloud.cookies_from_browser,
                    &cfg.soundcloud.cookies_file,
                ),
                cfg.soundcloud.extra_args.clone(),
                budget,
            )));
        }
        if cfg.soulseek.enabled {
            backends.push(Box::new(soulseek::Soulseek::new(
                &cfg.soulseek,
                creds,
                budget,
            )));
        }
        Self { backends }
    }

    pub fn is_empty(&self) -> bool {
        self.backends.is_empty()
    }

    pub fn len(&self) -> usize {
        self.backends.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = &dyn AcquisitionBackend> {
        self.backends.iter().map(|b| b.as_ref())
    }

    pub fn get(&self, id: BackendId) -> Option<&dyn AcquisitionBackend> {
        self.iter().find(|b| b.id() == id)
    }

    /// Backends that can search, for the fan-out.
    pub fn searchable(&self) -> impl Iterator<Item = &dyn AcquisitionBackend> {
        self.iter().filter(|b| b.capabilities().search)
    }

    /// The backend that claims `url`, if any.
    pub fn claim_url(&self, url: &str) -> Option<(&dyn AcquisitionBackend, ItemRef)> {
        self.iter().find_map(|b| b.claim_url(url).map(|r| (b, r)))
    }
}

/// Give a freshly downloaded `file` a cover, fetching `url` if it has none.
///
/// Called at fetch time rather than at import time on purpose. Embedding rewrites
/// the file, and both [`crate::pending`]'s size-and-mtime guard and
/// [`crate::import::existing_row_for_content`]'s dedup key off the file as it
/// stands — so the rewrite has to happen before either has recorded it. The
/// audio stream is copied, not re-encoded, so a fingerprint is unaffected.
///
/// Never fails the download: a track without a browser tile still plays.
pub fn ensure_cover(file: &std::path::Path, url: Option<&str>, deadline: std::time::Instant) {
    // A cover the file already carries is the one to keep — it came with the
    // release, where an artwork URL is whatever the listing happened to show.
    match crate::artwork::has_embedded(file) {
        Ok(true) => return,
        Ok(false) => {}
        Err(e) => {
            note!(
                "warning: could not check {} for cover art: {e}",
                file.display()
            );
            return;
        }
    }
    let Some(url) = url.map(str::trim).filter(|u| !u.is_empty()) else {
        return;
    };
    let Some(staged) = crate::artwork::sidecar_for(file) else {
        return;
    };
    if let Err(e) = download_cover(url, &staged, deadline) {
        note!("warning: could not fetch cover art from {url}: {e}");
        let _ = std::fs::remove_file(&staged);
        return;
    }
    // WAV cannot hold a picture; the sidecar stays put and the import picks it
    // up from there instead.
    if crate::artwork::container_holds_art(file) {
        match crate::artwork::embed(file, &staged) {
            // Embedded, so the sidecar has done its job.
            Ok(()) => {
                let _ = std::fs::remove_file(&staged);
            }
            Err(e) => note!(
                "warning: could not embed cover art into {}: {e}",
                file.display()
            ),
        }
    }
}

/// Download a cover to `dest`, refusing anything that is not an image.
pub fn download_cover(
    url: &str,
    dest: &std::path::Path,
    deadline: std::time::Instant,
) -> anyhow::Result<()> {
    let budget = deadline
        .saturating_duration_since(std::time::Instant::now())
        .max(std::time::Duration::from_secs(10));
    let agent = http::agent(budget);
    let body = agent.get(url).call()?.body_mut().read_to_vec()?;
    // Same guard as the audio path: an error page saved as artwork.jpg would
    // give rekordbox a blank tile and no clue why.
    if !crate::artwork::looks_like_image(&body) {
        anyhow::bail!("the server sent {} bytes that are not an image", body.len());
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(dest, &body)?;
    Ok(())
}

/// Resolve the configured format preference, dropping anything unusable.
///
/// A preference for a format rekordbox cannot open is a misconfiguration that
/// would otherwise surface as a successful, useless download — so it is filtered
/// here, loudly, once.
pub fn format_preference(cfg: &Config) -> anyhow::Result<Vec<AudioFormat>> {
    let mut out = Vec::new();
    for raw in &cfg.general.format_preference {
        match raw.parse::<AudioFormat>() {
            Ok(f) if f.usable_in_rekordbox() => out.push(f),
            Ok(f) => note!(
                "warning: format_preference lists {f}, which rekordbox cannot read — ignoring"
            ),
            Err(e) => note!("warning: {e} in format_preference — ignoring"),
        }
    }
    if out.is_empty() {
        anyhow::bail!(
            "format_preference has no formats rekordbox can read; \
             expected some of flac, aiff, wav, alac, mp3-320, mp3-v0, aac-256"
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A FLAC of silence, optionally with a cover already on it.
    fn track(dir: &std::path::Path, name: &str, cover: bool) -> Option<std::path::PathBuf> {
        std::fs::create_dir_all(dir).ok()?;
        let audio = dir.join(name);
        let mut cmd = crate::proc::capture("ffmpeg");
        cmd.args([
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=1",
        ])
        .arg(&audio);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        if crate::proc::run_with_deadline(cmd, deadline)
            .map(|o| !o.status.success())
            .unwrap_or(true)
        {
            return None; // no ffmpeg here
        }
        if cover {
            let art = dir.join("cover.jpg");
            let mut cmd = crate::proc::capture("ffmpeg");
            cmd.args(["-v", "error", "-y", "-f", "lavfi", "-i"])
                .arg("testsrc=size=200x200:duration=1")
                .args(["-frames:v", "1"])
                .arg(&art);
            crate::proc::run_with_deadline(cmd, deadline).ok()?;
            crate::artwork::embed(&audio, &art).ok()?;
        }
        Some(audio)
    }

    fn scratch() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("rr-cover-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn a_cover_the_file_already_has_is_kept_and_nothing_is_fetched() {
        // The release's own art beats whatever a listing happened to show, and
        // the short-circuit is what keeps a download from making a needless
        // request — the unreachable URL here is the proof it never calls out.
        let dir = scratch();
        let Some(audio) = track(&dir, "has-art.flac", true) else {
            return;
        };
        let before = std::fs::read(&audio).unwrap();

        ensure_cover(
            &audio,
            Some("http://127.0.0.1:1/nope.jpg"),
            std::time::Instant::now() + std::time::Duration::from_secs(30),
        );

        assert_eq!(
            std::fs::read(&audio).unwrap(),
            before,
            "file must not change"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_file_with_no_cover_and_no_url_is_left_exactly_as_it_was() {
        let dir = scratch();
        let Some(audio) = track(&dir, "no-art.flac", false) else {
            return;
        };
        let before = std::fs::read(&audio).unwrap();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        ensure_cover(&audio, None, deadline);
        ensure_cover(&audio, Some("   "), deadline);

        assert_eq!(std::fs::read(&audio).unwrap(), before);
        // And no sidecar was invented for art that does not exist.
        let sidecar = crate::artwork::sidecar_for(&audio).unwrap();
        assert!(!sidecar.exists(), "{} should not exist", sidecar.display());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_routed_note_goes_to_the_sink_instead_of_stderr() {
        // The only test that installs a sink, so it stays installed for the rest
        // of the run — harmless, since nothing else asserts on stderr.
        static SEEN: Mutex<Vec<String>> = Mutex::new(Vec::new());
        route_progress(Box::new(|line| SEEN.lock().unwrap().push(line.to_string())));

        note!("soulseek: {} at position {}", "queued", 12);
        assert!(
            SEEN.lock()
                .unwrap()
                .contains(&"soulseek: queued at position 12".to_string())
        );
    }

    #[test]
    fn default_config_registers_the_enabled_backends() {
        let reg = Registry::from_config(&Config::default(), &Credentials::default());
        assert!(!reg.is_empty());
        for id in BackendId::ALL {
            assert!(reg.get(*id).is_some(), "{id} should be enabled by default");
        }
        // Search must be available before anything is configured.
        assert!(reg.searchable().any(|b| b.id() == BackendId::Bandcamp));
        assert!(reg.searchable().any(|b| b.id() == BackendId::Soulseek));
    }

    #[test]
    fn a_disabled_backend_is_absent_entirely() {
        let mut cfg = Config::default();
        cfg.bandcamp.enabled = false;
        cfg.soulseek.enabled = false;
        let reg = Registry::from_config(&cfg, &Credentials::default());
        assert!(reg.get(BackendId::Bandcamp).is_none());
        assert!(reg.get(BackendId::Soulseek).is_none());
        assert!(reg.get(BackendId::SoundCloud).is_some());
    }

    #[test]
    fn backends_claim_only_their_own_urls() {
        let reg = Registry::from_config(&Config::default(), &Credentials::default());
        // First-match-wins across the registry, so each predicate has to be
        // narrow enough not to steal another backend's URL.
        let (b, _) = reg.claim_url(r"slsk://peer/@@a\b - c.flac").unwrap();
        assert_eq!(b.id(), BackendId::Soulseek);
        let (b, _) = reg
            .claim_url("https://soundcloud.com/artist/track")
            .unwrap();
        assert_eq!(b.id(), BackendId::SoundCloud);
    }

    #[test]
    fn url_claiming_routes_to_the_owning_backend() {
        let reg = Registry::from_config(&Config::default(), &Credentials::default());
        let (b, r) = reg
            .claim_url("https://burial.bandcamp.com/album/untrue")
            .expect("bandcamp should claim its own album url");
        assert_eq!(b.id(), BackendId::Bandcamp);
        assert_eq!(r.backend, BackendId::Bandcamp);
        assert!(reg.claim_url("https://example.com/whatever").is_none());
    }

    #[test]
    fn default_format_preference_resolves_lossless_first() {
        let prefs = format_preference(&Config::default()).unwrap();
        assert_eq!(prefs.first(), Some(&AudioFormat::Flac));
        assert!(prefs.iter().all(|f| f.usable_in_rekordbox()));
    }

    #[test]
    fn unreadable_and_unknown_formats_are_dropped_from_the_preference() {
        let mut cfg = Config::default();
        cfg.general.format_preference = vec!["vorbis".into(), "not-a-format".into(), "flac".into()];
        assert_eq!(format_preference(&cfg).unwrap(), vec![AudioFormat::Flac]);
    }

    #[test]
    fn a_preference_with_nothing_usable_is_an_error_not_a_silent_empty() {
        let mut cfg = Config::default();
        cfg.general.format_preference = vec!["vorbis".into(), "opus".into()];
        let err = format_preference(&cfg).unwrap_err().to_string();
        assert!(err.contains("no formats rekordbox can read"), "got: {err}");
    }
}
