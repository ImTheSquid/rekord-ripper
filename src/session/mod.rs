//! The library session every frontend drives: the database, the background
//! worker, and the shop, download and queue state built on them.
//!
//! Frontends own what is on screen — focus, filters, selections — and pass what
//! they mean by id. A gate that guards a write lives here, so no frontend can
//! reach the write without it.

pub mod diff;
pub mod filter;
pub mod queue;
pub mod shop;
pub mod transfer;
pub mod worker;

use std::collections::HashSet;
use std::time::Instant;

use crate::analysis::CopyOpts;
use crate::db::{MasterDb, SafetyOpts, rekordbox_running};
use crate::library::{TrackRow, load_rows};

use queue::{ForceBatch, ImportBatch, QueueState};
use shop::{FetchQueue, FetchState, ShopState};
use transfer::PendingBatch;

/// Which confirmation is open, and therefore what a yes means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfirmKind {
    /// The transfer screen's cp batch.
    Transfer,
    /// Creating `djmdContent` rows for queued downloads.
    ImportRows,
    /// Transferring one queued entry with the fingerprint gate skipped.
    ForceApply,
    /// Emptying the whole download queue.
    ClearQueue,
}

#[derive(Default)]
pub struct StatusLine {
    pub text: String,
    pub level: StatusLevel,
}

#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
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

pub struct Session {
    pub db: MasterDb,
    pub safety: SafetyOpts,

    pub rows: Vec<TrackRow>,
    pub rb_running: bool,
    rb_last_polled: Instant,

    pub copy_opts: CopyOpts,
    pub status: StatusLine,
    /// `None` when the worker thread could not be started; searching is then
    /// simply unavailable rather than the session failing to open.
    pub worker: Option<worker::Worker>,
    pub shop: ShopState,
    pub fetch: FetchState,
    /// Further downloads stack up behind the running one the way searches do.
    pub fetch_queue: FetchQueue,
    /// Offers whose file has landed, so a row keeps saying so after the queue
    /// drains. Failures are absent on purpose — see [`Session::offer_fetch`].
    fetched: HashSet<crate::acquire::ItemRef>,
    /// Source track ids submitted to the worker but not yet answered, so
    /// searching the same track twice does not search it twice.
    shop_queued: Vec<String>,
    pub cfg: crate::config::Config,
    pub unresolved_errors: bool,
    /// The download queue, and the store behind it.
    ///
    /// `None` means the queue is unavailable, not that the session failed to
    /// open — the same treatment `worker` gets. Held open rather than reopened
    /// per action: `open()` re-runs the schema setup and has nowhere to report
    /// a failure from inside an input handler.
    pub queue: QueueState,
    pub store: Option<crate::pending::PendingStore>,

    /// The open confirmation, if any. Its batch below is what a yes writes.
    pub confirm: Option<ConfirmKind>,
    pub pending: Option<PendingBatch>,
    /// The snapshot behind `ConfirmKind::ImportRows`.
    pub import_batch: Option<ImportBatch>,
    /// The snapshot behind `ConfirmKind::ForceApply`.
    pub force_batch: Option<ForceBatch>,
}

impl Session {
    /// Opens the session on rekordbox's own `master.db`.
    pub fn open(safety: SafetyOpts) -> anyhow::Result<Self> {
        Self::new(MasterDb::open()?, safety)
    }

    pub fn new(db: MasterDb, safety: SafetyOpts) -> anyhow::Result<Self> {
        let cfg_path = crate::paths::config_path(None)?;
        let cfg = crate::config::Config::load(&cfg_path).unwrap_or_default();
        let creds = crate::config::Credentials::load(&crate::paths::credentials_path()?)
            .unwrap_or_default();
        // A worker that will not start costs us searching, not the whole session.
        let worker = worker::Worker::spawn(&cfg, &creds).ok();

        let rows = load_rows(&db)?;
        let mut session = Session {
            db,
            safety,
            rows,
            rb_running: rekordbox_running(),
            rb_last_polled: Instant::now(),
            copy_opts: CopyOpts::default(),
            status: StatusLine::default(),
            worker,
            shop: ShopState::Idle,
            fetch: FetchState::Idle,
            fetch_queue: FetchQueue::default(),
            fetched: HashSet::new(),
            shop_queued: Vec::new(),
            cfg,
            unresolved_errors: false,
            queue: QueueState::default(),
            store: crate::pending::PendingStore::open().ok(),
            confirm: None,
            pending: None,
            import_batch: None,
            force_batch: None,
        };
        session
            .status
            .info(format!("Loaded {} tracks.", session.rows.len()));
        Ok(session)
    }

