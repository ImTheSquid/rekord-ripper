//! Typed errors, only at the backend boundary.
//!
//! The rest of the crate is `anyhow`, which is right for a local tool where
//! every error is print-and-exit. Acquisition is different: the caller has to
//! *branch*. "You haven't bought this yet", "your cookie expired", and "this
//! backend can't do that" are normal control flow, not failures to report, and
//! string-matching them out of an `anyhow` message is a bug factory.
//!
//! `BackendError` is `Error + Send + Sync + 'static`, so it converts into
//! `anyhow::Error` for free at the `main.rs` boundary. No taxonomy leaks into
//! `analysis`, `db`, or the TUI.

use std::time::Duration;

use super::types::{AudioFormat, BackendId, ItemRef};

pub type Result<T> = std::result::Result<T, BackendError>;

#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("{backend} does not support {op}")]
    Unsupported {
        backend: BackendId,
        op: &'static str,
    },

    #[error("{backend}: no such item: {item}")]
    NotFound { backend: BackendId, item: String },

    /// The item exists and you have not bought it. Expected, not exceptional.
    #[error("{backend}: not in your collection — buy it first ({item})")]
    NotOwned { backend: BackendId, item: ItemRef },

    /// Credentials were rejected. Bandcamp signals this as HTTP 200 with
    /// `{"error":true,"error_message":"must be logged in"}`, which is exactly
    /// why this is a distinct variant and not a parse failure.
    #[error("{backend}: session expired or rejected — {detail}")]
    AuthExpired {
        backend: BackendId,
        detail: String,
        reauth: Option<String>,
    },

    #[error("{backend}: no credentials configured — {how_to_fix}")]
    NoCredentials {
        backend: BackendId,
        how_to_fix: String,
    },

    #[error("{backend}: rate limited{}", match retry_after {
        Some(d) => format!(" — retry in {}s", d.as_secs()),
        None => String::new(),
    })]
    RateLimited {
        backend: BackendId,
        retry_after: Option<Duration>,
    },

    #[error("{backend}: timed out after {elapsed:?} during {op}")]
    Timeout {
        backend: BackendId,
        op: &'static str,
        elapsed: Duration,
    },

    #[error("{backend}: HTTP {status} from {url}")]
    Http {
        backend: BackendId,
        status: u16,
        url: String,
    },

    #[error("{backend}: network error — {detail}")]
    Network { backend: BackendId, detail: String },

    /// The site changed shape. Distinct from `Network` so the message can say
    /// "Bandcamp changed their page format" rather than blaming the connection.
    /// Both the search endpoint and the pagedata blob are undocumented, so this
    /// will happen eventually; it must not be retried.
    #[error("{backend}: unexpected response shape at {at} — {detail}")]
    Parse {
        backend: BackendId,
        at: &'static str,
        detail: String,
    },

    #[error("none of the available formats {available:?} match your preference {wanted:?}")]
    NoAcceptableFormat {
        available: Vec<AudioFormat>,
        wanted: Vec<AudioFormat>,
    },

    #[error("{tool} not found — install it, or set its path in config.toml")]
    ToolMissing { tool: String },

    #[error("{tool} failed: {detail}")]
    ToolFailed { tool: String, detail: String },

    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// Anything local that went wrong on our side of the boundary.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl BackendError {
    pub fn unsupported(backend: BackendId, op: &'static str) -> Self {
        Self::Unsupported { backend, op }
    }

    pub fn parse(backend: BackendId, at: &'static str, detail: impl Into<String>) -> Self {
        Self::Parse {
            backend,
            at,
            detail: detail.into(),
        }
    }

    /// Which backend produced this, when one did.
    pub fn backend(&self) -> Option<BackendId> {
        match self {
            Self::Unsupported { backend, .. }
            | Self::NotFound { backend, .. }
            | Self::NotOwned { backend, .. }
            | Self::AuthExpired { backend, .. }
            | Self::NoCredentials { backend, .. }
            | Self::RateLimited { backend, .. }
            | Self::Timeout { backend, .. }
            | Self::Http { backend, .. }
            | Self::Network { backend, .. }
            | Self::Parse { backend, .. } => Some(*backend),
            _ => None,
        }
    }

    /// Worth trying again after a backoff. Deliberately excludes `Parse`: a
    /// changed page format will be just as changed on the retry.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::RateLimited { .. } | Self::Timeout { .. } | Self::Network { .. } => true,
            Self::Http { status, .. } => *status == 429 || *status >= 500,
            _ => false,
        }
    }

    /// The user must do something; retrying cannot help.
    pub fn needs_user_action(&self) -> bool {
        matches!(
            self,
            Self::NotOwned { .. }
                | Self::AuthExpired { .. }
                | Self::NoCredentials { .. }
                | Self::ToolMissing { .. }
                | Self::NoAcceptableFormat { .. }
        )
    }

    /// A backend that can't do something isn't a failure worth printing during
    /// a fan-out — it's a row we simply don't have.
    pub fn is_silently_skippable(&self) -> bool {
        matches!(self, Self::Unsupported { .. } | Self::NotFound { .. })
    }

    /// How long to wait before a retry, when the backend told us.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::RateLimited { retry_after, .. } => *retry_after,
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_failures_are_retryable_and_permanent_ones_are_not() {
        let bc = BackendId::Bandcamp;
        assert!(
            BackendError::RateLimited {
                backend: bc,
                retry_after: None
            }
            .is_retryable()
        );
        assert!(
            BackendError::Http {
                backend: bc,
                status: 503,
                url: "u".into()
            }
            .is_retryable()
        );
        assert!(
            BackendError::Http {
                backend: bc,
                status: 429,
                url: "u".into()
            }
            .is_retryable()
        );
        // A 404 and a changed page shape will both be identical next time.
        assert!(
            !BackendError::Http {
                backend: bc,
                status: 404,
                url: "u".into()
            }
            .is_retryable()
        );
        assert!(!BackendError::parse(bc, "pagedata", "no blob").is_retryable());
    }

    #[test]
    fn user_actionable_failures_are_flagged() {
        let bc = BackendId::Bandcamp;
        assert!(
            BackendError::AuthExpired {
                backend: bc,
                detail: "must be logged in".into(),
                reauth: None
            }
            .needs_user_action()
        );
        assert!(
            BackendError::NotOwned {
                backend: bc,
                item: ItemRef::new(bc, "t:1")
            }
            .needs_user_action()
        );
        assert!(
            !BackendError::Network {
                backend: bc,
                detail: "dns".into()
            }
            .needs_user_action()
        );
    }

    #[test]
    fn unsupported_is_skipped_quietly_in_a_fan_out() {
        assert!(BackendError::unsupported(BackendId::SoundCloud, "enrich").is_silently_skippable());
        assert!(
            !BackendError::AuthExpired {
                backend: BackendId::Bandcamp,
                detail: "x".into(),
                reauth: None
            }
            .is_silently_skippable()
        );
    }

    #[test]
    fn backend_is_recoverable_from_the_error() {
        assert_eq!(
            BackendError::unsupported(BackendId::SoundCloud, "purchase").backend(),
            Some(BackendId::SoundCloud)
        );
        assert_eq!(
            BackendError::NoAcceptableFormat {
                available: vec![],
                wanted: vec![]
            }
            .backend(),
            None
        );
    }

    #[test]
    fn converts_into_anyhow_so_main_needs_no_special_casing() {
        let e: anyhow::Error = BackendError::unsupported(BackendId::Bandcamp, "search").into();
        assert!(e.to_string().contains("does not support search"));
    }

    #[test]
    fn rate_limit_message_mentions_the_wait_when_known() {
        let e = BackendError::RateLimited {
            backend: BackendId::Bandcamp,
            retry_after: Some(Duration::from_secs(30)),
        };
        assert!(e.to_string().contains("30s"), "got: {e}");
        assert_eq!(e.retry_after(), Some(Duration::from_secs(30)));
    }
}
