//! `config.toml` and `credentials.toml` read again whenever either changes.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde::de::DeserializeOwned;

use super::Session;
use crate::config::{Config, Credentials};
use crate::settings::{self, Credential, CredentialState, Field};

/// When a file was last written and how long it was, or `None` if absent.
type Stamp = Option<(SystemTime, u64)>;

fn stamp(path: &Path) -> Stamp {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// Notices edits to the two files, checked at most once a second.
pub struct ConfigWatch {
    pub config: PathBuf,
    credentials: Option<PathBuf>,
    stamps: [Stamp; 2],
    checked: Instant,
}

impl ConfigWatch {
    pub fn new(config: PathBuf, credentials: Option<PathBuf>) -> Self {
        let mut watch = Self {
            config,
            credentials,
            stamps: [None, None],
            checked: Instant::now(),
        };
        watch.stamps = watch.read_stamps();
        watch
    }

    fn read_stamps(&self) -> [Stamp; 2] {
        [
            stamp(&self.config),
            self.credentials.as_deref().and_then(stamp),
        ]
    }

    /// True when either file changed since the last check.
    fn changed(&mut self) -> bool {
        if self.checked.elapsed() < Duration::from_secs(1) {
            return false;
        }
        self.checked = Instant::now();
        let now = self.read_stamps();
        std::mem::replace(&mut self.stamps, now) != now
    }

    fn mark_read(&mut self) {
        self.stamps = self.read_stamps();
    }

    /// Both files, or a one-line reason one would not load.
    fn load(&self) -> Result<(Config, Credentials), String> {
        let cfg = read(&self.config, "config.toml")?;
        let creds = match &self.credentials {
            Some(p) => read(p, "credentials.toml")?,
            None => Credentials::default(),
        };
        Ok((cfg, creds))
    }
}

/// Defaults for an absent file. Silent, unlike `Config::load`: a terminal
/// frontend has nowhere to print a warning.
fn read<T: DeserializeOwned + Default>(path: &Path, name: &str) -> Result<T, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(T::default()),
        Err(e) => return Err(format!("{name}: {e}")),
    };
    toml::from_str(&text).map_err(|e| {
        let line = e
            .span()
            .map_or(1, |s| text[..s.start].matches('\n').count() + 1);
        format!("{name} line {line}: {}", e.message().trim())
    })
}

impl Session {
    /// Reloads the settings if either file changed.
    pub(super) fn poll_config(&mut self) {
        if self.config_watch.changed() {
            self.reload_config(true);
        }
    }

    /// Takes the files as they are now. A file that will not load leaves the
    /// previous settings in place.
    fn reload_config(&mut self, announce: bool) {
        self.config_watch.mark_read();
        let (cfg, creds) = match self.config_watch.load() {
            Ok(both) => both,
            Err(why) => {
                self.status
                    .err(format!("{why} — keeping the previous settings."));
                return;
            }
        };
        if let Some(worker) = &self.worker {
            worker.reconfigure(cfg.clone(), creds.clone());
        }
        self.creds = creds;
        crate::proc::use_tools(&cfg.tools);
        let unknown: Vec<_> = cfg.unknown.keys().cloned().collect();
        self.cfg = cfg;
        self.config_revision += 1;
        if announce && unknown.is_empty() {
            self.status.info("Reloaded settings.");
        } else if announce {
            self.status.warn(format!(
                "Reloaded settings, ignoring unrecognised {}.",
                unknown.join(", ")
            ));
        }
    }

    pub fn config_path(&self) -> &Path {
        &self.config_watch.config
    }

    /// Every editable setting with its current value.
    pub fn settings(&self) -> impl Iterator<Item = (&'static Field, String)> + '_ {
        settings::FIELDS
            .iter()
            .map(|f| (f, settings::current(&self.cfg, f)))
    }

    /// Writes one setting to `config.toml` and applies it.
    pub fn set_setting(&mut self, key: &str, raw: &str) -> anyhow::Result<()> {
        let path = self.config_watch.config.clone();
        let text = match std::fs::read_to_string(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            read => read?,
        };
        let edited = settings::edit(&text, key, raw)?;
        if edited != text {
            settings::write(&path, &edited, false)?;
            self.reload_config(false);
        }
        Ok(())
    }

    /// Every editable credential, described without its value if secret.
    pub fn credentials(&self) -> impl Iterator<Item = (&'static Credential, CredentialState)> + '_ {
        settings::CREDENTIALS
            .iter()
            .map(|c| (c, settings::credential_state(&self.creds, c)))
    }

    /// Writes one credential to `credentials.toml` and applies it. Empty
    /// removes it.
    pub fn set_credential(&mut self, key: &str, raw: &str) -> anyhow::Result<()> {
        let path = self
            .config_watch
            .credentials
            .clone()
            .ok_or_else(|| anyhow::anyhow!("there is no credentials.toml"))?;
        let text = match std::fs::read_to_string(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            read => read?,
        };
        let edited = settings::edit_credential(&text, key, raw)?;
        if edited != text {
            settings::write(&path, &edited, true)?;
            self.reload_config(false);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rr-watch-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn an_edit_is_seen_once() {
        let dir = scratch();
        let path = dir.join("config.toml");
        let mut watch = ConfigWatch::new(path.clone(), None);
        watch.checked -= Duration::from_secs(2);
        assert!(!watch.changed(), "nothing written yet");

        std::fs::write(&path, "[search]\nlimit = 3\n").unwrap();
        watch.checked -= Duration::from_secs(2);
        assert!(watch.changed());
        assert_eq!(watch.load().unwrap().0.search.limit, 3);
        watch.checked -= Duration::from_secs(2);
        assert!(!watch.changed());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_broken_file_reports_one_line_with_its_line_number() {
        let dir = scratch();
        let path = dir.join("config.toml");
        std::fs::write(&path, "[search]\nlimit = \n").unwrap();
        let why = ConfigWatch::new(path, None).load().unwrap_err();
        assert!(why.starts_with("config.toml line 2: "), "{why}");
        assert!(!why.contains('\n'), "{why}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_files_load_as_defaults() {
        let dir = scratch();
        let watch = ConfigWatch::new(dir.join("config.toml"), Some(dir.join("credentials.toml")));
        let (cfg, _) = watch.load().unwrap();
        assert_eq!(cfg.search.limit, Config::default().search.limit);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
