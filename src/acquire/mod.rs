//! Pluggable acquisition backends: search several music sources at once, compare
//! the offers, buy or rip, and hand the file to the analysis-copy pipeline.

pub mod backend;
pub mod error;
pub mod http;
pub mod report;
pub mod types;

pub use backend::AcquisitionBackend;
pub use error::{BackendError, Result};
pub use types::*;

use crate::config::{Config, Credentials};

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
    pub fn from_config(_cfg: &Config, _creds: &Credentials) -> Self {
        // Backends are registered here as they land.
        Self {
            backends: Vec::new(),
        }
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
            Ok(f) => eprintln!(
                "warning: format_preference lists {f}, which rekordbox cannot read — ignoring"
            ),
            Err(e) => eprintln!("warning: {e} in format_preference — ignoring"),
        }
    }
    if out.is_empty() {
        anyhow::bail!(
            "format_preference has no formats rekordbox can read; \
             expected some of flac, aiff, wav, alac, mp3-320"
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_registry_is_reported_as_empty() {
        let reg = Registry::from_config(&Config::default(), &Credentials::default());
        assert!(reg.is_empty());
        assert_eq!(reg.searchable().count(), 0);
        assert!(reg.get(BackendId::Bandcamp).is_none());
        assert!(reg.claim_url("https://soundcloud.com/a/b").is_none());
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
        cfg.general.format_preference =
            vec!["vorbis".into(), "not-a-format".into(), "flac".into()];
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
