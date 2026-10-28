//! The `config.toml` and `credentials.toml` keys a settings screen edits,
//! shared by every frontend.
//!
//! Edits go through `toml_edit`, so the file's comments and layout survive.
//! Compat levels and unknown keys are left to the file itself.

use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use toml_edit::{Array, DocumentMut, Item, Value};

use crate::acquire::AudioFormat;
use crate::config::{Config, CredentialSource, Credentials};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    Bool,
    Int {
        min: i64,
        max: i64,
    },
    Float {
        min: f64,
        max: f64,
    },
    Text,
    /// A file or folder. `~/` is expanded where it is used.
    Path,
    /// Comma-separated.
    List,
}

#[derive(Debug)]
pub struct Field {
    /// `section.key`, as in the file.
    pub key: &'static str,
    /// The heading a frontend groups it under.
    pub group: &'static str,
    pub label: &'static str,
    pub help: &'static str,
    pub kind: Kind,
}

const fn field(
    key: &'static str,
    group: &'static str,
    label: &'static str,
    kind: Kind,
    help: &'static str,
) -> Field {
    Field {
        key,
        group,
        label,
        help,
        kind,
    }
}

const fn int(min: i64, max: i64) -> Kind {
    Kind::Int { min, max }
}

const fn float(min: f64, max: f64) -> Kind {
    Kind::Float { min, max }
}

pub const DOWNLOADS: &str = "Downloads";
pub const SEARCH: &str = "Search";
pub const BANDCAMP: &str = "Bandcamp";
pub const SOUNDCLOUD: &str = "SoundCloud";
pub const SOULSEEK: &str = "Soulseek";
pub const FINGERPRINT: &str = "Fingerprint Gate";
pub const TOOLS: &str = "Tools";

