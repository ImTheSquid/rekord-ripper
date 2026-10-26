//! Searching the backends, and downloading what they offer.

use std::collections::{HashSet, VecDeque};
use std::time::Instant;

use crate::acquire::ItemRef;
use crate::acquire::shop::{GroupOutcome, QuerySpec, RankedOffer};
use crate::library::TrackRow;

use super::worker;
use super::{NoticeAction, Session, StatusLevel};

/// How far along a track is in the shop's track list.
///
/// Shown per row so a queue of searches is legible: which are answered, which
/// are still waiting, and how much each one found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShopTrackState {
    Untouched,
    Queued,
    Done(usize),
}

/// Where the shop has got to.
///
/// The whole point of the worker is that `Searching` is a real state the UI can
/// render, rather than a frozen screen.
pub enum ShopState {
    Idle,
    Searching {
        since: Instant,
        what: String,
        /// Progress through a bulk search. `(0, 1)` for a single search.
        done: usize,
        total: usize,
        /// What was asked for, so the results can be reopened rather than
        /// re-searched when you come back to them.
        specs: Vec<QuerySpec>,
    },
    Results {
        groups: Box<Vec<GroupOutcome>>,
        /// Index into the flattened offer list across all groups: the offer
        /// worth looking at, moved to each newly arrived block.
        cursor: usize,
        /// The specs these results answer, used to decide reopen vs re-search.
        specs: Vec<QuerySpec>,
    },
    Failed(String),
}

/// A download in flight, or its result.
///
/// Kept separate from `ShopState` so the offer table stays on screen while a
/// fetch runs — you can see what you picked.
#[derive(Default)]
pub enum FetchState {
    #[default]
    Idle,
    Running {
        since: Instant,
        what: String,
        /// The backend's last word on it — a queue position, a percentage. None
        /// until it says something.
        note: Option<String>,
    },
    Done {
        paths: Vec<std::path::PathBuf>,
        queued: Option<i64>,
    },
    Failed(String),
}

/// What an offer's download is doing, for the marker in its row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OfferFetch {
    /// Never asked for, or asked for and failed — either way, fetching it again
    /// will try.
    None,
    Queued,
    Running,
    Done,
}

/// A download handed to the worker: in flight if it is at the front, else
/// waiting its turn.
#[derive(Debug, PartialEq, Eq)]
struct QueuedFetch {
    /// `artist — title`, for the status line.
    what: String,
    /// The source track to pair the finished file with, if any.
    src: Option<String>,
    /// So the same offer twice is refused rather than downloaded twice.
    item: ItemRef,
}

/// The downloads handed to the worker, oldest first. The front one is running.
///
/// A queue rather than a slot because the worker already runs jobs one at a time
/// in submission order — several fetches need somewhere to keep each one's label
/// and source track, not a scheduler. Per-download `src` is the part that has to
/// be a queue: a single shared slot pairs every finished file with the *last*
/// download started rather than its own.
#[derive(Default)]
pub struct FetchQueue(VecDeque<QueuedFetch>);

impl FetchQueue {
    fn push(&mut self, q: QueuedFetch) {
        self.0.push_back(q);
    }

    /// A dead worker answers none of them, not just the one in flight.
    pub(super) fn abandon_all(&mut self) {
        self.0.clear();
    }

    /// Downloads ahead of `item`, or `None` when it is not queued at all.
    fn ahead_of(&self, item: &ItemRef) -> Option<usize> {
        self.0.iter().position(|q| &q.item == item)
    }

    /// Retire the finished front and hand it back.
    ///
    /// Front, not a search by id: the worker answers in submission order, and
    /// `Fetched` carries no way to tell which download it belongs to.
    fn finish_front(&mut self) -> Option<QueuedFetch> {
        self.0.pop_front()
    }

    fn label(&self) -> Option<&str> {
        self.0.front().map(|q| q.what.as_str())
    }

