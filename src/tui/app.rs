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
    },
    Results {
        outcome: Box<crate::acquire::shop::SearchOutcome>,
        cursor: usize,
    },
    Failed(String),
}

impl ShopState {
    /// Rows currently shown, for cursor clamping.
    pub fn len(&self) -> usize {
        match self {
            Self::Results { outcome, .. } => outcome.offers.len(),
            _ => 0,
        }
    }

    pub fn move_cursor(&mut self, delta: isize) {
        if let Self::Results { outcome, cursor } = self {
            let n = outcome.offers.len();
            if n == 0 {
                *cursor = 0;
                return;
            }
            let next = (*cursor as isize + delta).clamp(0, n as isize - 1);
            *cursor = next as usize;
        }
    }

    pub fn selected(&self) -> Option<&crate::acquire::shop::RankedOffer> {
        match self {
            Self::Results { outcome, cursor } => outcome.offers.get(*cursor),
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
            cfg,
            pending: None,
            unresolved_errors: false,
            quit_pending: false,
            should_quit: false,
        };
        app.recompute_visible();
        app.status.info(format!("Loaded {} tracks.", app.rows.len()));
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
        self.dst.selected.retain(|id| existing.contains(id.as_str()));
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
                super::worker::Update::Finished(outcome) => {
                    let found = outcome.offers.len();
                    let failed: Vec<String> = outcome
                        .failures()
                        .filter_map(|r| r.error.as_ref().map(|e| format!("{}: {e}", r.backend)))
                        .collect();
                    self.shop = ShopState::Results { outcome, cursor: 0 };
                    match (found, failed.is_empty()) {
                        (0, true) => self.status.warn("no offers found."),
                        (0, false) => self.status.err(format!("no offers — {}", failed.join("; "))),
                        (n, true) => self.status.ok(format!("{n} offers.")),
                        // A partial table is still useful; say what is missing.
                        (n, false) => self
                            .status
                            .warn(format!("{n} offers, degraded — {}", failed.join("; "))),
                    }
                }
                super::worker::Update::Failed(why) => {
                    self.status.err(format!("search failed: {why}"));
                    self.shop = ShopState::Failed(why);
                }
            }
        }
    }

    /// Start a search for the highlighted source track.
    ///
    /// Returns false when it could not be started, so the caller can explain why
    /// rather than opening an overlay that will never fill in.
    pub fn start_shop(&mut self) -> bool {
        let Some(track) = self.current_src() else {
            self.status.warn("no source track selected to search for.");
            return false;
        };
        let title = track.title.trim().to_string();
        if title.is_empty() {
            self.status.warn("that track has no title to search for.");
            return false;
        }
        // TrackRow stores these as plain Strings, with empty meaning absent.
        let artist = Some(track.artist.trim().to_string()).filter(|a| !a.is_empty());
        let what = format!("{} — {title}", artist.as_deref().unwrap_or("?"));

        let query = crate::acquire::types::SearchQuery {
            title,
            artist,
            duration_secs: track.length,
            limit: self.cfg.search.limit,
            ..Default::default()
        };
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
        if worker.is_busy() {
            self.status.warn("a search is already running.");
            return false;
        }
        if !worker.submit(super::worker::Job::Shop {
            query,
            opts: Box::new(opts),
        }) {
            self.status.err("could not start the search.");
            return false;
        }

        self.status.info(format!("searching for {what} …"));
        self.shop = ShopState::Searching {
            since: Instant::now(),
            what,
        };
        self.mode = InputMode::Shop;
        true
    }

    pub fn shop_busy(&self) -> bool {
        self.worker.as_ref().map(|w| w.is_busy()).unwrap_or(false)
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
