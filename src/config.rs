//! `config.toml` and `credentials.toml`.
//!
//! Every field has a default, so a missing config file is not an error — the
//! tool works out of the box and reports Bandcamp as unconfigured rather than
//! failing. Unknown keys warn instead of erroring, so a config written by a
//! newer build stays usable by an older one.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths;

/// Lossless first, then the lossy fallbacks rekordbox can actually read.
/// Ogg/Vorbis is deliberately absent: rekordbox cannot open it at all, so
/// "downloaded successfully" would mean "downloaded uselessly".
const DEFAULT_FORMAT_PREFERENCE: &[&str] = &["flac", "aiff", "wav", "alac", "mp3-320", "mp3-v0"];

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub general: General,
    #[serde(default)]
    pub search: Search,
    #[serde(default)]
    pub fingerprint: Fingerprint,
    #[serde(default)]
    pub pending: Pending,
    #[serde(default)]
    pub import: Import,
    #[serde(default)]
    pub bandcamp: Bandcamp,
    #[serde(default)]
    pub soundcloud: SoundCloud,
    #[serde(default)]
    pub soulseek: Soulseek,

    /// Keys we don't recognise, kept so a round-trip doesn't delete them.
    #[serde(flatten, default, skip_serializing_if = "toml::Table::is_empty")]
    pub unknown: toml::Table,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct General {
    /// Where kept downloads land. `~/` is expanded; nothing else is.
    pub download_dir: Option<String>,
    /// Tried in order when a backend offers a choice. First match wins.
    pub format_preference: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Search {
    /// Per-backend wall-clock budget for the fan-out.
    pub timeout_secs: u64,
    /// Raw hits requested per backend.
    pub limit: usize,
    /// Top-ranked offers per backend to price-probe. 0 disables probing.
    ///
    /// This is the rate-limit mitigation: probing every hit would mean an item
    /// page fetch per result, which is the quickest route to a 429.
    pub enrich_top_n: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Fingerprint {
    pub enabled: bool,
    /// Seconds of audio fingerprinted from the start of each file. Both sides
    /// must use the same window or coverage figures aren't comparable, so this
    /// is part of the cache key.
    pub window_secs: u32,
    /// Max duration-weighted segment score. 0..32, lower is more similar.
    pub score_max: f64,
    /// Min fraction of the shorter scan covered by the best matching segment.
    pub coverage_min: f32,
    /// Max alignment offset, in chromaprint items. One item is ~124ms, so 0 is
    /// the only defensible value — it bounds the true shift to about ±62ms.
    pub shift_items_max: i64,
    /// Soft warning threshold for the fine cross-correlation lag. Not a gate:
    /// codec encoder delay makes 10-80ms normal between a stream and a lossless
    /// file of the same master.
    pub fine_shift_ms: i64,
    /// Max |duration ratio - 1| and |BPM ratio - 1| before calling it a
    /// pitched or sped-up reupload.
    pub speed_ratio_tol: f64,
    /// Pre-filter: skip fingerprinting when integer durations differ by more.
    pub duration_tol_secs: i64,
    pub cache: bool,
    /// Fetch streaming sources to a scratch file so they can be fingerprinted.
    pub stream_fetch: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Pending {
    /// Days an awaiting-import entry survives before expiring.
    pub ttl_days: i64,
    /// `watch` poll interval. Each tick is one primary-key lookup unless
    /// rekordbox's update counter moved.
    pub watch_interval_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct Import {
    /// Create the rekordbox `djmdContent` row ourselves instead of waiting for
    /// you to drag the folder in.
    ///
    /// Off by default: it writes rows into a database rekordbox owns. Even when
    /// on, the write is still gated by the running-rekordbox refuse and still
    /// takes a backup first.
    pub insert_content_rows: bool,
    /// Allow row insertion even when Cloud Library Sync is active.
    ///
    /// Separate from the above because it is a different risk. A row with
    /// `rb_local_synced = 0` is what the cloud agent pushes, so a bad insert
    /// escapes the local backup and reaches your other devices.
    pub allow_insert_when_cloud_sync: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Bandcamp {
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SoundCloud {
    pub enabled: bool,
    pub yt_dlp_path: String,
    /// Appended verbatim to every yt-dlp invocation. Output-template and
    /// progress flags are controlled by rekord-ripper and will be overridden.
    pub extra_args: Vec<String>,
}

/// Talks to a running [slskd](https://github.com/slskd/slskd) over its REST
/// API. We never start or stop one — an absent daemon is reported, not fixed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Soulseek {
    pub enabled: bool,
    /// The slskd API root, e.g. `https://slskd.example.com:5030`. Empty means
    /// the backend is unconfigured and reports itself as such.
    pub url: String,
    /// Where slskd's completed downloads are served over HTTP, e.g.
    /// `https://slskd.example.com/files` pointed at its download directory.
    ///
    /// Needed because slskd's API can list and delete downloaded files but
    /// cannot hand over their bytes. Empty means the download directory is
    /// reachable as a local path on this machine, which is the case for a local
    /// slskd and for a mounted share.
    pub files_url: String,
    /// Idle seconds slskd waits for more search responses before calling a
    /// search done. slskd requires at least 5. Counted from the *last* response,
    /// so a busy query runs longer; `search_limit` is what actually bounds it.
    pub search_window_secs: u64,
    /// Peers to accept per search before slskd stops early. The main control on
    /// how long a `shop` waits.
    pub search_limit: usize,
    /// Ceiling on one download, queue time included. Hitting it leaves the
    /// transfer running in slskd; the next `fetch` attaches to it.
    pub fetch_timeout_secs: u64,
    /// Remove our staging directory from slskd's download directory once a file
    /// has been collected and its size verified. Needs slskd's
    /// `remote_file_management` enabled; without it this is skipped quietly.
    pub clean_up_remote: bool,
}

impl Default for General {
    fn default() -> Self {
        Self {
            download_dir: None,
            format_preference: DEFAULT_FORMAT_PREFERENCE
                .iter()
                .map(|s| s.to_string())
                .collect(),
        }
    }
}

impl Default for Search {
    fn default() -> Self {
        Self {
            timeout_secs: 20,
            limit: 8,
            enrich_top_n: 5,
        }
    }
}

impl Default for Fingerprint {
    fn default() -> Self {
        // Every threshold here is an uncalibrated placeholder chosen to fail
        // closed. Calibrate with `rekord-ripper fp` before trusting an
        // unattended transfer, and record the observed numbers when you do.
        Self {
            enabled: true,
            window_secs: 120,
            score_max: 8.0,
            coverage_min: 0.80,
            shift_items_max: 0,
            fine_shift_ms: 50,
            speed_ratio_tol: 0.005,
            duration_tol_secs: 2,
            cache: true,
            stream_fetch: true,
        }
    }
}

impl Default for Pending {
    fn default() -> Self {
        Self {
            ttl_days: 14,
            watch_interval_secs: 2,
        }
    }
}

impl Default for Bandcamp {
    fn default() -> Self {
        Self { enabled: true }
    }
}

impl Default for SoundCloud {
    fn default() -> Self {
        Self {
            enabled: true,
            yt_dlp_path: "yt-dlp".into(),
            extra_args: Vec::new(),
        }
    }
}

impl Default for Soulseek {
    fn default() -> Self {
        Self {
            enabled: true,
            url: String::new(),
            files_url: String::new(),
            // slskd rejects anything below 5.
            search_window_secs: 8,
            search_limit: 50,
            fetch_timeout_secs: 1800,
            clean_up_remote: true,
        }
    }
}

impl Config {
    /// Load from `path`, or return defaults if it doesn't exist.
    ///
    /// Warnings about unknown keys go to stderr, matching how the rest of the
    /// crate separates status output from product output.
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let cfg: Config =
            toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))?;
        for key in cfg.unknown.keys() {
            eprintln!(
                "warning: unrecognised config key '{key}' in {} — ignoring",
                path.display()
            );
        }
        Ok(cfg)
    }

    /// Resolved download directory, with `~/` expanded.
    pub fn download_dir(&self) -> Result<PathBuf> {
        match self.general.download_dir.as_deref() {
            Some(s) if !s.trim().is_empty() => paths::expand_tilde(s.trim()),
            _ => paths::default_download_dir(),
        }
    }

    pub fn to_toml(&self) -> Result<String> {
        Ok(toml::to_string_pretty(self)?)
    }
}

/// A credential that must never end up in a log line, an error message, or
/// `--json` output. No `Display`, and `Debug` redacts — reading the value has to
/// be deliberate.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn is_empty(&self) -> bool {
        self.0.trim().is_empty()
    }
    pub fn len(&self) -> usize {
        self.0.trim().len()
    }
    /// Read the underlying value. Named to be conspicuous at the call site.
    pub fn expose(&self) -> &str {
        self.0.trim()
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_empty() {
            f.write_str("Secret(<empty>)")
        } else {
            write!(f, "Secret(<redacted, {} bytes>)", self.len())
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Credentials {
    #[serde(default)]
    pub bandcamp: BandcampCredentials,
    #[serde(default)]
    pub soulseek: SoulseekCredentials,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BandcampCredentials {
    /// The `identity` cookie from a logged-in browser session, copied verbatim
    /// (it is already URL-encoded — do not re-encode it).
    ///
    /// This is a full-account bearer token, not a read-only API key: it can read
    /// your purchase history and act as you. Keep this file mode 600.
    pub identity_cookie: Secret,
    /// Read the cookie from this file instead, for people who keep secrets in
    /// `pass` or similar. Takes precedence over `identity_cookie`.
    pub identity_cookie_file: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SoulseekCredentials {
    /// An slskd API key, sent as `X-API-Key`. Generate one with
    /// `slskd --generate-secret 32` and give it the `readwrite` role.
    pub api_key: Secret,
    /// Read the API key from this file instead. Takes precedence over
    /// `api_key`.
    pub api_key_file: Option<String>,
    /// Username for HTTP basic auth on `files_url`, if it is protected. Not a
    /// secret on its own, but it lives here to keep the pair together.
    pub files_user: String,
    /// Password for HTTP basic auth on `files_url`.
    pub files_password: Secret,
    /// Read the files password from this file instead.
    pub files_password_file: Option<String>,
}

/// Where a credential came from, for `backends` output. Carries the name of the
/// variable or key it came from, because each backend has its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialSource {
    Env(&'static str),
    File(&'static str),
    Config,
}

impl std::fmt::Display for CredentialSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Env(var) => write!(f, "{var} env var"),
            Self::File(key) => f.write_str(key),
            Self::Config => f.write_str("credentials.toml"),
        }
    }
}

impl Credentials {
    /// Load `credentials.toml` if present, warning when its permissions are too
    /// open. A warning rather than a refusal — it is the user's file and their
    /// call, but they should know.
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        warn_if_world_readable(path);
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading credentials {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing credentials {}", path.display()))
    }

    /// The Bandcamp identity cookie, in precedence order:
    /// `BANDCAMP_IDENTITY` env var, then `identity_cookie_file`, then the
    /// inline value. Mirrors how `REKORDBOX_KEY` overrides the built-in key.
    pub fn bandcamp_identity(&self) -> Result<Option<(Secret, CredentialSource)>> {
        if let Ok(v) = std::env::var("BANDCAMP_IDENTITY")
            && !v.trim().is_empty()
        {
            return Ok(Some((
                Secret::new(v),
                CredentialSource::Env("BANDCAMP_IDENTITY"),
            )));
        }
        if let Some(f) = self.bandcamp.identity_cookie_file.as_deref()
            && !f.trim().is_empty()
        {
            let p = paths::expand_tilde(f.trim())?;
            let v = std::fs::read_to_string(&p)
                .with_context(|| format!("reading identity_cookie_file {}", p.display()))?;
            if !v.trim().is_empty() {
                return Ok(Some((
                    Secret::new(v),
                    CredentialSource::File("identity_cookie_file"),
                )));
            }
        }
        if !self.bandcamp.identity_cookie.is_empty() {
            return Ok(Some((
                self.bandcamp.identity_cookie.clone(),
                CredentialSource::Config,
            )));
        }
        Ok(None)
    }

    /// The slskd API key, in the same precedence order: `SLSKD_API_KEY`, then
    /// `api_key_file`, then the inline value.
    pub fn soulseek_api_key(&self) -> Result<Option<(Secret, CredentialSource)>> {
        Self::resolve(
            "SLSKD_API_KEY",
            self.soulseek.api_key_file.as_deref(),
            "api_key_file",
            &self.soulseek.api_key,
        )
    }

    /// The basic-auth password for `files_url`, same precedence order.
    pub fn soulseek_files_password(&self) -> Result<Option<(Secret, CredentialSource)>> {
        Self::resolve(
            "SLSKD_FILES_PASSWORD",
            self.soulseek.files_password_file.as_deref(),
            "files_password_file",
            &self.soulseek.files_password,
        )
    }

    /// Env var, then a file, then the inline value. Factored out because there
    /// are now several of these and three hand-rolled copies would drift.
    fn resolve(
        var: &'static str,
        file_key_value: Option<&str>,
        file_key_name: &'static str,
        inline: &Secret,
    ) -> Result<Option<(Secret, CredentialSource)>> {
        if let Ok(v) = std::env::var(var)
            && !v.trim().is_empty()
        {
            return Ok(Some((Secret::new(v), CredentialSource::Env(var))));
        }
        if let Some(f) = file_key_value
            && !f.trim().is_empty()
        {
            let p = paths::expand_tilde(f.trim())?;
            let v = std::fs::read_to_string(&p)
                .with_context(|| format!("reading {file_key_name} {}", p.display()))?;
            if !v.trim().is_empty() {
                return Ok(Some((
                    Secret::new(v),
                    CredentialSource::File(file_key_name),
                )));
            }
        }
        if !inline.is_empty() {
            return Ok(Some((inline.clone(), CredentialSource::Config)));
        }
        Ok(None)
    }
}

#[cfg(unix)]
fn warn_if_world_readable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            eprintln!(
                "warning: {} is mode {mode:o} — it holds a full-account credential. \
                 chmod 600 it.",
                path.display()
            );
        }
    }
}