#[rustfmt::skip]
pub const FIELDS: &[Field] = &[
    field("general.download_dir", DOWNLOADS, "Download folder", Kind::Path,
        "Where downloads land. Empty uses the incoming folder in the app's data folder."),
    field("general.format_preference", DOWNLOADS, "Format preference", Kind::List,
        "Tried in order; the first format a source offers wins."),
    field("import.insert_content_rows", DOWNLOADS, "Import downloads into rekordbox", Kind::Bool,
        "Adds each download to master.db itself, after a backup. Off, you drag the download folder into rekordbox."),
    field("pending.ttl_days", DOWNLOADS, "Keep unimported downloads for (days)", int(1, 3650),
        "A download not imported by then is dropped from the queue."),
    field("tools.ffmpeg_path", TOOLS, "ffmpeg", Kind::Path,
        "A command name or a full path. Reads, converts and fingerprints audio."),
    field("tools.ffprobe_path", TOOLS, "ffprobe", Kind::Path,
        "A command name or a full path. Reads a download's format before import."),
    field("search.timeout_secs", SEARCH, "Time limit per source (s)", int(1, 600), ""),
    field("search.limit", SEARCH, "Results per source", int(1, 100), ""),
    field("search.enrich_top_n", SEARCH, "Price-check the top results", int(0, 50),
        "Each check fetches an item page, and too many invite rate limits. 0 turns it off."),
    field("search.bulk_max", SEARCH, "Tracks per bulk search", int(1, 1000),
        "A larger selection is searched in batches of this size."),
    field("bandcamp.enabled", BANDCAMP, "Search Bandcamp", Kind::Bool, ""),
    field("soundcloud.enabled", SOUNDCLOUD, "Search SoundCloud", Kind::Bool, ""),
    field("soundcloud.yt_dlp_path", SOUNDCLOUD, "yt-dlp", Kind::Path,
        "A command name or a full path."),
    field("soundcloud.cookies_from_browser", SOUNDCLOUD, "Cookies from browser", Kind::Text,
        "For example firefox, or chrome:Profile 1. Signed in, SoundCloud offers better formats."),
    field("soundcloud.cookies_file", SOUNDCLOUD, "Cookie file", Kind::Path,
        "A Netscape-format cookie jar, for a browser that isn't on this Mac."),
    field("soundcloud.extra_args", SOUNDCLOUD, "Extra yt-dlp arguments", Kind::List, ""),
    field("soulseek.enabled", SOULSEEK, "Search Soulseek", Kind::Bool, ""),
    field("soulseek.url", SOULSEEK, "slskd address", Kind::Text,
        "For example https://slskd.example.com:5030. Empty leaves Soulseek unconfigured."),
    field("soulseek.files_url", SOULSEEK, "Downloads URL", Kind::Text,
        "Where slskd's downloads are served over HTTP. Empty means they are reachable on this Mac."),
    field("soulseek.search_window_secs", SOULSEEK, "Wait after the last response (s)", int(5, 600), ""),
    field("soulseek.search_limit", SOULSEEK, "Peers per search", int(1, 1000), ""),
    field("soulseek.fetch_timeout_secs", SOULSEEK, "Download time limit (s)", int(60, 86400),
        "A download still running then is left to slskd and picked up next time."),
    field("soulseek.clean_up_remote", SOULSEEK, "Delete from slskd once collected", Kind::Bool,
        "Stops sharing what you downloaded. Needs slskd's remote_file_management."),
    field("soulseek.fetch_folder_image", SOULSEEK, "Fetch folder cover images", Kind::Bool,
        "A second transfer from the same peer, tried only when the file has no cover."),
    field("fingerprint.window_secs", FINGERPRINT, "Window (s)", int(10, 600),
        "Seconds fingerprinted from the start of each file."),
    field("fingerprint.score_max", FINGERPRINT, "Maximum score", float(0.0, 32.0),
        "0 to 32; lower is stricter."),
    field("fingerprint.coverage_min", FINGERPRINT, "Minimum coverage", float(0.0, 1.0),
        "Fraction of the shorter file the best match must cover."),
    field("fingerprint.shift_items_max", FINGERPRINT, "Maximum shift (items)", int(0, 100),
        "One item is about 124 ms. Anything above 0 lets cues land off the beat."),
    field("fingerprint.fine_shift_ms", FINGERPRINT, "Warn above a shift of (ms)", int(0, 10000),
        "A warning, not a refusal."),
    field("fingerprint.speed_ratio_tol", FINGERPRINT, "Speed tolerance", float(0.0, 1.0),
        "Largest length or BPM ratio difference before calling it a re-pitched upload."),
    field("fingerprint.duration_tol_secs", FINGERPRINT, "Length tolerance (s)", int(0, 600), ""),
    field("fingerprint.cache", FINGERPRINT, "Cache fingerprints", Kind::Bool, ""),
    field("fingerprint.stream_fetch", FINGERPRINT, "Fetch streams to fingerprint them", Kind::Bool, ""),
];

pub fn field_for(key: &str) -> Option<&'static Field> {
    FIELDS.iter().find(|f| f.key == key)
}

