//! Every filesystem location this tool reads or writes, resolved in one place.
//!
//! Two roots: rekordbox's own app dir (which we only ever read from, plus the
//! ANLZ copies `apply_plan` makes under `share/`), and our own data dir, which
//! is where backups, config, and scratch live. Both come from raw env vars —
//! same style as the rest of the crate, and it keeps the dependency list short.

use std::env::var;
use std::path::PathBuf;

use anyhow::{Result, anyhow};

/// Rekordbox's application directory, containing `master.db` and `share/`.
pub fn rekordbox_app_dir() -> Result<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        var("APPDATA")
            .map(|appdata| PathBuf::from(appdata).join("Pioneer/rekordbox"))
            .map_err(|_| anyhow!("APPDATA env var not found"))
    }
    #[cfg(target_os = "macos")]
    {
        var("HOME")
            .map(|home| PathBuf::from(home).join("Library/Pioneer/rekordbox"))
            .map_err(|_| anyhow!("HOME env var not found"))
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    compile_error!("Rekordbox only runs on macOS and Windows.");
}

/// Our own data directory. Not created here — each caller creates what it needs.
///
/// Windows uses `LOCALAPPDATA` rather than roaming `APPDATA` deliberately: this
/// holds machine-local backups and a full-account Bandcamp credential, neither
/// of which should follow a user between machines.
pub fn data_dir() -> Result<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        var("LOCALAPPDATA")
            .map(|d| PathBuf::from(d).join("rekord-ripper"))
            .map_err(|_| anyhow!("LOCALAPPDATA env var not found"))
    }
    #[cfg(target_os = "macos")]
    {
        var("HOME")
            .map(|h| PathBuf::from(h).join("Library/Application Support/rekord-ripper"))
            .map_err(|_| anyhow!("HOME env var not found"))
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    compile_error!("Rekordbox only runs on macOS and Windows.");
}

/// Timestamped copies of `master.db`, written before every mutating run.
pub fn backup_dir() -> Result<PathBuf> {
    Ok(data_dir()?.join("backups"))
}

/// `config.toml`, honouring `--config` then `REKORD_RIPPER_CONFIG` then the default.
///
/// Mirrors the override precedence `resolve_key` already uses for `REKORDBOX_KEY`.
pub fn config_path(override_path: Option<&std::path::Path>) -> Result<PathBuf> {
    if let Some(p) = override_path {
        return Ok(p.to_path_buf());
    }
    if let Ok(p) = var("REKORD_RIPPER_CONFIG") {
        let p = p.trim();
        if !p.is_empty() {
            return Ok(PathBuf::from(p));
        }
    }
    Ok(data_dir()?.join("config.toml"))
}

/// The Bandcamp identity cookie, kept out of `config.toml` so the config can be
/// shared or pasted without leaking a credential.
pub fn credentials_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("credentials.toml"))
}

/// Where acquired files land by default.
///
/// One stable directory, not one per track: rekordbox has no auto-import, so the
/// user drags this folder in by hand and that should cost one drag per batch.
pub fn default_download_dir() -> Result<PathBuf> {
    Ok(data_dir()?.join("incoming"))
}

/// Parent of the per-run scratch directories used for throwaway rips.
///
/// Deliberately not `/tmp`: macOS purges it mid-run, and a predictable path
/// matters when an error message has to name the file.
pub fn scratch_root() -> Result<PathBuf> {
    Ok(data_dir()?.join("scratch"))
}

/// Pending old→new pairings awaiting a rekordbox import.
pub fn pending_db_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("pending.sqlite"))
}

/// Cached fingerprints, keyed on path + size + mtime + window + preset.
pub fn fingerprint_cache_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("fpcache.sqlite"))
}

/// Expand a leading `~/` only. No shell globbing, no `$VAR` interpolation —
/// a config value should not depend on the shell that happened to write it.
pub fn expand_tilde(s: &str) -> Result<PathBuf> {
    match s.strip_prefix("~/") {
        Some(rest) => {
            let home = var("HOME")
                .or_else(|_| var("USERPROFILE"))
                .map_err(|_| anyhow!("cannot expand '~' — neither HOME nor USERPROFILE is set"))?;
            Ok(PathBuf::from(home).join(rest))
        }
        None => Ok(PathBuf::from(s)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_dir_is_under_data_dir() {
        let (data, backups) = (data_dir().unwrap(), backup_dir().unwrap());
        assert!(backups.starts_with(&data));
        assert_eq!(backups.file_name().unwrap(), "backups");
    }

    #[test]
    fn config_path_prefers_explicit_override() {
        let explicit = PathBuf::from("/tmp/somewhere/else.toml");
        assert_eq!(config_path(Some(&explicit)).unwrap(), explicit);
    }

    #[test]
    fn expand_tilde_leaves_absolute_and_relative_paths_alone() {
        assert_eq!(expand_tilde("/abs/path").unwrap(), PathBuf::from("/abs/path"));
        assert_eq!(expand_tilde("rel/path").unwrap(), PathBuf::from("rel/path"));
        // A bare "~" is not the prefix we expand, and must not be mangled.
        assert_eq!(expand_tilde("~").unwrap(), PathBuf::from("~"));
    }

    #[test]
    fn expand_tilde_expands_home_prefix() {
        let home = var("HOME").or_else(|_| var("USERPROFILE")).unwrap();
        assert_eq!(
            expand_tilde("~/Music/x").unwrap(),
            PathBuf::from(home).join("Music/x")
        );
    }
}
