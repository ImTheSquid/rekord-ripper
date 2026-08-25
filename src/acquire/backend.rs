//! The `AcquisitionBackend` trait.

use super::error::{BackendError, Result};
use super::types::*;

/// A place music can be found and acquired from: a store, a rip target, or both.
///
/// # Contract
///
/// Backends are called concurrently — `search_all` fans out with
/// `std::thread::scope`, one thread per backend — hence `Send + Sync` and `&self`
/// everywhere. Cache with `OnceLock`/`Mutex` rather than `&mut self`, so the
/// registry can hand out `&dyn AcquisitionBackend`.
///
/// **Every method that does I/O must bound itself.** `thread::scope` joins all of
/// its threads and cannot abandon one, so a backend that hangs hangs the whole
/// fan-out. Use `http::agent(deadline)` for requests and
/// `proc::run_with_deadline` for child processes.
///
/// Unimplemented capabilities return `Unsupported` via the default bodies. Never
/// panic, and never return empty success to mean "can't".
pub trait AcquisitionBackend: Send + Sync {
    /// Stable identity. Must match the `ItemRef::backend` of every offer emitted.
    fn id(&self) -> BackendId;

    /// What this backend can do. Lets the orchestrator skip backends that cannot
    /// serve a request without paying for a round-trip to find out.
    fn capabilities(&self) -> Capabilities;

    /// Offline credential check. Must **not** make a network call — this renders
    /// a status table. Liveness surfaces later as `AuthExpired`.
    fn credentials(&self) -> CredentialState {
        CredentialState::NotRequired
    }

    /// Claim a URL as ours, converting it to an `ItemRef`. Used by `rip <URL>`
    /// and by rekordbox streaming rows, whose `FolderPath` holds a service URI.
    /// Returns `None` if the URL isn't ours. Must not do I/O.
    fn claim_url(&self, _url: &str) -> Option<ItemRef> {
        None
    }

    /// Cheap, wide search — one request where possible.
    ///
    /// Returned offers must leave `pricing` as `Unprobed`, `formats` as `None`,
    /// and `ownership` as `Unknown` unless the backend genuinely knows them for
    /// free. Do not guess, and do not issue a request per hit; that is `enrich`'s
    /// job. Respect `query.limit`.
    fn search(&self, _query: &SearchQuery) -> Result<Vec<Offer>> {
        Err(BackendError::unsupported(self.id(), "search"))
    }

    /// Fill in pricing, formats, and ownership.
    ///
    /// Batched on purpose: Bandcamp answers ownership for an entire collection in
    /// one request, so a per-offer call would multiply that by N and earn a 429.
    ///
    /// - Every offer's `item_ref.backend` equals `self.id()`; callers guarantee it.
    /// - Must not reorder, drop, or replace elements.
    /// - Must be idempotent.
    /// - A per-offer failure goes in `Offer::enrich_error` and is **not** an
    ///   `Err`. Return `Err` only for whole-batch failures — one 404 item page
    ///   must not lose the other rows.
    /// - Leaving an offer `Unprobed` is legal; it renders as `?`.
    fn enrich(&self, _offers: &mut [Offer]) -> Result<()> {
        Ok(())
    }

    /// How the user obtains the right to download. Prefer deriving the URL from
    /// the offer over making a request.
    fn purchase(&self, _item: &ItemRef) -> Result<PurchaseFlow> {
        Err(BackendError::unsupported(self.id(), "purchase"))
    }

    /// Download the audio. One entry per file — a track yields one, an album many.
    ///
    /// Writes only inside `opts.dest_dir`, via a `.part` sibling renamed on
    /// completion, so an interrupted fetch never leaves a truncated file that
    /// looks valid. Honours `opts.format_pref` in order and returns
    /// `NoAcceptableFormat` rather than silently substituting something else.
    ///
    /// Returns `NotOwned` — not `NotFound` — when the item exists but has not
    /// been bought. The CLI's "buy it first" message depends on that difference.
    fn fetch(&self, _item: &ItemRef, _opts: &FetchOpts) -> Result<Vec<AcquiredFile>> {
        Err(BackendError::unsupported(self.id(), "fetch"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A backend that implements only the required methods, proving the defaults
    /// give a usable search-only (here: nothing-at-all) implementation.
    struct Stub;

    impl AcquisitionBackend for Stub {
        fn id(&self) -> BackendId {
            BackendId::Bandcamp
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities::default()
        }
    }

    #[test]
    fn the_trait_is_object_safe() {
        let b: Box<dyn AcquisitionBackend> = Box::new(Stub);
        assert_eq!(b.id(), BackendId::Bandcamp);
    }

    #[test]
    fn unimplemented_operations_report_unsupported_rather_than_panicking() {
        let b = Stub;
        let q = SearchQuery::from_text("x", 5);
        assert!(matches!(
            b.search(&q),
            Err(BackendError::Unsupported { op: "search", .. })
        ));
        assert!(matches!(
            b.purchase(&ItemRef::new(BackendId::Bandcamp, "t:1")),
            Err(BackendError::Unsupported { op: "purchase", .. })
        ));
    }

    #[test]
    fn enrich_defaults_to_a_no_op_not_an_error() {
        // A backend with nothing to add must not fail a whole fan-out batch.
        assert!(Stub.enrich(&mut []).is_ok());
    }

    #[test]
    fn claim_url_defaults_to_declining() {
        assert!(Stub.claim_url("https://example.com/x").is_none());
    }

    #[test]
    fn backends_are_shareable_across_threads() {
        // This is what makes the thread::scope fan-out legal.
        let b: Box<dyn AcquisitionBackend> = Box::new(Stub);
        let r = &b;
        std::thread::scope(|s| {
            let h = s.spawn(move || r.id());
            assert_eq!(h.join().unwrap(), BackendId::Bandcamp);
        });
    }
}