/// The value `cfg` holds for `field`, as a settings screen shows it. Empty for
/// an unset optional key.
pub fn current(cfg: &Config, field: &Field) -> String {
    let Ok(table) = toml::Table::try_from(cfg) else {
        return String::new();
    };
    let (section, name) = split(field.key);
    match table.get(section).and_then(|s| s.get(name)) {
        Some(toml::Value::String(s)) => s.clone(),
        Some(toml::Value::Boolean(b)) => b.to_string(),
        Some(toml::Value::Integer(i)) => i.to_string(),
        // f32 fields widen to f64 noise, so round to what anyone would type.
        Some(toml::Value::Float(f)) => ((f * 1e6).round() / 1e6).to_string(),
        Some(toml::Value::Array(a)) => a
            .iter()
            .filter_map(|v| v.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        _ => String::new(),
    }
}

/// `text` with `key` set from `raw`, checked to load before it is returned.
/// Empty text, list or path removes the key, leaving its default.
pub fn edit(text: &str, key: &str, raw: &str) -> Result<String> {
    let field = field_for(key).ok_or_else(|| anyhow!("{key} is not a setting"))?;
    let out = set_key(text, key, parse(field, raw)?)?;
    let cfg: Config = toml::from_str(&out).context("the edited config.toml does not load")?;
    check(&cfg)?;
    Ok(out)
}

/// `text` with `key` set to `value`, or removed for `None`.
fn set_key(text: &str, key: &str, value: Option<Value>) -> Result<String> {
    let mut doc: DocumentMut = text.parse().context("the file does not parse")?;
    let (section, name) = split(key);
    let table = doc
        .entry(section)
        .or_insert(toml_edit::table())
        .as_table_like_mut()
        .ok_or_else(|| anyhow!("[{section}] is not a table"))?;
    match value {
        None => {
            table.remove(name);
        }
        Some(mut v) => match table.get_mut(name) {
            Some(Item::Value(old)) => {
                // Keeps a trailing comment on the line.
                *v.decor_mut() = old.decor().clone();
                *old = v;
            }
            _ => {
                table.insert(name, Item::Value(v));
            }
        },
    }
    Ok(doc.to_string())
}

fn parse(field: &Field, raw: &str) -> Result<Option<Value>> {
    let raw = raw.trim();
    let label = field.label;
    Ok(Some(match field.kind {
        Kind::Bool => match raw {
            "true" => true.into(),
            "false" => false.into(),
            _ => bail!("{label} must be true or false"),
        },
        Kind::Int { min, max } => {
            let n: i64 = raw
                .parse()
                .map_err(|_| anyhow!("{label} must be a whole number"))?;
            if !(min..=max).contains(&n) {
                bail!("{label} must be between {min} and {max}");
            }
            n.into()
        }
        Kind::Float { min, max } => {
            let n: f64 = raw
                .parse()
                .map_err(|_| anyhow!("{label} must be a number"))?;
            if !(min..=max).contains(&n) {
                bail!("{label} must be between {min} and {max}");
            }
            n.into()
        }
        Kind::Text | Kind::Path if raw.is_empty() => return Ok(None),
        Kind::Text | Kind::Path => raw.into(),
        Kind::List => {
            let items: Array = raw
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect();
            if items.is_empty() {
                return Ok(None);
            }
            items.into()
        }
    }))
}

/// What loading would only warn about, refused before it is written.
fn check(cfg: &Config) -> Result<()> {
    for raw in &cfg.general.format_preference {
        let f: AudioFormat = raw
            .parse()
            .map_err(|e| anyhow!("{e} in format preference"))?;
        if !f.usable_in_rekordbox() {
            bail!("rekordbox cannot read {f}");
        }
    }
    let sc = &cfg.soundcloud;
    if !sc.cookies_from_browser.trim().is_empty() && !sc.cookies_file.trim().is_empty() {
        bail!("SoundCloud takes cookies from a browser or a file, not both: clear one first");
    }
    Ok(())
}

/// A `credentials.toml` key. A secret is written but never read back out.
#[derive(Debug)]
pub struct Credential {
    pub key: &'static str,
    pub group: &'static str,
    pub label: &'static str,
    pub secret: bool,
}

#[rustfmt::skip]
pub const CREDENTIALS: &[Credential] = &[
    Credential { key: "bandcamp.identity_cookie", group: BANDCAMP, label: "Identity cookie", secret: true },
    Credential { key: "soulseek.api_key", group: SOULSEEK, label: "slskd API key", secret: true },
    Credential { key: "soulseek.files_user", group: SOULSEEK, label: "Downloads URL user", secret: false },
    Credential { key: "soulseek.files_password", group: SOULSEEK, label: "Downloads URL password", secret: true },
];

/// What a settings screen may show about a credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialState {
    /// A value is in use, from wherever.
    pub set: bool,
    /// An environment variable or a file is used instead of what is saved.
    pub overridden: bool,
    pub note: String,
    /// The value itself, for a credential that is not a secret.
    pub value: String,
}