    pub fn reload_rows(&mut self) -> anyhow::Result<()> {
        self.rows = load_rows(&self.db)?;
        Ok(())
    }

    pub fn row(&self, id: &str) -> Option<&TrackRow> {
        self.rows.iter().find(|r| r.id == id)
    }

    pub fn poll_rekordbox_if_due(&mut self) {
        if self.rb_last_polled.elapsed() >= std::time::Duration::from_secs(1) {
            self.rb_running = rekordbox_running();
            self.rb_last_polled = Instant::now();
        }
    }

    /// Close whichever confirmation is open without writing anything.
    pub fn cancel_confirm(&mut self) {
        self.confirm = None;
        self.pending = None;
        self.import_batch = None;
        self.force_batch = None;
    }

    /// Say yes to the open confirmation.
    pub fn accept_confirm(&mut self) {
        match self.confirm {
            Some(ConfirmKind::Transfer) => {
                self.apply_pending();
            }
            Some(ConfirmKind::ImportRows) => self.apply_import_batch(),
            Some(ConfirmKind::ForceApply) => self.apply_force_batch(),
            Some(ConfirmKind::ClearQueue) => self.clear_queue(),
            None => {}
        }
    }

    /// Drain the worker. Called every tick; never blocks.
    pub fn pump_worker(&mut self) {
        let Some(worker) = self.worker.as_mut() else {
            return;
        };
        for update in worker.drain() {
            match update {
                worker::Update::Started => {}
                worker::Update::Progress {
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
                worker::Update::Note(line) => {
                    // Only a download has anywhere to put it; a warning raised
                    // during a search has no row of its own and is dropped.
                    if let FetchState::Running { note, .. } = &mut self.fetch {
                        *note = Some(line);
                    }
                }
                worker::Update::Finished(new_groups) => self.on_searched(*new_groups),
                worker::Update::Fetched(result) => self.on_fetched(*result),
                worker::Update::Probed(r) => {
                    let (entry_id, generation, result) = *r;
                    self.on_probed(entry_id, generation, result);
                }
                worker::Update::Fingerprinted(r) => {
                    let (entry_id, generation, result) = *r;
                    self.on_fingerprinted(entry_id, generation, result);
                }
                worker::Update::Failed(why) => {
                    self.status.err(format!("worker failed: {why}"));
                    // A dead thread answers nothing. Without this every queued
                    // row keeps its "working" tag forever and every action is
                    // refused as already-in-flight.
                    self.queue.abandon_in_flight(&why);
                    if matches!(self.fetch, FetchState::Running { .. }) {
                        self.fetch_queue.abandon_all();
                        self.fetch = FetchState::Failed(why);
                    } else {
                        self.shop = ShopState::Failed(why);
                    }
                }
            }
        }
    }

    /// Searches outstanding — not every job. Frontends phrase this count as
    /// "search(es)", so counting fingerprints here would make them lie.
    pub fn shop_outstanding(&self) -> usize {
        self.outstanding_of(worker::JobKind::Search)
    }

    pub fn outstanding_of(&self, kind: worker::JobKind) -> usize {
        self.worker
            .as_ref()
            .map(|w| w.outstanding_of(kind))
            .unwrap_or(0)
    }

    /// Everything in flight, for a quit guard.
    pub fn work_in_flight(&self) -> Vec<(worker::JobKind, usize)> {
        use worker::JobKind::*;
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
}