    /// Every download, oldest first, paired with whether it is the running one.
    pub fn iter(&self) -> impl Iterator<Item = (&str, bool)> {
        self.0
            .iter()
            .enumerate()
            .map(|(i, q)| (q.what.as_str(), i == 0))
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl ShopState {
    /// Total offers across every group.
    pub fn len(&self) -> usize {
        match self {
            Self::Results { groups, .. } => groups.iter().map(|g| g.outcome.offers.len()).sum(),
            _ => 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn move_cursor(&mut self, delta: isize) {
        let n = self.len();
        if let Self::Results { cursor, .. } = self {
            if n == 0 {
                *cursor = 0;
                return;
            }
            *cursor = (*cursor as isize + delta).clamp(0, n as isize - 1) as usize;
        }
    }

    /// Walk the flattened offer list, yielding `(group, offer)` in display order.
    pub fn flattened(&self) -> impl Iterator<Item = (&GroupOutcome, &RankedOffer)> {
        let groups: &[GroupOutcome] = match self {
            Self::Results { groups, .. } => groups,
            _ => &[],
        };
        groups
            .iter()
            .flat_map(|g| g.outcome.offers.iter().map(move |o| (g, o)))
    }

    pub fn selected(&self) -> Option<&RankedOffer> {
        self.selected_with_group().map(|(_, o)| o)
    }

    /// The highlighted offer and the group it belongs to.
    ///
    /// The group carries the source track, which is what a bulk search needs in
    /// order to pair a download with the right local track.
    pub fn selected_with_group(&self) -> Option<(&GroupOutcome, &RankedOffer)> {
        let cursor = match self {
            Self::Results { cursor, .. } => *cursor,
            _ => return None,
        };
        self.flattened().nth(cursor)
    }

    /// The offer `item` as found for `src_id`.
    ///
    /// Both halves, because one offer can answer two tracks' searches, and which
    /// track a download is paired with depends on which of them was picked.
    pub fn find(&self, src_id: Option<&str>, item: &ItemRef) -> Option<(&GroupOutcome, &RankedOffer)> {
        self.flattened()
            .find(|(g, o)| g.src_id.as_deref() == src_id && &o.offer.item_ref == item)
    }

    /// The specs these results answer, if any.
    pub fn specs(&self) -> Option<&[QuerySpec]> {
        match self {
            Self::Searching { specs, .. } | Self::Results { specs, .. } => Some(specs),
            _ => None,
        }
    }
}

/// Basket rows still worth searching, in library order, capped, with the
/// uncapped total.
///
/// Walks every row rather than the visible ones: filtering the list after filling
/// the basket used to drop the hidden items from the search without a word, which
/// looked exactly like the basket being forgotten.
///
/// `pending` is applied *before* the cap, so a basket bigger than the cap works
/// through itself one search at a time. Capping first meant every search offered
/// the same already-queued batch and the rest were unreachable.
fn basket_rows(
    rows: &[TrackRow],
    selected: &HashSet<String>,
    cap: usize,
    pending: impl Fn(&TrackRow) -> bool,
) -> (Vec<TrackRow>, usize) {
    let all: Vec<TrackRow> = rows
        .iter()
        .filter(|r| selected.contains(&r.id) && pending(r))
        .cloned()
        .collect();
    let total = all.len();
    (all.into_iter().take(cap).collect(), total)
}

/// The per-track tag the shop list shows.
///
/// A group that came back with nothing is still *answered* — the difference
/// between "found nothing" and "not searched" is the whole reason for the tag.
fn track_state(shop: &ShopState, queued: &[String], src_id: &str) -> ShopTrackState {
    let found = shop
        .flattened()
        .filter(|(g, _)| g.src_id.as_deref() == Some(src_id))
        .count();
    if found > 0 {
        return ShopTrackState::Done(found);
    }
    if let ShopState::Results { groups, .. } = shop
        && groups.iter().any(|g| g.src_id.as_deref() == Some(src_id))
    {
        return ShopTrackState::Done(0);
    }
    if queued.iter().any(|q| q == src_id) {
        return ShopTrackState::Queued;
    }
    ShopTrackState::Untouched
}

impl Session {
    /// Build the search spec for one source row.
    fn spec_for(&self, row: &TrackRow) -> Option<QuerySpec> {
        let title = row.title.trim().to_string();
        if title.is_empty() {
            return None;
        }
        // TrackRow stores these as plain Strings, with empty meaning absent.
        let artist = Some(row.artist.trim().to_string()).filter(|a| !a.is_empty());
        Some(QuerySpec {
            label: format!("{} — {title}", artist.as_deref().unwrap_or("?")),
            src_id: Some(row.id.clone()),
            query: crate::acquire::types::SearchQuery {
                title,
                artist,
                duration_secs: row.length,
                limit: self.cfg.search.limit,
                ..Default::default()
            },
        })
    }

    /// Search for one track, or show what is already known about it.
    ///
    /// Never re-runs a finished search: that used to throw completed results
    /// away, and while one was running it refused with no way back to it at all.
    pub fn shop_track(&mut self, id: &str) -> bool {
        let Some(row) = self.row(id).cloned() else {
            self.status.warn("no track highlighted.");
            return false;
        };

        // Already answered: point the offer cursor at its block.
        if let Some(i) = self.first_offer_index_for(&row.id) {
            let found = self
                .shop
                .flattened()
                .filter(|(g, _)| g.src_id.as_deref() == Some(row.id.as_str()))
                .count();
            if let ShopState::Results { cursor, .. } = &mut self.shop {
                *cursor = i;
            }
            self.status.info(format!(
                "showing the {found} offer(s) already found for {}.",
                row.title
            ));
            self.status.act(NoticeAction::Research {
                src_id: row.id.clone(),
            });
            return true;
        }
        // Already in the queue: nothing to do but say so.
        if self.shop_queued.contains(&row.id) {
            self.status.info(format!(
                "{} is already queued — {} search(es) to go.",
                row.title,
                self.shop_outstanding()
            ));
            return true;
        }
        // Otherwise add it to the list, behind anything already running.
        self.enqueue_shop(&[row])
    }

    /// How far along a track is: answered, waiting, or untouched.
    pub fn shop_track_state(&self, src_id: &str) -> ShopTrackState {
        track_state(&self.shop, &self.shop_queued, src_id)
    }

    /// Add source tracks to the search queue, keeping any results already shown.
    ///
    /// Sequential by construction: the worker takes one job at a time, so asking
    /// for several tracks builds a list that works through itself rather than
    /// firing a burst of concurrent requests at each backend.
    pub fn enqueue_shop(&mut self, rows: &[TrackRow]) -> bool {
        let specs: Vec<_> = rows
            .iter()
            .filter(|r| self.is_pending(&r.id))
            .filter_map(|r| self.spec_for(r))
            .collect();

        if specs.is_empty() {
            self.status
                .info("nothing new to search — those are already done or queued.");
            return false;
        }
        self.submit_shop_queued(specs)
    }

    /// Worth searching: neither answered already nor waiting in the queue.
    fn is_pending(&self, src_id: &str) -> bool {
        self.first_offer_index_for(src_id).is_none()
            && !self.shop_queued.iter().any(|q| q == src_id)
    }

    /// Index into the flattened offer list of the first offer found for `src_id`.
    fn first_offer_index_for(&self, src_id: &str) -> Option<usize> {
        self.shop
            .flattened()
            .position(|(g, _)| g.src_id.as_deref() == Some(src_id))
    }

    /// Re-run one search, replacing just that track's results.
    pub fn research(&mut self, id: Option<&str>) -> bool {
        let Some(row) = id.and_then(|id| self.row(id)).cloned() else {
            self.status.warn("nothing to re-search.");
            return false;
        };
        let Some(spec) = self.spec_for(&row) else {
            self.status.warn("that track has no title to search for.");
            return false;
        };
        // Drop the stale block for this track only, so a re-search does not throw
        // away results for everything else in the list.
        self.drop_group_for(&row.id);
        self.shop_queued.retain(|id| id != &row.id);
        self.submit_shop_queued(vec![spec])
    }

    /// Remove the results block for one source track.
    fn drop_group_for(&mut self, src_id: &str) {
        if let ShopState::Results {
            groups,
            cursor,
            specs,
        } = &mut self.shop
        {
            groups.retain(|g| g.src_id.as_deref() != Some(src_id));
            specs.retain(|s| s.src_id.as_deref() != Some(src_id));
            let n: usize = groups.iter().map(|g| g.outcome.offers.len()).sum();
            *cursor = (*cursor).min(n.saturating_sub(1));
        }
    }

    /// Search for everything in the basket.
    ///
    /// A basket rather than "everything visible": a filter can match hundreds of
    /// rows, and each track is a full fan-out across every backend.
    pub fn shop_basket(&mut self, basket: &HashSet<String>) -> bool {
        if basket.is_empty() {
            self.status.warn("the basket is empty.");
            self.status.act(NoticeAction::FillBasket);
            return false;
        }
        let cap = self.cfg.search.bulk_max.max(1);
        let (rows, total) = basket_rows(&self.rows, basket, cap, |r| self.is_pending(&r.id));
        let queued = rows.len();
        let ok = self.enqueue_shop(&rows);
        // Said *after* the submit, which sets its own status — a warning written
        // before it was simply overwritten and never seen.
        if ok && total > queued {
            let text = format!("{} {} left in the basket.", self.status.text, total - queued);
            self.status.amend(StatusLevel::Warn, text);
            self.status.act(NoticeAction::NextBasketBatch { cap });
        }
        ok
    }

    /// The source track of the highlighted offer.
    pub fn shop_offer_src(&self) -> Option<&str> {
        self.shop
            .selected_with_group()
            .and_then(|(g, _)| g.src_id.as_deref())
    }

    fn submit_shop_queued(&mut self, specs: Vec<QuerySpec>) -> bool {
        let opts = crate::acquire::shop::SearchOpts {
            timeout: std::time::Duration::from_secs(self.cfg.search.timeout_secs.max(1)),
            enrich_top_n: self.cfg.search.enrich_top_n,
            ..Default::default()
        };

        let Some(worker) = self.worker.as_mut() else {
            self.status
                .err("the search thread is not running; restart the app.");
            return false;
        };
        if !worker.submit(worker::Job::Shop {
            specs: specs.clone(),
            opts: Box::new(opts),
        }) {
            self.status.err("could not start the search.");
            return false;
        }
        let outstanding = worker.outstanding();

        for spec in &specs {
            if let Some(id) = &spec.src_id {
                self.shop_queued.push(id.clone());
            }
        }
        let what = match specs.len() {
            1 => specs[0].label.clone(),
            n => format!("{n} tracks"),
        };
        self.status.info(if outstanding > 1 {
            format!("queued {what} — {outstanding} search(es) pending.")
        } else {
            format!("searching for {what} …")
        });

        // Keep existing results on screen while more arrive; only show the
        // searching pane when there is nothing to look at yet.
        if self.shop.is_empty() {
            let total = specs.len();
            self.shop = ShopState::Searching {
                since: Instant::now(),
                what,
                done: 0,
                total,
                specs,
            };
        } else if let ShopState::Results { specs: have, .. } = &mut self.shop {
            have.extend(specs);
        }
        true
    }

    /// A search answered: append its groups and say what it found.
    pub(super) fn on_searched(&mut self, new_groups: Vec<GroupOutcome>) {
        for g in new_groups.iter() {
            if let Some(id) = &g.src_id {
                self.shop_queued.retain(|q| q != id);
            }
        }
        let found: usize = new_groups.iter().map(|g| g.outcome.offers.len()).sum();
        let mut failed: Vec<String> = Vec::new();
        for g in new_groups.iter() {
            for r in g.outcome.failures() {
                if let Some(e) = &r.error {
                    let msg = format!("{}: {e}", r.backend);
                    if !failed.contains(&msg) {
                        failed.push(msg);
                    }
                }
            }
        }

        // Append rather than replace, so a queue of searches builds up one list
        // instead of each result wiping the last.
        let specs = self.shop.specs().unwrap_or(&[]).to_vec();
        let mut groups: Vec<GroupOutcome> =
            match std::mem::replace(&mut self.shop, ShopState::Idle) {
                ShopState::Results { groups, .. } => *groups,
                _ => Vec::new(),
            };
        let cursor_at = groups.iter().map(|g| g.outcome.offers.len()).sum::<usize>();
        groups.extend(new_groups);
        let total: usize = groups.iter().map(|g| g.outcome.offers.len()).sum();
        self.shop = ShopState::Results {
            groups: Box::new(groups),
            // Land on the newly arrived block, which is what you were waiting for.
            cursor: cursor_at.min(total.saturating_sub(1)),
            specs,
        };

        let pending = self.worker.as_ref().map(|w| w.outstanding()).unwrap_or(0);
        let tail = if pending > 0 {
            format!(" — {pending} still queued")
        } else {
            String::new()
        };
        match (found, failed.is_empty()) {
            (0, true) => self.status.warn(format!("no offers found{tail}.")),
            (0, false) => self
                .status
                .err(format!("no offers — {}{tail}", failed.join("; "))),
            (n, true) => self.status.ok(format!("{n} more offers{tail}.")),
            // A partial table is still useful; say what is missing.
            (n, false) => self.status.warn(format!(
                "{n} more offers, degraded — {}{tail}",
                failed.join("; ")
            )),
        }
    }

    /// The front download finished, one way or the other.
    pub(super) fn on_fetched(
        &mut self,
        result: Result<Vec<crate::acquire::types::AcquiredFile>, String>,
    ) {
        match result {
            Ok(files) => {
                let paths: Vec<std::path::PathBuf> = files.iter().map(|f| f.path.clone()).collect();
                let lossy = files.iter().any(|f| !f.format.is_lossless());
                // Queueing needs the database, so it happens here on the session's
                // thread rather than in the worker.
                let done = self.finish_front_fetch();
                if let Some(q) = &done {
                    self.fetched.insert(q.item.clone());
                }
                let queued = self.queue_transfer_for(done.and_then(|q| q.src), &paths);
                match (queued, lossy) {
                    (Some(id), _) => {
                        self.status
                            .ok(format!("downloaded and queued transfer #{id}."));
                        self.status.act(NoticeAction::ImportDownloads);
                    }
                    (None, true) => self
                        .status
                        .warn("downloaded, but it is a lossy transcode".to_string()),
                    (None, false) => self.status.ok(format!("downloaded {} file(s)", paths.len())),
                }
                // The next download owns the panel while it runs; this one's
                // outcome stays in the status line.
                if self.fetch_queue.is_empty() {
                    self.fetch = FetchState::Done { paths, queued };
                } else {
                    self.note_downloads_left();
                }
            }
            Err(why) => {
                self.finish_front_fetch();
                self.status.err(format!("download failed: {why}"));
                if self.fetch_queue.is_empty() {
                    self.fetch = FetchState::Failed(why);
                } else {
                    self.note_downloads_left();
                }
            }
        }
    }

    /// Download one offer, as found for `src_id`.
    ///
    /// Does the whole thing rather than printing a command to run: the worker
    /// makes a multi-minute download survivable, so there is no reason to hand
    /// the user homework. `fallback_src` pairs the file with a track when the
    /// offer came from a search that had none.
    pub fn start_fetch(
        &mut self,
        src_id: Option<&str>,
        item: &ItemRef,
        fallback_src: Option<String>,
    ) -> bool {
        // Take the source from the offer's own group: in a bulk search an offer
        // belongs to whichever track it was found for.
        let Some((offer, group_src)) = self
            .shop
            .find(src_id, item)
            .map(|(g, r)| (r.offer.clone(), g.src_id.clone()))
        else {
            self.status.warn("no offer selected.");
            return false;
        };
        // Something you have to pay for cannot be fetched; say what to do instead.
        if offer.requires_purchase() {
            self.status.warn(format!(
                "you don't own this yet ({}).",
                crate::acquire::render::price_cell(&offer)
            ));
            self.status.act(NoticeAction::OpenBuyPage {
                src_id: group_src,
                item: offer.item_ref.clone(),
            });
            return false;
        }

        let dest = match self.cfg.download_dir() {
            Ok(d) => d,
            Err(e) => {
                self.status.err(format!("no download directory: {e}"));
                return false;
            }
        };
        let format_pref = match crate::acquire::format_preference(&self.cfg) {
            Ok(p) => p,
            Err(e) => {
                self.status.err(e.to_string());
                return false;
            }
        };

        if let Some(n) = self.fetch_queue.ahead_of(&offer.item_ref) {
            self.status.info(match n {
                0 => "that one is downloading now.".to_string(),
                _ => format!("that one is already queued, {n} download(s) ahead of it."),
            });
            return false;
        }

        let Some(worker) = self.worker.as_mut() else {
            self.status.err("the worker thread is not running.");
            return false;
        };
        if !worker.submit(worker::Job::Fetch {
            item: offer.item_ref.clone(),
            dest,
            format_pref,
            overwrite: false,
        }) {
            self.status.err("could not start the download.");
            return false;
        }

        let what = format!("{} — {}", offer.artist, offer.title);
        // Remember the source so the transfer can be queued on completion.
        let src = group_src.or(fallback_src);
        let running = !self.fetch_queue.is_empty();
        self.fetch_queue.push(QueuedFetch {
            what: what.clone(),
            src,
            item: offer.item_ref.clone(),
        });
        if running {
            self.status.info(format!(
                "queued {what} — {} download(s) ahead of it.",
                self.fetch_queue.len() - 1
            ));
        } else {
            self.status.info(format!("downloading {what} …"));
            self.fetch = FetchState::Running {
                since: Instant::now(),
                what,
                note: None,
            };
        }
        true
    }

    /// Retire the finished download and start showing the next one.
    ///
    /// Returns the source track the finished one was for. Popping the front is
    /// what keeps the pairing right: the worker answers in submission order.
    fn finish_front_fetch(&mut self) -> Option<QueuedFetch> {
        let done = self.fetch_queue.finish_front();
        if let Some(what) = self.fetch_queue.label().map(str::to_string) {
            self.fetch = FetchState::Running {
                since: Instant::now(),
                what,
                note: None,
            };
        }
        done
    }

    /// What an offer's download is doing, for the marker in its row.
    pub fn offer_fetch(&self, item: &ItemRef) -> OfferFetch {
        match self.fetch_queue.ahead_of(item) {
            Some(0) => OfferFetch::Running,
            Some(_) => OfferFetch::Queued,
            // A failed download is deliberately left unmarked: the row has to
            // look retryable, because fetching it again is.
            None if self.fetched.contains(item) => OfferFetch::Done,
            None => OfferFetch::None,
        }
    }

    /// Open an offer's page in a browser, which is where buying happens.
    pub fn open_offer(&mut self, offer: &crate::acquire::types::Offer) {
        match crate::proc::open_url(&offer.url) {
            Ok(()) => self
                .status
                .ok(format!("opened {} in your browser.", offer.title)),
            Err(e) => self.status.err(format!("could not open a browser: {e}")),
        }
    }

    /// Append the outstanding download count to whatever the status line just
    /// said, so a finished download does not read as the whole queue finishing.
    fn note_downloads_left(&mut self) {
        let text = format!("{} {} still queued.", self.status.text, self.fetch_queue.len());
        self.status.amend(StatusLevel::Info, text);
    }

    /// Record a pending old→new pairing for a downloaded file.
    ///
    /// Returns the queued entry id. `None` when there was no source track,
    /// which is a legitimate "just download it" case.
    fn queue_transfer_for(&mut self, src_id: Option<String>, paths: &[std::path::PathBuf]) -> Option<i64> {
        let src_id = src_id?;
        let src = crate::analysis::load_track(&self.db, &src_id).ok()?;
        let store = crate::pending::PendingStore::open().ok()?;
        let first = paths.first()?;
        store
            .add(
                &src,
                first,
                None,
                // Rekordbox auto-analyses on import and leaves cues behind.
                true,
                self.copy_opts.lock,
                self.cfg.pending.ttl_days,
            )
            .ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acquire::shop::SearchOutcome;
    use crate::acquire::types::{BackendId, ItemKind, Offer, SearchQuery};

    fn spec(src_id: &str) -> QuerySpec {
        QuerySpec {
            label: format!("track {src_id}"),
            src_id: Some(src_id.to_string()),
            query: SearchQuery::from_text("x", 5),
        }
    }

    fn group(src_id: &str, offers: usize) -> GroupOutcome {
        let ranked = (0..offers)
            .map(|i| RankedOffer {
                offer: Offer::new(
                    ItemRef::new(BackendId::Bandcamp, format!("t:{src_id}:{i}")),
                    ItemKind::Track,
                    "A",
                    format!("T{i}"),
                    "https://x/y",
                ),
                row: i + 1,
                match_score: 50,
            })
            .collect();
        GroupOutcome {
            label: format!("track {src_id}"),
            src_id: Some(src_id.to_string()),
            outcome: SearchOutcome {
                offers: ranked,
                per_backend: vec![],
            },
        }
    }

    fn results(groups: Vec<GroupOutcome>, specs: Vec<QuerySpec>) -> ShopState {
        ShopState::Results {
            groups: Box::new(groups),
            cursor: 0,
            specs,
        }
    }

    #[test]
    fn completed_results_remember_what_they_were_for() {
        // The bug: nothing recorded which track the results answered, so there
        // was no way to tell "reopen these" from "search again".
        let s = results(vec![group("101", 2)], vec![spec("101")]);
        assert_eq!(s.specs().unwrap().len(), 1);
        assert_eq!(s.specs().unwrap()[0].src_id.as_deref(), Some("101"));
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn the_cursor_walks_across_groups() {
        let mut s = results(
            vec![group("101", 2), group("202", 3)],
            vec![spec("101"), spec("202")],
        );
        assert_eq!(s.len(), 5);

        // First group.
        assert_eq!(
            s.selected_with_group().unwrap().0.src_id.as_deref(),
            Some("101")
        );
        s.move_cursor(1);
        assert_eq!(
            s.selected_with_group().unwrap().0.src_id.as_deref(),
            Some("101")
        );
        // Crossing into the second group must switch which track it is for.
        s.move_cursor(1);
        assert_eq!(
            s.selected_with_group().unwrap().0.src_id.as_deref(),
            Some("202")
        );
        s.move_cursor(10);
        assert_eq!(
            s.selected_with_group().unwrap().0.src_id.as_deref(),
            Some("202")
        );
        assert_eq!(s.selected().unwrap().offer.title, "T2");
    }

    #[test]
    fn the_cursor_clamps_at_both_ends() {
        let mut s = results(vec![group("101", 2)], vec![spec("101")]);
        s.move_cursor(-5);
        assert_eq!(s.selected().unwrap().offer.title, "T0");
        s.move_cursor(99);
        assert_eq!(s.selected().unwrap().offer.title, "T1");
    }

    #[test]
    fn an_empty_group_contributes_no_rows_but_still_shows() {
        // A bulk search where one track found nothing must not break the cursor.
        let s = results(
            vec![group("101", 0), group("202", 2)],
            vec![spec("101"), spec("202")],
        );
        assert_eq!(s.len(), 2);
        assert_eq!(
            s.selected_with_group().unwrap().0.src_id.as_deref(),
            Some("202")
        );
    }

    #[test]
    fn nothing_is_selected_when_there_are_no_results() {
        assert!(ShopState::Idle.selected().is_none());
        assert!(ShopState::Failed("x".into()).selected().is_none());
        let empty = results(vec![], vec![]);
        assert!(empty.selected().is_none());
        assert!(empty.is_empty());
    }

    #[test]
    fn an_offer_found_for_two_tracks_is_told_apart_by_track() {
        // The same release can answer two searches; which one was picked decides
        // which local track the download is paired with.
        let mut a = group("101", 1);
        let mut b = group("202", 1);
        let shared = ItemRef::new(BackendId::Bandcamp, "t:shared");
        a.outcome.offers[0].offer.item_ref = shared.clone();
        b.outcome.offers[0].offer.item_ref = shared.clone();
        let s = results(vec![a, b], vec![]);
        assert_eq!(
            s.find(Some("202"), &shared).unwrap().0.src_id.as_deref(),
            Some("202")
        );
        assert!(s.find(Some("303"), &shared).is_none());
    }

    fn basket(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    fn library() -> Vec<TrackRow> {
        vec![
            TrackRow::stub("1", "alpha"),
            TrackRow::stub("2", "beta"),
            TrackRow::stub("3", "gamma"),
            TrackRow::stub("4", "delta"),
        ]
    }

    #[test]
    fn the_basket_survives_a_filter_that_hides_part_of_it() {
        // The bug: the search was built from the *visible* rows, so typing a
        // filter after filling the basket silently dropped whatever it hid. The
        // basket still showed the right count, so it read as the program losing
        // selections at random.
        let (rows, total) = basket_rows(&library(), &basket(&["1", "3", "4"]), 25, |_| true);
        assert_eq!(total, 3);
        assert_eq!(
            rows.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["1", "3", "4"],
            "every basket item must be queued regardless of the filter"
        );
    }

    #[test]
    fn the_basket_is_queued_in_library_order() {
        // Selection order is not recorded, so the order has to come from
        // somewhere stable rather than from a HashSet's iteration order.
        let picked = basket(&["4", "2", "1"]);
        for _ in 0..8 {
            let (rows, _) = basket_rows(&library(), &picked, 25, |_| true);
            assert_eq!(
                rows.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
                ["1", "2", "4"]
            );
        }
    }

    #[test]
    fn the_cap_limits_what_is_queued_but_reports_the_whole_basket() {
        let (rows, total) = basket_rows(&library(), &basket(&["1", "2", "3", "4"]), 2, |_| true);
        assert_eq!(rows.len(), 2);
        assert_eq!(total, 4, "the caller needs the real total to warn with");
    }

    #[test]
    fn the_cap_applies_to_what_is_left_to_search_not_the_whole_basket() {
        // The bug: capping before this filter meant a basket over the cap only
        // ever offered its first `cap` items, and once those were queued every
        // further press said "nothing new to search".
        let all = basket(&["1", "2", "3", "4"]);
        let done = ["1", "2"];
        let (rows, total) = basket_rows(&library(), &all, 2, |r| !done.contains(&r.id.as_str()));
        assert_eq!(
            rows.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["3", "4"],
            "the second press must reach the items the first one did not"
        );
        assert_eq!(total, 2, "the total counts what is still to search");
    }

    fn queued(what: &str, src: Option<&str>) -> QueuedFetch {
        QueuedFetch {
            what: what.to_string(),
            src: src.map(str::to_string),
            item: ItemRef::new(BackendId::Bandcamp, format!("k:{what}")),
        }
    }

    #[test]
    fn each_finished_download_is_paired_with_its_own_source_track() {
        // The bug this guards: one shared `fetch_src` slot meant the second
        // download's file was queued as a transfer against the *third* track's
        // source, silently pairing the wrong old→new tracks.
        let mut q = FetchQueue::default();
        q.push(queued("a", Some("src-a")));
        q.push(queued("b", Some("src-b")));
        q.push(queued("c", None));

        fn src_of(q: Option<QueuedFetch>) -> Option<String> {
            q.and_then(|q| q.src)
        }

        assert_eq!(q.label(), Some("a"), "the front one is the running one");
        assert_eq!(src_of(q.finish_front()).as_deref(), Some("src-a"));
        assert_eq!(q.label(), Some("b"));
        assert_eq!(src_of(q.finish_front()).as_deref(), Some("src-b"));
        assert_eq!(
            src_of(q.finish_front()),
            None,
            "'just download it' stays unpaired"
        );
        assert!(q.is_empty());
        assert_eq!(q.label(), None);
    }

    #[test]
    fn the_same_offer_twice_reports_its_place_rather_than_queueing_again() {
        let mut q = FetchQueue::default();
        q.push(queued("a", None));
        q.push(queued("b", None));

        assert_eq!(q.ahead_of(&queued("a", None).item), Some(0));
        assert_eq!(q.ahead_of(&queued("b", None).item), Some(1));
        assert_eq!(q.ahead_of(&queued("c", None).item), None);
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn a_dead_worker_abandons_the_whole_queue_not_just_the_running_one() {
        let mut q = FetchQueue::default();
        q.push(queued("a", None));
        q.push(queued("b", None));
        q.abandon_all();
        assert!(q.is_empty(), "nothing will ever answer these");
    }

    #[test]
    fn an_id_no_longer_in_the_library_is_ignored() {
        let (rows, total) = basket_rows(&library(), &basket(&["2", "999"]), 25, |_| true);
        assert_eq!(rows.len(), 1);
        assert_eq!(total, 1);
        assert_eq!(rows[0].id, "2");
    }

    #[test]
    fn the_track_tag_tells_found_nothing_apart_from_not_searched() {
        // Both would otherwise render as a blank tag, and the user could not tell
        // whether searching again would do anything.
        let s = results(vec![group("101", 3), group("202", 0)], vec![]);
        let queued = vec!["303".to_string()];
        assert_eq!(track_state(&s, &queued, "101"), ShopTrackState::Done(3));
        assert_eq!(track_state(&s, &queued, "202"), ShopTrackState::Done(0));
        assert_eq!(track_state(&s, &queued, "303"), ShopTrackState::Queued);
        assert_eq!(track_state(&s, &queued, "404"), ShopTrackState::Untouched);
    }

    #[test]
    fn a_queued_track_that_has_answered_reads_as_done() {
        // The queue list is cleared on arrival, but a stale entry must not make a
        // finished search look like it is still waiting.
        let s = results(vec![group("101", 2)], vec![]);
        let stale = vec!["101".to_string()];
        assert_eq!(track_state(&s, &stale, "101"), ShopTrackState::Done(2));
    }

    #[test]
    fn nothing_is_queued_or_done_before_the_first_search() {
        assert_eq!(
            track_state(&ShopState::Idle, &[], "101"),
            ShopTrackState::Untouched
        );
    }

    #[test]
    fn a_search_in_flight_still_reports_its_specs() {
        // So searching again can reopen a running search instead of refusing
        // with no way back.
        let s = ShopState::Searching {
            since: Instant::now(),
            what: "track 101".into(),
            done: 0,
            total: 1,
            specs: vec![spec("101")],
        };
        assert_eq!(s.specs().unwrap()[0].src_id.as_deref(), Some("101"));
        assert!(s.selected().is_none());
    }
}
