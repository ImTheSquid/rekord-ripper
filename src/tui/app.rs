use std::collections::HashSet;
use std::time::Instant;

use crate::analysis::{CopyOpts, Plan};
use crate::db::{MasterDb, SafetyOpts, rekordbox_running};

use super::data::{TrackRow, dst_visible, load_rows, src_visible};

pub const DURATION_TOL_SECS: i64 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Src,
    Dst,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputMode {
    Normal,
    Search(Focus),
    Confirm,
    Help,
    /// The offer table overlay, driven by the background worker.
    Shop,
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

#[derive(Clone, Copy, Default, Debug)]
pub struct DstFilters {
    pub auto: bool,
    pub fuzzy_from_src: bool,
}

#[derive(Clone, Debug)]
pub struct ColumnState {
    pub query: String,
    pub visible: Vec<usize>,
    pub cursor: usize,
    pub selected: HashSet<String>,
}

impl Default for ColumnState {
    fn default() -> Self {
        Self {
            query: String::new(),
            visible: Vec::new(),
            cursor: 0,
            selected: HashSet::new(),
        }
    }
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
                super::worker::Update::Failed(why) => {
                    self.status.err(format!("search failed: {why}"));
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

    /// What `s` does: show what is already there, or search if there is nothing.
    ///
    /// This is the fix for a dead end: `s` used to always start a fresh search,
    /// so a completed result you had stepped away from was thrown away, and while
    /// one was running `s` refused *without* reopening the overlay — leaving no
    /// way back to it at all.
    pub fn open_shop(&mut self) -> bool {
        let Some(row) = self.current_src().cloned() else {
            self.status.warn("no source track selected to search for.");
            return false;
        };

        // Already answered: show it, and put the cursor on its block.
        if let Some(i) = self.first_offer_index_for(&row.id) {
            self.mode = InputMode::Shop;
            if let ShopState::Results { cursor, .. } = &mut self.shop {
                *cursor = i;
            }
            self.status
                .info("already searched — 'r' re-runs just this track.");
            return true;
        }
        // Already in the queue: just show the overlay.
        if self.shop_queued.contains(&row.id) {
            self.mode = InputMode::Shop;
            self.status.info(format!(
                "queued — {} search(es) to go.",
                self.worker.as_ref().map(|w| w.outstanding()).unwrap_or(0)
            ));
            return true;
        }
        // Otherwise add it to the list, behind anything already running.
        self.enqueue_shop(&[row])
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
            self.mode = InputMode::Shop;
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

    /// Search for the highlighted source track, replacing its existing results.
    pub fn start_shop(&mut self) -> bool {
        let Some(row) = self.current_src().cloned() else {
            self.status.warn("no source track selected to search for.");
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

    /// Add every selected source track to the search queue.
    ///
    /// Selection rather than "everything visible": a filter can match hundreds of
    /// rows, and each track is a full fan-out across every backend.
    pub fn shop_selected(&mut self, cap: usize) -> bool {
        if self.src.selected.is_empty() {
            self.status
                .warn("nothing selected — press space on the source rows you want, then 'S'.");
            return false;
        }
        // Selection order is not meaningful, so follow the visible order.
        let rows: Vec<TrackRow> = self
            .src
            .visible
            .iter()
            .filter_map(|&i| self.rows.get(i))
            .filter(|r| self.src.selected.contains(&r.id))
            .cloned()
            .collect();

        let total = rows.len();
        let rows: Vec<TrackRow> = rows.into_iter().take(cap).collect();
        if total > rows.len() {
            self.status.warn(format!(
                "{total} selected; queueing the first {}.",
                rows.len()
            ));
        }
        self.enqueue_shop(&rows)
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
        self.mode = InputMode::Shop;
        true
    }

    /// Searches submitted but not yet answered.
    pub fn shop_outstanding(&self) -> usize {
        self.worker.as_ref().map(|w| w.outstanding()).unwrap_or(0)
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
        if worker.is_busy() {
            self.status.warn("something is already running.");
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
        self.fetch_src = group_src.or_else(|| self.current_src().map(|r| r.id.clone()));
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
