//! The library session every frontend drives: the database, the background
//! worker, and the shop, download and queue state built on them.
//!
//! Frontends own what is on screen — focus, filters, selections — and pass what
//! they mean by id. A gate that guards a write lives here, so no frontend can
//! reach the write without it.

pub mod confirm;
pub mod diff;
pub mod filter;
pub mod queue;
pub mod shop;
pub mod transfer;
pub mod view;
pub mod worker;

use std::collections::HashSet;
use std::time::Instant;

use crate::analysis::CopyOpts;
use crate::db::{MasterDb, SafetyOpts, rekordbox_running};
use crate::library::{TrackRow, load_rows};

pub use confirm::ConfirmKind;
use confirm::Confirmations;
use queue::QueueState;
use shop::{FetchQueue, FetchState, ShopState};

#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusLevel {
    #[default]
    Info,
    Warn,
    Err,
    Ok,
}

/// The next step a message points at. Text never names a key or a button;
/// each frontend says how to take the step in its own terms.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NoticeAction {
    OpenBuyPage {
        src_id: Option<String>,
        item: crate::acquire::ItemRef,
    },
    Research {
        src_id: String,
    },
    FillBasket,
    NextBasketBatch {
        cap: usize,
    },
    ImportDownloads,
    CheckDownloads,
    ForceEntry {
        entry_id: i64,
    },
    ForgetEntry {
        entry_id: i64,
    },
    Review(ConfirmKind),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    /// Increasing, so a frontend can ask for everything after the last it saw.
    pub id: u64,
    pub level: StatusLevel,
    pub text: String,
    pub action: Option<NoticeAction>,
}

/// Notices kept for a frontend that shows a history.
const NOTICE_HISTORY: usize = 50;

/// What the session last said, and what it said before that.
#[derive(Default)]
pub struct StatusLine {
    pub text: String,
    pub level: StatusLevel,
    pub action: Option<NoticeAction>,
    history: std::collections::VecDeque<Notice>,
    next_id: u64,
}

impl StatusLine {
    pub fn info(&mut self, msg: impl Into<String>) {
        self.say(StatusLevel::Info, msg.into());
    }
    pub fn ok(&mut self, msg: impl Into<String>) {
        self.say(StatusLevel::Ok, msg.into());
    }
    pub fn warn(&mut self, msg: impl Into<String>) {
        self.say(StatusLevel::Warn, msg.into());
    }
    pub fn err(&mut self, msg: impl Into<String>) {
        self.say(StatusLevel::Err, msg.into());
    }

    /// Attach the next step to what was just said.
    pub fn act(&mut self, action: NoticeAction) {
        self.action = Some(action.clone());
        if let Some(last) = self.history.back_mut() {
            last.action = Some(action);
        }
    }

    /// Reword what was just said, rather than saying it twice.
    pub fn amend(&mut self, level: StatusLevel, text: impl Into<String>) {
        self.text = text.into();
        self.level = level;
        if let Some(last) = self.history.back_mut() {
            last.text = self.text.clone();
            last.level = level;
        }
    }

    /// Everything said after notice `after`, oldest first.
    pub fn since(&self, after: u64) -> impl Iterator<Item = &Notice> {
        self.history.iter().filter(move |n| n.id > after)
    }

    fn say(&mut self, level: StatusLevel, text: String) {
        self.next_id += 1;
        self.text = text.clone();
        self.level = level;
        self.action = None;
        if self.history.len() == NOTICE_HISTORY {
            self.history.pop_front();
        }
        self.history.push_back(Notice {
            id: self.next_id,
            level,
            text,
            action: None,
        });
    }
}

pub struct Session {
    pub db: MasterDb,
    pub safety: SafetyOpts,

    pub rows: Vec<TrackRow>,
    /// Bumped whenever `rows` is replaced, so a frontend holding a copy knows
    /// to fetch it again.
    pub rows_revision: u64,
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

    /// Confirmations waiting on a yes, each with the batch it would write.
    pub confirmations: Confirmations,
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
            rows_revision: 0,
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
            confirmations: Confirmations::default(),
        };
        session
            .status
            .info(format!("Loaded {} tracks.", session.rows.len()));
        Ok(session)
    }

    pub fn reload_rows(&mut self) -> anyhow::Result<()> {
        self.rows = load_rows(&self.db)?;
        self.rows_revision += 1;
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

    /// The newest open confirmation, which a modal frontend shows.
    pub fn confirm(&self) -> Option<ConfirmKind> {
        self.confirmations.newest()
    }

    /// Close one confirmation without writing anything.
    pub fn cancel(&mut self, kind: ConfirmKind) {
        self.confirmations.take(kind);
    }

    /// Say yes to one confirmation. True when its writes were attempted, rather
    /// than refused or not open.
    pub fn accept(&mut self, kind: ConfirmKind) -> bool {
        if !self.confirmations.is_open(kind) {
            return false;
        }
        match kind {
            ConfirmKind::Transfer => self.apply_pending(),
            ConfirmKind::ImportRows => self.apply_import_batch(),
            ConfirmKind::ForceApply => self.apply_force_batch(),
            ConfirmKind::ClearQueue => self.clear_queue(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_message_is_kept_with_its_action() {
        let mut s = StatusLine::default();
        s.info("searching");
        s.warn("you don't own this yet.");
        s.act(NoticeAction::FillBasket);
        let all: Vec<&Notice> = s.since(0).collect();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].action, None);
        assert_eq!(all[1].action, Some(NoticeAction::FillBasket));
        assert_eq!(s.action, Some(NoticeAction::FillBasket));
        // A new message does not inherit the last one's step.
        s.ok("done");
        assert_eq!(s.action, None);
    }

    #[test]
    fn amending_rewords_the_last_message_rather_than_repeating_it() {
        let mut s = StatusLine::default();
        s.info("downloaded 1 file(s)");
        s.amend(StatusLevel::Info, "downloaded 1 file(s) 2 still queued.");
        let all: Vec<&Notice> = s.since(0).collect();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].text, s.text);
    }

    #[test]
    fn history_is_capped_and_readable_from_any_point() {
        let mut s = StatusLine::default();
        for i in 0..NOTICE_HISTORY + 10 {
            s.info(format!("{i}"));
        }
        assert_eq!(s.since(0).count(), NOTICE_HISTORY);
        let last = s.since(0).last().unwrap().id;
        assert_eq!(s.since(last).count(), 0);
        assert_eq!(s.since(last - 3).count(), 3);
    }
}
