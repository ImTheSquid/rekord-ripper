use std::collections::HashSet;
use std::time::Instant;

use crate::analysis::{CopyOpts, Plan};
use crate::db::{MasterDb, SafetyOpts, rekordbox_running};

use crate::library::{TrackRow, load_rows};

use super::data::{dst_visible, src_visible};

pub const DURATION_TOL_SECS: i64 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Src,
    Dst,
}

/// The two full screens.
///
/// The shop used to be an overlay on the transfer view, which forced `Space` to
/// mean two unrelated things at once — "copy target" in the DESTINATIONS column
/// and "shop for this" in SOURCES — and left "select some sources, then move to
/// destinations" with no meaning at all. Each screen now owns its own selection,
/// and each selection means exactly one thing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    Transfer,
    Shop,
    /// Downloads that have not become transfers yet.
    Pending,
}

/// Which pane of the shop screen the keys go to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShopFocus {
    Tracks,
    Offers,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputMode {
    Normal,
    Search(Focus),
    /// A modal awaiting y/n. Kinded because `handle_key` dispatches on mode
    /// *before* screen, so a single `Confirm` meant `y` on any screen ran the
    /// transfer batch.
    Confirm(ConfirmKind),
    Help,
    /// Typing into the shop screen's track filter.
    ShopSearch,
}

/// Which modal is open, and therefore what `y` means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfirmKind {
    /// The transfer screen's cp batch.
    Transfer,
    /// Creating `djmdContent` rows for queued downloads.
    ImportRows,
}

/// Rows a confirmed import will write.
///
/// A snapshot, taken when the modal opens and never added to. `pump_worker`
/// runs on every tick regardless of mode, so a probe landing mid-modal would
/// otherwise grow the batch between the user reading it and pressing `y` —
/// which is exactly the gate that exists to get a yes *having seen* the rows.
pub struct ImportBatch {
    pub rows: Vec<crate::import::NewContent>,
    /// The entries those rows belong to, in the same order.
    pub entry_ids: Vec<i64>,
    /// Clamped during render, like the help popup: `import::render` is ~24 lines
    /// a row, so a batch of three overflows any terminal.
    pub scroll: u16,
}

/// How far along a track is in the shop screen's track list.
///
/// Shown per row so a queue of searches is legible: which are answered, which
/// are still waiting, and how much each one found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShopTrackState {
    Untouched,
    Queued,
    Done(usize),
}

/// Where the shop overlay has got to.
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
        specs: Vec<crate::acquire::shop::QuerySpec>,
    },
    Results {
        groups: Box<Vec<crate::acquire::shop::GroupOutcome>>,
        /// Index into the flattened offer list across all groups.
        cursor: usize,
        /// The specs these results answer, used to decide reopen vs re-search.
        specs: Vec<crate::acquire::shop::QuerySpec>,
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
    },
    Done {
        paths: Vec<std::path::PathBuf>,
        queued: Option<i64>,
    },
    Failed(String),
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
    pub fn flattened(
        &self,
    ) -> impl Iterator<
        Item = (
            &crate::acquire::shop::GroupOutcome,
            &crate::acquire::shop::RankedOffer,
        ),
    > {
        let groups: &[crate::acquire::shop::GroupOutcome] = match self {
            Self::Results { groups, .. } => groups,
            _ => &[],
        };
        groups
            .iter()
            .flat_map(|g| g.outcome.offers.iter().map(move |o| (g, o)))
    }

    pub fn selected(&self) -> Option<&crate::acquire::shop::RankedOffer> {
        self.selected_with_group().map(|(_, o)| o)
    }

    /// The highlighted offer and the group it belongs to.
    ///
    /// The group carries the source track, which is what a bulk search needs in
    /// order to pair a download with the right local track.
    pub fn selected_with_group(
        &self,
    ) -> Option<(
        &crate::acquire::shop::GroupOutcome,
        &crate::acquire::shop::RankedOffer,
    )> {
        let cursor = match self {
            Self::Results { cursor, .. } => *cursor,
            _ => return None,
        };
        self.flattened().nth(cursor)
    }

    /// The specs these results answer, if any.
    pub fn specs(&self) -> Option<&[crate::acquire::shop::QuerySpec]> {
        match self {
            Self::Searching { specs, .. } | Self::Results { specs, .. } => Some(specs),
            _ => None,
        }
    }
}

/// Selected rows the column's filter is currently hiding.
///
/// Worth surfacing: a selection you cannot see reads as one the program dropped,
/// even though it is still in the batch.
fn hidden_selected(rows: &[TrackRow], col: &ColumnState) -> usize {
    let shown = col
        .visible
        .iter()
        .filter_map(|&i| rows.get(i))
        .filter(|r| col.selected.contains(&r.id))
        .count();
    col.selected.len().saturating_sub(shown)
}