pub fn credential_state(creds: &Credentials, c: &Credential) -> CredentialState {
    let resolved = match c.key {
        "bandcamp.identity_cookie" => creds.bandcamp_identity(),
        "soulseek.api_key" => creds.soulseek_api_key(),
        "soulseek.files_password" => creds.soulseek_files_password(),
        _ => {
            let user = &creds.soulseek.files_user;
            return CredentialState {
                set: !user.is_empty(),
                overridden: false,
                note: String::new(),
                value: user.clone(),
            };
        }
    };
    let (set, overridden, note) = match resolved {
        Ok(None) => (false, false, "Not set.".to_string()),
        Ok(Some((_, CredentialSource::Config))) => {
            (true, false, "Saved in credentials.toml.".to_string())
        }
        Ok(Some((_, CredentialSource::Env(var)))) => (
            true,
            true,
            format!("Set by the {var} environment variable, which overrides what is saved here."),
        ),
        Ok(Some((_, CredentialSource::File(key)))) => (
            true,
            true,
            format!("Read from the file in {key}, which overrides what is saved here."),
        ),
        Err(e) => (false, true, format!("{e:#}")),
    };
    CredentialState {
        set,
        overridden,
        note,
        value: String::new(),
    }
}

/// `text` with credential `key` set to `raw`. Empty removes it.
pub fn edit_credential(text: &str, key: &str, raw: &str) -> Result<String> {
    if !CREDENTIALS.iter().any(|c| c.key == key) {
        bail!("{key} is not a credential");
    }
    let raw = raw.trim();
    let out = set_key(text, key, (!raw.is_empty()).then(|| raw.into()))?;
    toml::from_str::<Credentials>(&out).context("the edited credentials.toml does not load")?;
    Ok(out)
}

/// Replaces the file in one step, through a symlink to wherever it points.
/// A private file is readable only by its owner from the moment it exists.
pub fn write(path: &Path, text: &str, private: bool) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let dir = target
        .parent()
        .ok_or_else(|| anyhow!("{} has no folder", target.display()))?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let name = target.file_name().unwrap_or_default().to_string_lossy();
    let tmp = dir.join(format!(".{name}.tmp"));
    let _ = std::fs::remove_file(&tmp);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(if private { 0o600 } else { 0o644 })
        .open(&tmp)
        .with_context(|| format!("creating {}", tmp.display()))?;
    file.write_all(text.as_bytes())
        .with_context(|| format!("writing {}", tmp.display()))?;
    drop(file);
    if !private && let Ok(meta) = std::fs::metadata(&target) {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    std::fs::rename(&tmp, &target).with_context(|| format!("replacing {}", target.display()))
}