#[cfg(not(unix))]
fn warn_if_world_readable(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_parses_to_defaults() {
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.bandcamp.enabled);
        assert!(cfg.soundcloud.enabled);
        assert!(cfg.soulseek.enabled);
        // Unconfigured by default: there is no sensible default slskd address,
        // so it reports itself as missing rather than guessing at one.
        assert!(cfg.soulseek.url.is_empty());
        assert!(cfg.soulseek.files_url.is_empty());
        assert_eq!(cfg.soulseek.search_window_secs, 8);
        assert_eq!(cfg.search.enrich_top_n, 5);
        assert_eq!(cfg.fingerprint.window_secs, 120);
        // The two dangerous knobs must both be off without being asked for.
        assert!(!cfg.import.insert_content_rows);
        assert!(!cfg.import.allow_insert_when_cloud_sync);
    }

    #[test]
    fn partial_section_keeps_other_defaults() {
        let cfg: Config = toml::from_str("[search]\nlimit = 3\n").unwrap();
        assert_eq!(cfg.search.limit, 3);
        assert_eq!(cfg.search.enrich_top_n, 5, "untouched key kept its default");
        assert!(cfg.bandcamp.enabled, "absent section is enabled-by-default");
    }

    #[test]
    fn unknown_keys_are_preserved_not_rejected() {
        let cfg: Config = toml::from_str("[future_feature]\nx = 1\n").unwrap();
        assert!(cfg.unknown.contains_key("future_feature"));
    }

    #[test]
    fn config_round_trips_through_toml() {
        let mut cfg = Config::default();
        cfg.search.limit = 11;
        cfg.fingerprint.score_max = 6.5;
        cfg.general.download_dir = Some("~/Music/rr".into());
        let back: Config = toml::from_str(&cfg.to_toml().unwrap()).unwrap();
        assert_eq!(back.search.limit, 11);
        assert_eq!(back.fingerprint.score_max, 6.5);
        assert_eq!(back.general.download_dir.as_deref(), Some("~/Music/rr"));
    }

    #[test]
    fn default_format_preference_excludes_formats_rekordbox_cannot_read() {
        let prefs = &Config::default().general.format_preference;
        assert_eq!(prefs.first().unwrap(), "flac");
        assert!(!prefs.iter().any(|f| f == "vorbis" || f == "ogg"));
    }

    #[test]
    fn secret_debug_never_prints_the_value() {
        let s = Secret::new("super-secret-cookie-value");
        let shown = format!("{s:?}");
        assert!(!shown.contains("super-secret"), "leaked: {shown}");
        assert!(shown.contains("redacted"), "got: {shown}");
        assert_eq!(format!("{:?}", Secret::default()), "Secret(<empty>)");
    }

    #[test]
    fn secret_trims_surrounding_whitespace() {
        // A cookie pasted from a browser inspector usually arrives with a newline.
        let s = Secret::new("  abc\n");
        assert_eq!(s.expose(), "abc");
        assert!(!s.is_empty());
        assert!(Secret::new("   \n").is_empty());
    }

    #[test]
    fn inline_cookie_is_used_when_no_override_exists() {
        let creds = Credentials {
            bandcamp: BandcampCredentials {
                identity_cookie: Secret::new("from-config"),
                identity_cookie_file: None,
            },
            ..Default::default()
        };
        // Guard against a stray env var in the developer's shell.
        if std::env::var("BANDCAMP_IDENTITY").is_ok() {
            return;
        }
        let (secret, source) = creds.bandcamp_identity().unwrap().unwrap();
        assert_eq!(secret.expose(), "from-config");
        assert_eq!(source, CredentialSource::Config);
    }

    #[test]
    fn missing_cookie_reports_none_rather_than_erroring() {
        if std::env::var("BANDCAMP_IDENTITY").is_ok() {
            return;
        }
        assert!(
            Credentials::default()
                .bandcamp_identity()
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn missing_config_file_is_not_an_error() {
        let cfg = Config::load(Path::new("/nonexistent/rekord-ripper/config.toml")).unwrap();
        assert!(cfg.bandcamp.enabled);
    }
}