/// Basket rows in library order, capped, with the uncapped total.
///
/// Walks every row rather than the visible ones: filtering the list after filling
/// the basket used to drop the hidden items from the search without a word, which
/// looked exactly like the basket being forgotten.
fn basket_rows(
    rows: &[TrackRow],
    selected: &HashSet<String>,
    cap: usize,
) -> (Vec<TrackRow>, usize) {
    let all: Vec<TrackRow> = rows
        .iter()
        .filter(|r| selected.contains(&r.id))
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

#[derive(Clone, Copy, Default, Debug)]
pub struct DstFilters {
    pub auto: bool,
    pub fuzzy_from_src: bool,
}

#[derive(Clone, Debug, Default)]
pub struct ColumnState {
    pub query: String,
    pub visible: Vec<usize>,
    pub cursor: usize,
    pub selected: HashSet<String>,
}

impl ColumnState {
    pub fn clamp_cursor(&mut self) {
        if self.visible.is_empty() {
            self.cursor = 0;
        } else if self.cursor >= self.visible.len() {
            self.cursor = self.visible.len() - 1;
        }
    }
    pub fn move_by(&mut self, delta: isize) {
        if self.visible.is_empty() {
            self.cursor = 0;
            return;
        }
        let n = self.visible.len() as isize;
        let mut c = self.cursor as isize + delta;
        if c < 0 {
            c = 0;
        }
        if c >= n {
            c = n - 1;
        }
        self.cursor = c as usize;
    }
    pub fn jump_top(&mut self) {
        self.cursor = 0;
    }
    pub fn jump_bottom(&mut self) {
        if !self.visible.is_empty() {
            self.cursor = self.visible.len() - 1;
        }
    }
}

#[derive(Default)]
pub struct StatusLine {
    pub text: String,
    pub level: StatusLevel,
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
pub enum StatusLevel {
    #[default]
    Info,
    Warn,
    Err,
    Ok,
}

impl StatusLine {
    pub fn info(&mut self, msg: impl Into<String>) {
        self.text = msg.into();
        self.level = StatusLevel::Info;
    }
    pub fn ok(&mut self, msg: impl Into<String>) {
        self.text = msg.into();
        self.level = StatusLevel::Ok;
    }
    pub fn warn(&mut self, msg: impl Into<String>) {
        self.text = msg.into();
        self.level = StatusLevel::Warn;
    }
    pub fn err(&mut self, msg: impl Into<String>) {
        self.text = msg.into();
        self.level = StatusLevel::Err;
    }
}

pub struct PendingBatch {
    pub plans: Vec<Plan>,
    pub failures: Vec<(String, String)>, // dst_id, error
    /// Rows scrolled past in the confirm modal, clamped at draw time.
    pub scroll: u16,
}

pub struct App {
    pub db: MasterDb,
    pub safety: SafetyOpts,

    pub rows: Vec<TrackRow>,
    pub rb_running: bool,
    pub rb_last_polled: Instant,

    pub src: ColumnState,
    pub dst: ColumnState,
    pub focus: Focus,
    pub screen: Screen,
    pub shop_focus: ShopFocus,
    /// The shop screen's own track list: its own filter, and a selection that
    /// only ever means "search for these".
    pub shop_list: ColumnState,
    pub mode: InputMode,
    pub copy_opts: CopyOpts,
    pub dst_filters: DstFilters,

    pub status: StatusLine,
    /// `None` when the worker thread could not be started; searching is then
    /// simply unavailable rather than the TUI failing to open.
    pub worker: Option<super::worker::Worker>,
    pub shop: ShopState,
    pub fetch: FetchState,
    /// The source track a fetch was started for, so the transfer can be queued
    /// once the file lands.
    fetch_src: Option<String>,
    /// Source track ids submitted to the worker but not yet answered, so `s` on
    /// the same track twice does not search it twice.
    shop_queued: Vec<String>,
    pub cfg: crate::config::Config,
    pub pending: Option<PendingBatch>,
    pub unresolved_errors: bool,
    /// The download queue, and the store behind it.
    ///
    /// `None` means the queue is unavailable, not that the TUI failed to start
    /// — the same treatment `worker` gets. Held open rather than reopened per
    /// keypress: `open()` re-runs the schema setup and has nowhere to report a
    /// failure from inside a key handler.
    pub queue: super::queue::QueueState,
    pub store: Option<crate::pending::PendingStore>,
    /// The snapshot behind `ConfirmKind::ImportRows`.
    pub import_batch: Option<ImportBatch>,
    /// First visible line of the help popup. Clamped during render, which is
    /// the only place the popup's height is known.
    pub help_scroll: u16,
    /// Set true on the first `q` press when there's unsaved selection state.
    /// Reset by any other key. A second `q` while this is true actually quits.
    pub quit_pending: bool,
    pub should_quit: bool,
}

impl App {
    pub fn new(db: MasterDb, safety: SafetyOpts) -> anyhow::Result<Self> {
        let cfg_path = crate::paths::config_path(None)?;
        let cfg = crate::config::Config::load(&cfg_path).unwrap_or_default();
        let creds = crate::config::Credentials::load(&crate::paths::credentials_path()?)
            .unwrap_or_default();
        // A worker that will not start costs us searching, not the whole TUI.
        let worker = super::worker::Worker::spawn(&cfg, &creds).ok();

        let rows = load_rows(&db)?;
        let mut app = App {
            db,
            safety,
            rows,
            rb_running: rekordbox_running(),
            rb_last_polled: Instant::now(),
            src: ColumnState::default(),
            dst: ColumnState::default(),
            focus: Focus::Src,
            screen: Screen::Transfer,
            shop_focus: ShopFocus::Tracks,
            shop_list: ColumnState::default(),
            mode: InputMode::Normal,
            copy_opts: CopyOpts::default(),
            dst_filters: DstFilters::default(),
            status: StatusLine::default(),
            worker,
            shop: ShopState::Idle,
            fetch: FetchState::Idle,
            fetch_src: None,
            shop_queued: Vec::new(),
            cfg,
            pending: None,
            unresolved_errors: false,
            queue: super::queue::QueueState::default(),
            store: crate::pending::PendingStore::open().ok(),
            import_batch: None,
            help_scroll: 0,
            quit_pending: false,
            should_quit: false,
        };
        app.recompute_visible();
        app.status
            .info(format!("Loaded {} tracks.", app.rows.len()));
        Ok(app)
    }

    pub fn recompute_visible(&mut self) {
        self.src.visible = src_visible(&self.rows, &self.src.query);
        self.src.clamp_cursor();

        // The shop list filters the same rows independently, so moving around on
        // one screen never disturbs the other.
        self.shop_list.visible = src_visible(&self.rows, &self.shop_list.query);
        self.shop_list.clamp_cursor();

        // Hand the current src to dst_visible so it always gets excluded from
        // the dst list (you can't copy a track onto itself); the fuzzy flag is
        // a separate axis that further narrows by normalized title + length.
        let src = self
            .src
            .visible
            .get(self.src.cursor)
            .and_then(|&i| self.rows.get(i))
            .cloned();
        self.dst.visible = dst_visible(
            &self.rows,
            &self.dst.query,
            self.dst_filters.auto,
            src.as_ref(),
            self.dst_filters.fuzzy_from_src,
            DURATION_TOL_SECS,
        );
        self.dst.clamp_cursor();
    }

    pub fn reload_db(&mut self) -> anyhow::Result<()> {
        self.rows = load_rows(&self.db)?;
        // Drop selections that no longer correspond to existing rows.
        let existing: HashSet<&str> = self.rows.iter().map(|r| r.id.as_str()).collect();
        self.dst
            .selected
            .retain(|id| existing.contains(id.as_str()));
        self.shop_list
            .selected
            .retain(|id| existing.contains(id.as_str()));
        self.recompute_visible();
        Ok(())
    }

    pub fn poll_rekordbox_if_due(&mut self) {
        if self.rb_last_polled.elapsed() >= std::time::Duration::from_secs(1) {
            self.rb_running = rekordbox_running();
            self.rb_last_polled = Instant::now();
        }
    }

    /// Drain the worker. Called every tick; never blocks.
    pub fn pump_worker(&mut self) {
        let Some(worker) = self.worker.as_mut() else {
            return;
        };
        for update in worker.drain() {
            match update {
                super::worker::Update::Started => {}
                super::worker::Update::Progress {
                    done: d,
                    total: t,
                    label,
                } => {
                    if let ShopState::Searching {
                        done, total, what, ..
                    } = &mut self.shop
                    {
                        *done = d;
                        *total = t;
                        // A bulk search gets real progress instead of a spinner
                        // that says nothing about how far along it is.
                        if t > 1 && !label.is_empty() {
                            *what = format!("{label}  ({}/{t})", d + 1);
                        }
                    }
                }
                super::worker::Update::Finished(new_groups) => {
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

                    // Append rather than replace, so a queue of searches builds up
                    // one list instead of each result wiping the last.
                    let specs = self.shop.specs().unwrap_or(&[]).to_vec();
                    let mut groups: Vec<crate::acquire::shop::GroupOutcome> =
                        match std::mem::replace(&mut self.shop, ShopState::Idle) {
                            ShopState::Results { groups, .. } => *groups,
                            _ => Vec::new(),
                        };
                    let cursor_at = groups.iter().map(|g| g.outcome.offers.len()).sum::<usize>();
                    groups.extend(*new_groups);
                    let total: usize = groups.iter().map(|g| g.outcome.offers.len()).sum();
                    self.shop = ShopState::Results {
                        groups: Box::new(groups),
                        // Land on the newly arrived block, which is what you were
                        // waiting for.
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
                super::worker::Update::Fetched(result) => match *result {
                    Ok(files) => {
                        let paths: Vec<std::path::PathBuf> =
                            files.iter().map(|f| f.path.clone()).collect();
                        let lossy = files.iter().any(|f| !f.format.is_lossless());
                        // Queueing needs the database, so it happens here on the
                        // main thread rather than in the worker.
                        let queued = self.queue_transfer_for(&paths);
                        match (queued, lossy) {
                            (Some(id), _) => self.status.ok(format!(
                                "downloaded and queued transfer #{id} — import, then `pending --apply`"
                            )),
                            (None, true) => self
                                .status
                                .warn("downloaded, but it is a lossy transcode".to_string()),
                            (None, false) => {
                                self.status.ok(format!("downloaded {} file(s)", paths.len()))
                            }
                        }
                        self.fetch = FetchState::Done { paths, queued };
                    }
                    Err(why) => {
                        self.status.err(format!("download failed: {why}"));
                        self.fetch = FetchState::Failed(why);
                    }
                },
                // The queue screen consumes these; until it exists they only
                // prove the work runs off the event thread.
                super::worker::Update::Probed(r) => {
                    let (entry_id, generation, result) = *r;
                    self.on_probed(entry_id, generation, result);
                }
                super::worker::Update::Fingerprinted(r) => {
                    let (entry_id, generation, result) = *r;
                    self.on_fingerprinted(entry_id, generation, result);
                }
                super::worker::Update::Failed(why) => {
                    self.status.err(format!("worker failed: {why}"));
                    // A dead thread answers nothing. Without this every queued
                    // row keeps its "working" tag forever and every key is
                    // refused as already-in-flight.
                    self.queue.abandon_in_flight(&why);
                    if matches!(self.fetch, FetchState::Running { .. }) {
                        self.fetch = FetchState::Failed(why);
                    } else {
                        self.shop = ShopState::Failed(why);
                    }
                }
            }
        }
    }

    /// Build the search spec for one source row.
    fn spec_for(&self, row: &TrackRow) -> Option<crate::acquire::shop::QuerySpec> {
        let title = row.title.trim().to_string();
        if title.is_empty() {
            return None;
        }
        // TrackRow stores these as plain Strings, with empty meaning absent.
        let artist = Some(row.artist.trim().to_string()).filter(|a| !a.is_empty());
        Some(crate::acquire::shop::QuerySpec {
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

    /// What `s` on the transfer screen does: cross to the shop screen, landing on
    /// the track that was highlighted, and search for it.
    pub fn open_shop(&mut self) -> bool {
        self.screen = Screen::Shop;
        let Some(row) = self.current_src().cloned() else {
            self.shop_focus = ShopFocus::Tracks;
            self.status
                .info("pick a track and press 's' to search for it.");
            return false;
        };
        self.focus_shop_list_on(&row.id);
        self.shop_track()
    }

    /// Search for the shop screen's highlighted track, or show what is already
    /// known about it.
    ///
    /// Never re-runs a finished search: `s` used to throw completed results away,
    /// and while one was running it refused with no way back to it at all.
    pub fn shop_track(&mut self) -> bool {
        let Some(row) = self.current_shop_track().cloned() else {
            self.status.warn("no track highlighted.");
            return false;
        };

        // Already answered: jump the offer table to its block. Focus stays where
        // it is — stealing it to the offers pane is what made the next `s` look
        // like a dead key.
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
                "showing the {found} offer(s) already found for {} — 'r' re-runs it.",
                row.title
            ));
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

    /// Put the shop list's cursor on `id`.
    ///
    /// A filter hiding the row loses to the seeded cursor: arriving from the
    /// transfer screen on some unrelated track would be worse than a cleared
    /// filter.
    fn focus_shop_list_on(&mut self, id: &str) {
        if let Some(p) = self.position_in_shop_list(id) {
            self.shop_list.cursor = p;
            return;
        }
        self.shop_list.query.clear();
        self.recompute_visible();
        if let Some(p) = self.position_in_shop_list(id) {
            self.shop_list.cursor = p;
        }
    }

    fn position_in_shop_list(&self, id: &str) -> Option<usize> {
        self.shop_list
            .visible
            .iter()
            .position(|&i| self.rows.get(i).is_some_and(|r| r.id == id))
    }

    pub fn toggle_shop_focus(&mut self) {
        self.shop_focus = match self.shop_focus {
            ShopFocus::Tracks => ShopFocus::Offers,
            ShopFocus::Offers => ShopFocus::Tracks,
        };
    }

    pub fn shop_move(&mut self, delta: isize) {
        match self.shop_focus {
            ShopFocus::Tracks => self.shop_list.move_by(delta),
            // Deliberately does not move the track cursor. It used to follow the
            // offer cursor, which meant `s` acted on whichever track the offers
            // happened to belong to rather than the one the user had highlighted
            // — so from the offers pane `s` always landed on an already-searched
            // track and did nothing visible.
            ShopFocus::Offers => self.shop.move_cursor(delta),
        }
    }

    pub fn shop_jump(&mut self, top: bool) {
        match (self.shop_focus, top) {
            (ShopFocus::Tracks, true) => self.shop_list.jump_top(),
            (ShopFocus::Tracks, false) => self.shop_list.jump_bottom(),
            (ShopFocus::Offers, true) => self.shop_move(isize::MIN / 2),
            (ShopFocus::Offers, false) => self.shop_move(isize::MAX / 2),
        }
    }

    /// Add or remove the highlighted track from the basket.
    pub fn toggle_basket(&mut self) {
        if self.shop_focus != ShopFocus::Tracks {
            self.status
                .info("space fills the basket — Tab back to the track list.");
            return;
        }
        let Some(id) = self.current_shop_track().map(|r| r.id.clone()) else {
            return;
        };
        if !self.shop_list.selected.remove(&id) {
            self.shop_list.selected.insert(id);
        }
    }

    /// How far along a track is: answered, waiting, or untouched.
    pub fn shop_track_state(&self, src_id: &str) -> ShopTrackState {
        track_state(&self.shop, &self.shop_queued, src_id)
    }

    /// Add source tracks to the search queue, keeping any results already shown.
    ///
    /// Sequential by construction: the worker takes one job at a time, so tapping
    /// `s` on several tracks builds a list that works through itself rather than
    /// firing a burst of concurrent requests at each backend.
    pub fn enqueue_shop(&mut self, rows: &[TrackRow]) -> bool {
        let specs: Vec<_> = rows
            .iter()
            // Skip anything already answered or already queued.
            .filter(|r| {
                self.first_offer_index_for(&r.id).is_none() && !self.shop_queued.contains(&r.id)
            })
            .filter_map(|r| self.spec_for(r))
            .collect();

        if specs.is_empty() {
            self.status
                .info("nothing new to search — those are already done or queued.");
            return false;
        }
        self.submit_shop_queued(specs)
    }

    /// Index into the flattened offer list of the first offer found for `src_id`.
    fn first_offer_index_for(&self, src_id: &str) -> Option<usize> {
        self.shop
            .flattened()
            .position(|(g, _)| g.src_id.as_deref() == Some(src_id))
    }

    /// Re-run one search, replacing just that track's results.
    ///
    /// On the offers pane that means the group the highlighted offer belongs to,
    /// which after a queue of searches is not necessarily the track under the
    /// list cursor.
    pub fn start_shop(&mut self) -> bool {
        let id = match self.shop_focus {
            ShopFocus::Offers => self
                .shop
                .selected_with_group()
                .and_then(|(g, _)| g.src_id.clone())
                .or_else(|| self.current_shop_track().map(|r| r.id.clone())),
            ShopFocus::Tracks => self.current_shop_track().map(|r| r.id.clone()),
        };
        let Some(row) = id.and_then(|id| self.rows.iter().find(|r| r.id == id).cloned()) else {
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
    pub fn shop_selected(&mut self, cap: usize) -> bool {
        if self.shop_list.selected.is_empty() {
            self.status
                .warn("the basket is empty — press space on the tracks you want, then 'S'.");
            return false;
        }
        let (rows, total) = basket_rows(&self.rows, &self.shop_list.selected, cap);
        let queued = rows.len();
        let ok = self.enqueue_shop(&rows);
        // Said *after* the submit, which sets its own status — a warning written
        // before it was simply overwritten and never seen.
        if ok && total > queued {
            let text = self.status.text.clone();
            self.status.warn(format!(
                "{text} {total} in the basket, only the first {queued} queued — press 'S' again for the rest."
            ));
        }
        ok
    }

    /// Basket items the current filter is hiding.
    pub fn basket_hidden(&self) -> usize {
        hidden_selected(&self.rows, &self.shop_list)
    }

    /// Selected destinations the current filters are hiding.
    ///
    /// `fuzzy_from_src` narrows the destination list from the *source* cursor, so
    /// simply moving around on the left can hide rows that are still selected and
    /// still in the apply batch.
    pub fn dst_hidden(&self) -> usize {
        hidden_selected(&self.rows, &self.dst)
    }

    /// The source track of the highlighted offer.
    ///
    /// Marked in the track list rather than moved to: the cursor there is the
    /// user's, and `s` acts on it.
    pub fn shop_offer_src(&self) -> Option<&str> {
        self.shop
            .selected_with_group()
            .and_then(|(g, _)| g.src_id.as_deref())
    }

    fn submit_shop_queued(&mut self, specs: Vec<crate::acquire::shop::QuerySpec>) -> bool {
        let opts = crate::acquire::shop::SearchOpts {
            timeout: std::time::Duration::from_secs(self.cfg.search.timeout_secs.max(1)),
            enrich_top_n: self.cfg.search.enrich_top_n,
            ..Default::default()
        };

        let Some(worker) = self.worker.as_mut() else {
            self.status
                .err("the search thread is not running; restart the TUI.");
            return false;
        };
        if !worker.submit(super::worker::Job::Shop {
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
        self.screen = Screen::Shop;
        true
    }

    /// Searches submitted but not yet answered.
    /// Open the queue screen, retiring anything that can no longer progress.
    pub fn open_queue(&mut self) {
        self.screen = Screen::Pending;
        self.reload_queue();
    }

    /// Re-read the store, sweeping first.
    ///
    /// `sweep` is what the CLI runs on every `pending` invocation: it retires
    /// entries whose file or source track went away. Entries vanishing from the
    /// list with no explanation would be worse than the noise, so each one is
    /// reported.
    pub fn reload_queue(&mut self) {
        let Some(store) = self.store.as_ref() else {
            self.status
                .err("the pending queue could not be opened — downloads cannot be finished here.");
            return;
        };
        // Collected before reporting: the store borrow has to end before the
        // status line can be written to.
        let swept = store.sweep(&self.db);
        let listed = self.store.as_ref().map(|s| s.all());

        match swept {
            Ok(retired) => {
                for (id, state, why) in retired {
                    self.status.warn(format!("#{id} → {state}: {why}"));
                }
            }
            Err(e) => self.status.warn(format!("sweep failed: {e}")),
        }
        match listed {
            Some(Ok(entries)) => {
                // Which files rekordbox already has a row for. The store's own
                // state cannot answer that — it stays `AwaitingImport` until the
                // transfer lands — so an entry whose row had just been created
                // still read "awaiting import".
                let present: std::collections::HashSet<i64> = entries
                    .iter()
                    .filter(|e| {
                        crate::pending::find_imported_row(&self.db, &e.acquired_path)
                            .ok()
                            .flatten()
                            .is_some()
                    })
                    .map(|e| e.id)
                    .collect();
                self.queue.reload(entries);
                self.queue.set_rows_present(present);
            }
            Some(Err(e)) => self.status.err(format!("could not read the queue: {e}")),
            None => {}
        }
    }

    /// Probe every queued download that rekordbox has no row for.
    ///
    /// Gate 1 is checked here rather than at the confirmation: probing a queue
    /// takes real time, and spending it only to refuse with "edit your config
    /// file" is not an instruction anyone can act on from inside a TUI.
    pub fn start_import(&mut self) {
        if !self.cfg.import.insert_content_rows {
            self.status.err(
                "creating rekordbox rows is off — set insert_content_rows = true under [import] \
                 in your config, or import the files into rekordbox by hand.",
            );
            return;
        }
        if self.queue.any_in_flight() {
            self.status.info("already working — give it a moment.");
            return;
        }

        // Which entries still need a row. Cheap DB lookups, so they stay here.
        let needing: Vec<(i64, std::path::PathBuf)> = self
            .queue
            .entries
            .iter()
            .filter(|e| e.state == crate::pending::State::AwaitingImport)
            .filter(|e| {
                crate::pending::find_imported_row(&self.db, &e.acquired_path)
                    .ok()
                    .flatten()
                    .is_none()
            })
            .map(|e| (e.id, e.acquired_path.clone()))
            .collect();

        if needing.is_empty() {
            self.status
                .info("every queued download already has a rekordbox row.");
            return;
        }

        let generation = self.queue.next_generation();
        let mut sent = 0;
        for (entry_id, path) in needing {
            if !path.exists() {
                self.queue.set_work(
                    entry_id,
                    super::queue::EntryWork::Failed("file is gone".into()),
                );
                continue;
            }
            let job = super::worker::Job::Probe {
                entry_id,
                generation,
                path,
            };
            if self.worker.as_mut().is_some_and(|w| w.submit(job)) {
                self.queue.set_work(
                    entry_id,
                    super::queue::EntryWork::Probing {
                        since: Instant::now(),
                        generation,
                    },
                );
                sent += 1;
            }
        }
        if sent == 0 {
            self.status
                .err("could not start — the worker is not running.");
        } else {
            self.status.info(format!("reading {sent} file(s)…"));
        }
    }

    /// A probe came back: plan the row, and open the modal once all are in.
    pub fn on_probed(
        &mut self,
        entry_id: i64,
        generation: u64,
        result: Result<crate::audio::AudioInfo, String>,
    ) {
        if !self.queue.accepts(entry_id, generation) {
            return;
        }
        let Some(entry) = self
            .queue
            .entries
            .iter()
            .find(|e| e.id == entry_id)
            .cloned()
        else {
            return;
        };
        let path = entry.acquired_path.clone();

        let planned = result.and_then(|info| {
            // The download's own tags win; the source track fills the gaps.
            //
            // A rip usually carries no tags at all, and `plan_insert`'s last
            // resort is the filename stem — which produced a track titled
            // "OW：3N - ALL THE LOCALS ARE LOCALING" with no artist, and a
            // full-width colon at that, because a real one cannot go in a
            // filename. The queue knows what this file is a copy of, so it can
            // do better than the stem.
            let title = info
                .tags
                .title
                .is_none()
                .then_some(entry.src_title.as_deref())
                .flatten();
            let artist = (info.tags.artist.is_none() && info.tags.album_artist.is_none())
                .then_some(entry.src_artist.as_deref())
                .flatten();
            crate::import::plan_insert(&self.db, &path, &info, title, artist)
                .map_err(|e| e.to_string())
        });
        match planned {
            Ok(new) => self
                .queue
                .set_work(entry_id, super::queue::EntryWork::Planned(Box::new(new))),
            Err(why) => self
                .queue
                .set_work(entry_id, super::queue::EntryWork::Failed(why)),
        }
        if !self.queue.any_in_flight() {
            self.open_import_confirm();
        }
    }

    /// Snapshot everything planned and show it.
    fn open_import_confirm(&mut self) {
        let mut rows = Vec::new();
        let mut entry_ids = Vec::new();
        for entry in &self.queue.entries {
            if let Some(super::queue::EntryWork::Planned(new)) = self.queue.work_for(entry.id) {
                rows.push((**new).clone());
                entry_ids.push(entry.id);
            }
        }
        if rows.is_empty() {
            self.status.warn("nothing could be planned — see the rows.");
            return;
        }
        // One artist row between them, not one each: nothing has been written
        // yet, so each plan asked the database and none saw the others.
        crate::import::dedupe_lookups(&mut rows);

        self.import_batch = Some(ImportBatch {
            rows,
            entry_ids,
            scroll: 0,
        });
        self.mode = InputMode::Confirm(ConfirmKind::ImportRows);
    }

    /// Gates 2 and 3 are behind us: write the rows.
    pub fn apply_import_batch(&mut self) {
        let Some(batch) = self.import_batch.take() else {
            self.mode = InputMode::Normal;
            return;
        };
        self.mode = InputMode::Normal;

        if let Err(e) = crate::db::safety_preflight(self.safety) {
            self.status.err(format!("{e}"));
            return;
        }
        let backup = match self.db.backup() {
            Ok(p) => p,
            Err(e) => {
                self.status
                    .err(format!("backup failed, nothing written: {e}"));
                return;
            }
        };

        // One insert owns its transaction, so a failure part-way leaves the
        // earlier rows committed. The backup is the undo; carry on rather than
        // stranding the rest of the batch.
        let (mut done, mut failed) = (0usize, Vec::new());
        for (new, entry_id) in batch.rows.iter().zip(&batch.entry_ids) {
            match crate::import::insert(&mut self.db, new) {
                Ok(mut note) => {
                    note.backup = Some(backup.to_string_lossy().into_owned());
                    let _ = note.write_beside(&backup);
                    self.queue.clear_work(*entry_id);
                    done += 1;
                }
                Err(e) => {
                    failed.push(e.to_string());
                    self.queue
                        .set_work(*entry_id, super::queue::EntryWork::Failed(e.to_string()));
                }
            }
        }

        self.reload_queue();
        let total = batch.rows.len();
        if let Some(first) = failed.first() {
            self.unresolved_errors = true;
            self.status
                .err(format!("imported {done}/{total}. Failed → {first}"));
            return;
        }
        self.status.ok(format!(
            "imported {done} row(s) — checking fingerprints now. Backup: {}",
            backup.display()
        ));
        // The rows exist only so the transfer can happen, so go straight on to
        // it rather than leaving the user on a screen that looks unchanged with
        // no word about what to press. The fingerprint is still the gate, and
        // it still fails closed.
        if done > 0 {
            self.start_apply();
        }
    }

    /// Fingerprint the queued downloads that now have a row, one at a time.
    ///
    /// One at a time on purpose. A gate against a streaming source rips it
    /// before decoding, so ten entries could hold the single worker thread for
    /// the better part of an hour — and `Worker` has no way to drop queued
    /// jobs, so there would be no cancel short of killing the TUI.
    pub fn start_apply(&mut self) {
        if self.queue.any_in_flight() {
            self.status.info("already working — give it a moment.");
            return;
        }
        self.queue.next_generation();
        if !self.submit_next_gate() {
            self.status
                .info("nothing to check — import the downloads first with 'i'.");
        }
    }

    /// Submit the next entry needing a verdict. False when there are none left.
    fn submit_next_gate(&mut self) -> bool {
        let generation = self.queue.current_generation();
        let candidates: Vec<crate::pending::Entry> = self
            .queue
            .entries
            .iter()
            .filter(|e| e.state == crate::pending::State::AwaitingImport)
            .filter(|e| {
                !matches!(
                    self.queue.work_for(e.id),
                    Some(
                        super::queue::EntryWork::Ready { .. } | super::queue::EntryWork::Failed(_)
                    )
                )
            })
            .cloned()
            .collect();

        for entry in candidates {
            // Resolved now, and the plan is built against this id rather than a
            // fresh lookup when the verdict lands.
            let Some(dst_content_id) =
                crate::pending::find_imported_row(&self.db, &entry.acquired_path)
                    .ok()
                    .flatten()
            else {
                continue;
            };
            let Ok(src) = crate::analysis::load_track(&self.db, &entry.src_content_id) else {
                self.queue.set_work(
                    entry.id,
                    super::queue::EntryWork::Failed("the source track is gone".into()),
                );
                continue;
            };
            if src.uuid != entry.src_uuid {
                self.queue.set_work(
                    entry.id,
                    super::queue::EntryWork::Failed("the source track was replaced".into()),
                );
                continue;
            }
            // A row this tool just inserted has a NULL BPM, so only duration
            // evidence is available; one rekordbox imported has both.
            let dst = crate::analysis::load_track(&self.db, &dst_content_id).ok();
            let (dst_length, dst_bpm) = match &dst {
                Some(d) => (d.length, d.bpm),
                None => (None, None),
            };

            let job = super::worker::Job::Fingerprint {
                entry_id: entry.id,
                generation,
                src: Box::new(src),
                dst_path: entry.acquired_path.clone(),
                dst_length,
                dst_bpm,
            };
            if self.worker.as_mut().is_some_and(|w| w.submit(job)) {
                self.queue.set_work(
                    entry.id,
                    super::queue::EntryWork::Fingerprinting {
                        since: Instant::now(),
                        generation,
                        dst_content_id,
                    },
                );
                self.status.info(format!(
                    "fingerprinting {}…",
                    entry.src_title.as_deref().unwrap_or("the download")
                ));
                return true;
            }
        }
        false
    }

    /// A verdict arrived. Build the plan against the row it was computed for.
    pub fn on_fingerprinted(
        &mut self,
        entry_id: i64,
        generation: u64,
        result: Result<crate::transfer::GateOutcome, String>,
    ) {
        if !self.queue.accepts(entry_id, generation) {
            return;
        }
        let Some(super::queue::EntryWork::Fingerprinting { dst_content_id, .. }) =
            self.queue.work_for(entry_id)
        else {
            return;
        };
        let dst_content_id = dst_content_id.clone();
        let Some(entry) = self
            .queue
            .entries
            .iter()
            .find(|e| e.id == entry_id)
            .cloned()
        else {
            return;
        };

        match result {
            Err(why) => self
                .queue
                .set_work(entry_id, super::queue::EntryWork::Failed(why)),
            Ok(outcome) if !outcome.verdict.is_accept() => {
                let why = outcome.verdict.summary();
                if let Some(store) = self.store.as_ref() {
                    let _ = store.set_rejected(entry_id, &why);
                }
                self.queue
                    .set_work(entry_id, super::queue::EntryWork::Failed(why));
            }
            Ok(outcome) => {
                // The row the verdict was computed against may have been
                // replaced while the gate ran. Applying to a different one
                // would bypass the only real check there is.
                let still = crate::pending::find_imported_row(&self.db, &entry.acquired_path)
                    .ok()
                    .flatten();
                if still.as_deref() != Some(dst_content_id.as_str()) {
                    self.queue.set_work(
                        entry_id,
                        super::queue::EntryWork::Failed(
                            "the rekordbox row changed while checking — press 'a' again".into(),
                        ),
                    );
                } else {
                    let opts = CopyOpts {
                        replace: entry.replace,
                        lock: entry.lock,
                    };
                    match crate::analysis::build_plan(
                        &self.db,
                        &entry.src_content_id,
                        &dst_content_id,
                        &opts,
                    ) {
                        Ok(plan) => self.queue.set_work(
                            entry_id,
                            super::queue::EntryWork::Ready {
                                plan: Box::new(plan),
                                verdict: outcome.verdict,
                            },
                        ),
                        Err(e) => self
                            .queue
                            .set_work(entry_id, super::queue::EntryWork::Failed(e.to_string())),
                    }
                }
            }
        }

        // Next in line, or write what is ready.
        if !self.submit_next_gate() {
            self.apply_ready();
        }
    }

    /// Write every plan the gate accepted.
    ///
    /// `apply_plan` takes its own backup per plan, and the store is only moved
    /// to `Applied` once the write succeeds — a verdict held in memory and lost
    /// to a quit costs a re-check, where a persisted `Matched` would strand the
    /// entry somewhere nothing moves it out of.
    fn apply_ready(&mut self) {
        let ready: Vec<i64> = self
            .queue
            .entries
            .iter()
            .map(|e| e.id)
            .filter(|id| {
                matches!(
                    self.queue.work_for(*id),
                    Some(super::queue::EntryWork::Ready { .. })
                )
            })
            .collect();
        if ready.is_empty() {
            self.status.warn("nothing passed the fingerprint check.");
            return;
        }
        if let Err(e) = crate::db::safety_preflight(self.safety) {
            self.status.err(format!("{e}"));
            return;
        }

        let (mut done, mut failed) = (0usize, Vec::new());
        for id in &ready {
            // Taken, not borrowed: `Plan` is not `Clone`, and applying it needs
            // ownership. On failure a `Failed` goes back in its place.
            let Some(super::queue::EntryWork::Ready { plan, verdict }) = self.queue.take_work(*id)
            else {
                continue;
            };
            let entry = self.queue.entries.iter().find(|e| e.id == *id).cloned();
            match crate::analysis::apply_plan(&mut self.db, &plan) {
                Ok(_backup) => {
                    // Only now: a verdict written before the transfer landed
                    // would leave the entry claiming work that never happened.
                    if let (Some(store), Some(entry)) = (self.store.as_ref(), entry.as_ref()) {
                        let _ = store.set_matched(*id, &plan.dst.id, &verdict.summary());
                        let _ = crate::transfer::mark_applied(store, entry);
                    }
                    done += 1;
                }
                Err(e) => {
                    failed.push(e.to_string());
                    self.queue
                        .set_work(*id, super::queue::EntryWork::Failed(e.to_string()));
                }
            }
        }

        self.reload_queue();
        match failed.first() {
            None => self.status.ok(format!("applied {done} transfer(s).")),
            Some(first) => {
                self.unresolved_errors = true;
                self.status
                    .err(format!("applied {done}/{}. Failed → {first}", ready.len()));
            }
        }
    }

    /// Put a rejected entry back in the running.
    ///
    /// `State::is_terminal` is deliberately false for `Rejected`, so a rejection
    /// is meant to be retryable — after re-encoding a file, or loosening
    /// `score_max`. The CLI has no way to ask for that; this is it.
    pub fn retry_selected(&mut self) {
        let Some(entry) = self.queue.selected().cloned() else {
            return;
        };
        self.queue.clear_work(entry.id);
        if entry.state != crate::pending::State::AwaitingImport {
            let Some(store) = self.store.as_ref() else {
                return;
            };
            if let Err(e) = store.set_state(entry.id, crate::pending::State::AwaitingImport) {
                self.status
                    .err(format!("could not reset #{}: {e}", entry.id));
                return;
            }
        }
        self.reload_queue();
        self.status
            .info(format!("#{} is back in the queue.", entry.id));
    }

    /// Drop one entry. Deliberately one at a time: `remove` is a hard delete
    /// with no undo, so a bulk clear would be a foot-gun.
    pub fn forget_selected(&mut self) {
        let Some(entry) = self.queue.selected().cloned() else {
            return;
        };
        let Some(store) = self.store.as_ref() else {
            return;
        };
        match store.remove(entry.id) {
            Ok(()) => {
                self.queue.clear_work(entry.id);
                self.reload_queue();
                self.status.info(format!("forgot #{}.", entry.id));
            }
            Err(e) => self.status.err(format!("could not forget it: {e}")),
        }
    }

    /// Scroll whichever modal is open.
    pub fn scroll_confirm(&mut self, delta: i32) {
        if let Some(batch) = self.import_batch.as_mut() {
            batch.scroll = (batch.scroll as i32 + delta).max(0) as u16;
        }
        if let Some(batch) = self.pending.as_mut() {
            batch.scroll = (batch.scroll as i32 + delta).max(0) as u16;
        }
    }

    /// Searches outstanding — not every job. The shop screen's counter and the
    /// quit guard both phrase themselves as "search(es)", so counting
    /// fingerprints here would make them lie.
    pub fn shop_outstanding(&self) -> usize {
        self.outstanding_of(super::worker::JobKind::Search)
    }

    pub fn outstanding_of(&self, kind: super::worker::JobKind) -> usize {
        self.worker
            .as_ref()
            .map(|w| w.outstanding_of(kind))
            .unwrap_or(0)
    }

    /// Everything in flight, for the quit guard.
    pub fn work_in_flight(&self) -> Vec<(super::worker::JobKind, usize)> {
        use super::worker::JobKind::*;
        [Search, Fetch, Probe, Fingerprint]
            .into_iter()
            .map(|k| (k, self.outstanding_of(k)))
            .filter(|(_, n)| *n > 0)
            .collect()
    }

    /// When the work currently in flight started, for the spinner.
    pub fn shop_since(&self) -> Option<Instant> {
        match (&self.shop, &self.fetch) {
            (_, FetchState::Running { since, .. }) => Some(*since),
            (ShopState::Searching { since, .. }, _) => Some(*since),
            _ => None,
        }
    }

    pub fn shop_busy(&self) -> bool {
        self.worker.as_ref().map(|w| w.is_busy()).unwrap_or(false)
    }

    /// Download the highlighted offer.
    ///
    /// Does the whole thing rather than printing a command to run: the worker
    /// makes a multi-minute download survivable, so there is no reason to hand
    /// the user homework.
    pub fn start_fetch(&mut self) -> bool {
        // Take the source from the offer's own group, not the cursor in the left
        // column: in a bulk search the highlighted offer belongs to whichever
        // track it was found for, which may not be the one highlighted now.
        let Some((offer, group_src)) = self
            .shop
            .selected_with_group()
            .map(|(g, r)| (r.offer.clone(), g.src_id.clone()))
        else {
            self.status.warn("no offer selected.");
            return false;
        };
        // Something you have to pay for cannot be fetched; say what to do instead.
        if offer.requires_purchase() {
            self.status.warn(format!(
                "you don't own this yet ({}). Press 'o' to open the buy page.",
                super::super::acquire::render::price_cell(&offer)
            ));
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

        let Some(worker) = self.worker.as_mut() else {
            self.status.err("the worker thread is not running.");
            return false;
        };
        // Only a download conflicts with a download. A fingerprint can take
        // minutes, and blocking on the shared counter made `f` a dead key with
        // nothing on screen to explain why.
        if worker.outstanding_of(super::worker::JobKind::Fetch) > 0 {
            self.status.warn("a download is already running.");
            return false;
        }
        if !worker.submit(super::worker::Job::Fetch {
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
        self.fetch_src = group_src.or_else(|| self.current_shop_track().map(|r| r.id.clone()));
        self.status.info(format!("downloading {what} …"));
        self.fetch = FetchState::Running {
            since: Instant::now(),
            what,
        };
        true
    }

    /// Record a pending old→new pairing for a downloaded file.
    ///
    /// Returns the queued entry id. `None` when there was no source track
    /// selected, which is a legitimate "just download it" case.
    fn queue_transfer_for(&mut self, paths: &[std::path::PathBuf]) -> Option<i64> {
        let src_id = self.fetch_src.take()?;
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

    pub fn focused_column_mut(&mut self) -> &mut ColumnState {
        match self.focus {
            Focus::Src => &mut self.src,
            Focus::Dst => &mut self.dst,
        }
    }
    pub fn focused_column(&self) -> &ColumnState {
        match self.focus {
            Focus::Src => &self.src,
            Focus::Dst => &self.dst,
        }
    }

    pub fn current_src(&self) -> Option<&TrackRow> {
        self.src
            .visible
            .get(self.src.cursor)
            .and_then(|&i| self.rows.get(i))
    }
    pub fn current_dst(&self) -> Option<&TrackRow> {
        self.dst
            .visible
            .get(self.dst.cursor)
            .and_then(|&i| self.rows.get(i))
    }
    pub fn current_shop_track(&self) -> Option<&TrackRow> {
        self.shop_list
            .visible
            .get(self.shop_list.cursor)
            .and_then(|&i| self.rows.get(i))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acquire::shop::{GroupOutcome, QuerySpec, SearchOutcome};
    use crate::acquire::types::{BackendId, ItemKind, ItemRef, Offer, SearchQuery};

    fn spec(src_id: &str) -> QuerySpec {
        QuerySpec {
            label: format!("track {src_id}"),
            src_id: Some(src_id.to_string()),
            query: SearchQuery::from_text("x", 5),
        }
    }

    fn group(src_id: &str, offers: usize) -> GroupOutcome {
        let ranked = (0..offers)
            .map(|i| crate::acquire::shop::RankedOffer {
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
        let (rows, total) = basket_rows(&library(), &basket(&["1", "3", "4"]), 25);
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
            let (rows, _) = basket_rows(&library(), &picked, 25);
            assert_eq!(
                rows.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
                ["1", "2", "4"]
            );
        }
    }

    #[test]
    fn the_cap_limits_what_is_queued_but_reports_the_whole_basket() {
        let (rows, total) = basket_rows(&library(), &basket(&["1", "2", "3", "4"]), 2);
        assert_eq!(rows.len(), 2);
        assert_eq!(total, 4, "the caller needs the real total to warn with");
    }

    #[test]
    fn an_id_no_longer_in_the_library_is_ignored() {
        let (rows, total) = basket_rows(&library(), &basket(&["2", "999"]), 25);
        assert_eq!(rows.len(), 1);
        assert_eq!(total, 1);
        assert_eq!(rows[0].id, "2");
    }

    #[test]
    fn a_filter_hiding_a_selection_is_counted_not_lost() {
        let rows = library();
        let mut col = ColumnState {
            selected: basket(&["1", "4"]),
            ..ColumnState::default()
        };
        // Everything visible: nothing hidden.
        col.visible = vec![0, 1, 2, 3];
        assert_eq!(hidden_selected(&rows, &col), 0);
        // A filter matching only "alpha" hides the other pick.
        col.visible = vec![0];
        assert_eq!(hidden_selected(&rows, &col), 1);
        // A filter matching neither hides both, and the selection still stands.
        col.visible = vec![1, 2];
        assert_eq!(hidden_selected(&rows, &col), 2);
        assert_eq!(col.selected.len(), 2, "counting must not mutate anything");
    }

    #[test]
    fn the_track_tag_tells_found_nothing_apart_from_not_searched() {
        // Both would otherwise render as a blank tag, and the user could not tell
        // whether pressing 's' again would do anything.
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
        // So `s` can reopen a running search instead of refusing with no way back.
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