fn split(key: &str) -> (&str, &str) {
    key.split_once('.').unwrap_or((key, ""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_field_names_a_real_key() {
        let cfg = Config::default();
        let table = toml::Table::try_from(&cfg).unwrap();
        for f in FIELDS {
            let (section, name) = split(f.key);
            // download_dir is the one key that is absent until set.
            if f.key != "general.download_dir" {
                assert!(
                    table[section].get(name).is_some(),
                    "{} is not in Config",
                    f.key
                );
            }
            edit("", f.key, &current(&cfg, f)).unwrap_or_else(|e| panic!("{}: {e:#}", f.key));
        }
    }

    #[test]
    fn an_edit_keeps_comments_and_other_keys() {
        let text = "# mine\n[search]\nlimit = 8 # tuned\ntimeout_secs = 20\n\n[future]\nx = 1\n";
        let out = edit(text, "search.limit", "12").unwrap();
        assert_eq!(
            out,
            "# mine\n[search]\nlimit = 12 # tuned\ntimeout_secs = 20\n\n[future]\nx = 1\n"
        );
    }

    #[test]
    fn an_edit_adds_a_missing_section() {
        let out = edit("", "import.insert_content_rows", "true").unwrap();
        let cfg: Config = toml::from_str(&out).unwrap();
        assert!(cfg.import.insert_content_rows);
    }

    #[test]
    fn empty_removes_the_key() {
        let out = edit(
            "[general]\ndownload_dir = \"~/x\"\n",
            "general.download_dir",
            " ",
        )
        .unwrap();
        let cfg: Config = toml::from_str(&out).unwrap();
        assert!(cfg.general.download_dir.is_none());
    }

    #[test]
    fn values_out_of_range_or_unreadable_are_refused() {
        assert!(
            edit("", "soulseek.search_window_secs", "4").is_err(),
            "slskd's minimum is 5"
        );
        assert!(edit("", "search.limit", "eight").is_err());
        assert!(edit("", "fingerprint.coverage_min", "1.5").is_err());
        assert!(edit("", "general.format_preference", "flac, vorbis").is_err());
        assert!(edit("", "nope.key", "1").is_err());
    }

    #[test]
    fn both_cookie_sources_are_refused() {
        let text = edit("", "soundcloud.cookies_file", "~/cookies.txt").unwrap();
        assert!(edit(&text, "soundcloud.cookies_from_browser", "firefox").is_err());
    }

    #[test]
    fn lists_are_comma_separated() {
        let out = edit("", "general.format_preference", "flac, aiff ,,mp3-320").unwrap();
        let cfg: Config = toml::from_str(&out).unwrap();
        assert_eq!(cfg.general.format_preference, ["flac", "aiff", "mp3-320"]);
        assert_eq!(
            current(&cfg, field_for("general.format_preference").unwrap()),
            "flac, aiff, mp3-320"
        );
    }

    #[test]
    fn float_values_read_back_as_typed() {
        let cfg = Config::default();
        assert_eq!(
            current(&cfg, field_for("fingerprint.coverage_min").unwrap()),
            "0.8"
        );
    }

    #[test]
    fn a_credential_edit_keeps_the_rest_and_empty_clears_it() {
        let text = "# keys\n[soulseek]\napi_key = \"old\"\nfiles_user = \"me\"\n";
        let out = edit_credential(text, "soulseek.api_key", " new\n").unwrap();
        assert_eq!(
            out,
            "# keys\n[soulseek]\napi_key = \"new\"\nfiles_user = \"me\"\n"
        );
        let out = edit_credential(&out, "soulseek.api_key", "").unwrap();
        assert_eq!(out, "# keys\n[soulseek]\nfiles_user = \"me\"\n");
        assert!(
            edit_credential("", "soulseek.url", "x").is_err(),
            "not a credential"
        );
    }

    #[test]
    fn a_secret_is_never_handed_back() {
        let text = edit_credential("", "soulseek.files_password", "hunter2").unwrap();
        let creds: Credentials = toml::from_str(&text).unwrap();
        for c in CREDENTIALS {
            let state = credential_state(&creds, c);
            assert!(
                !format!("{state:?}").contains("hunter2"),
                "{}: {state:?}",
                c.key
            );
        }
        if std::env::var("SLSKD_FILES_PASSWORD").is_err() {
            let pw = CREDENTIALS
                .iter()
                .find(|c| c.key == "soulseek.files_password")
                .unwrap();
            let state = credential_state(&creds, pw);
            assert!(state.set && !state.overridden, "{state:?}");
        }
    }

    #[test]
    fn a_private_file_is_created_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("rr-creds-{}", uuid::Uuid::new_v4()));
        let path = dir.join("credentials.toml");
        write(&path, "", true).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_replaces_through_a_symlink() {
        let dir = std::env::temp_dir().join(format!("rr-settings-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let real = dir.join("real.toml");
        std::fs::write(&real, "").unwrap();
        let link = dir.join("config.toml");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        write(&link, "[search]\nlimit = 3\n", false).unwrap();
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_to_string(&real).unwrap(),
            "[search]\nlimit = 3\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
