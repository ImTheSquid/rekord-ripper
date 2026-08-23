//! The shared HTTP client.
//!
//! Blocking by design — `ureq`, no tokio, matching the rest of the crate. Every
//! agent carries a global timeout, because `thread::scope` joins its threads and
//! cannot abandon one that hangs.

use std::time::Duration;

use ureq::Agent;

use super::error::BackendError;
use super::types::BackendId;

/// Identify ourselves honestly. An unidentified scraper hammering an
/// undocumented endpoint is how an IP gets blocked.
pub const USER_AGENT: &str = concat!(
    "rekord-ripper/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/ImTheSquid/rekord-ripper)"
);

/// A per-request ceiling, so one slow response cannot eat a whole backend's
/// budget while several requests are still queued behind it.
const PER_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// Build an agent bounded by `budget`.
///
/// `timeout_global` covers the whole request including body transfer, which is
/// the only bound that actually prevents a hang — a connect timeout alone does
/// not stop a server that accepts and then dribbles.
pub fn agent(budget: Duration) -> Agent {
    let per_request = budget.min(PER_REQUEST_TIMEOUT);
    Agent::config_builder()
        .timeout_global(Some(per_request))
        .user_agent(USER_AGENT)
        .build()
        .into()
}

/// A longer-lived agent for downloads, where a multi-minute transfer is normal
/// and a 15-second ceiling would be wrong.
pub fn download_agent(budget: Duration) -> Agent {
    Agent::config_builder()
        .timeout_global(Some(budget))
        .user_agent(USER_AGENT)
        .build()
        .into()
}

/// Map a `ureq` failure onto our taxonomy, preserving the distinction between
/// "try again" and "stop".
pub fn map_err(backend: BackendId, url: &str, e: ureq::Error) -> BackendError {
    match e {
        ureq::Error::StatusCode(status) => {
            if status == 429 {
                BackendError::RateLimited {
                    backend,
                    retry_after: None,
                }
            } else {
                BackendError::Http {
                    backend,
                    status,
                    url: url.to_string(),
                }
            }
        }
        ureq::Error::Timeout(_) => BackendError::Timeout {
            backend,
            op: "http",
            elapsed: PER_REQUEST_TIMEOUT,
        },
        other => BackendError::Network {
            backend,
            detail: other.to_string(),
        },
    }
}

/// Retry `op` while it fails retryably, with exponential backoff.
///
/// Bounded and centralised deliberately: three backends rolling their own
/// backoff loops is three subtly different ones.
pub fn with_retries<T>(
    attempts: u32,
    mut op: impl FnMut() -> Result<T, BackendError>,
) -> Result<T, BackendError> {
    let mut delay = Duration::from_millis(500);
    let mut last = None;
    for attempt in 0..attempts.max(1) {
        match op() {
            Ok(v) => return Ok(v),
            Err(e) if e.is_retryable() && attempt + 1 < attempts => {
                // Honour a server-supplied wait over our own guess.
                std::thread::sleep(e.retry_after().unwrap_or(delay));
                delay = (delay * 2).min(Duration::from_secs(8));
                last = Some(e);
            }
            Err(e) => return Err(e),
        }
    }
    Err(last.expect("loop ran at least once and did not return"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn user_agent_identifies_the_tool_and_version() {
        assert!(USER_AGENT.starts_with("rekord-ripper/"));
        assert!(USER_AGENT.contains(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn agents_build_with_a_bounded_timeout() {
        let _ = agent(Duration::from_secs(10));
        let _ = download_agent(Duration::from_secs(600));
    }

    #[test]
    fn too_many_requests_becomes_rate_limited_not_a_bare_http_error() {
        // 429 has to be distinguishable so the retry loop can back off instead of
        // reporting a hard failure.
        let e = map_err(BackendId::Bandcamp, "u", ureq::Error::StatusCode(429));
        assert!(matches!(e, BackendError::RateLimited { .. }));
        assert!(e.is_retryable());
    }

    #[test]
    fn other_statuses_keep_their_code_and_url() {
        let e = map_err(BackendId::Bandcamp, "https://x/y", ureq::Error::StatusCode(404));
        match e {
            BackendError::Http { status, url, .. } => {
                assert_eq!(status, 404);
                assert_eq!(url, "https://x/y");
            }
            other => panic!("expected Http, got {other:?}"),
        }
    }

    #[test]
    fn retries_a_transient_failure_then_succeeds() {
        let calls = Cell::new(0);
        let got = with_retries(3, || {
            calls.set(calls.get() + 1);
            if calls.get() < 3 {
                Err(BackendError::Network {
                    backend: BackendId::Bandcamp,
                    detail: "flaky".into(),
                })
            } else {
                Ok(7)
            }
        })
        .unwrap();
        assert_eq!(got, 7);
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn does_not_retry_a_permanent_failure() {
        let calls = Cell::new(0);
        let err = with_retries(5, || {
            calls.set(calls.get() + 1);
            Err::<(), _>(BackendError::parse(BackendId::Bandcamp, "pagedata", "gone"))
        })
        .unwrap_err();
        assert!(matches!(err, BackendError::Parse { .. }));
        assert_eq!(calls.get(), 1, "a changed page shape must not be retried");
    }

    #[test]
    fn gives_up_after_the_attempt_budget() {
        let calls = Cell::new(0);
        let err = with_retries(3, || {
            calls.set(calls.get() + 1);
            Err::<(), _>(BackendError::Network {
                backend: BackendId::Bandcamp,
                detail: "down".into(),
            })
        })
        .unwrap_err();
        assert!(matches!(err, BackendError::Network { .. }));
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn a_zero_attempt_budget_still_runs_once() {
        let calls = Cell::new(0);
        let _ = with_retries(0, || {
            calls.set(calls.get() + 1);
            Ok::<_, BackendError>(())
        });
        assert_eq!(calls.get(), 1);
    }
}
