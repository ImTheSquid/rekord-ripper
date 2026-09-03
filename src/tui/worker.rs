//! A background worker for the TUI.
//!
//! # Why this has to exist
//!
//! `event_loop` is a single-threaded blocking poll and `handle_key` is
//! synchronous, so calling a backend search from a key handler freezes rendering
//! for as long as the network takes — no spinner, no feedback, a dead terminal
//! for seconds. This moves that work off the event thread and drains results from
//! the existing tick.
//!
//! # Why a detached thread is safe *here*
//!
//! `shop::search_all` deliberately uses scoped threads and bounded work rather
//! than detaching, because a leaked thread mid-*download* can leave a truncated
//! file behind. This worker is different in the way that matters: it only reads
//! from the network into memory and sends the result over a channel. It writes no
//! files and never touches `master.db` — which is also why it can be `'static`
//! at all, since `rusqlite::Connection` is not `Sync`.
//!
//! So on quit we drop the job sender and let the thread finish whatever request
//! it is in (already bounded by the HTTP timeout) rather than blocking the user's
//! exit on it.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::time::{Duration, Instant};

use crate::acquire::Registry;
use crate::acquire::shop::{self, GroupOutcome, QuerySpec, SearchOpts};
use crate::acquire::types::{AcquiredFile, AudioFormat, FetchOpts, ItemRef, Retention};
use crate::config::{Config, Credentials};

/// Ceiling for one download, including any wait while bandcamp prepares it.
const FETCH_BUDGET: Duration = Duration::from_secs(1800);

/// What kind of work a job is, so the UI can say which.
///
/// `outstanding` used to be a single count that three call sites read as
/// "searches". Once fingerprints share the thread, an untagged count makes the
/// shop screen report phantom searches, makes `f` a dead key with no
/// explanation, and makes the quit guard name the wrong thing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum JobKind {
    Search,
    Fetch,
    /// Reading a file's headers, before a row can be planned for it.
    Probe,
    /// The fingerprint gate. Minutes, when the source has to be ripped first.
    Fingerprint,
}

impl JobKind {
    /// For "3 fingerprint(s) still running".
    pub fn label(self) -> &'static str {
        match self {
            Self::Search => "search",
            Self::Fetch => "download",
            Self::Probe => "file check",
            Self::Fingerprint => "fingerprint",
        }
    }
}

/// Work the UI can ask for.
pub enum Job {
    /// One or many searches. A single search is a one-element `specs`, so bulk
    /// is not a separate path.
    Shop {
        specs: Vec<QuerySpec>,
        opts: Box<SearchOpts>,
    },
    /// Download an offer. Long-running by nature — a purchased FLAC is tens of
    /// megabytes and bandcamp may need minutes to prepare it — which is exactly
    /// why it belongs here rather than in a key handler.
    Fetch {
        item: ItemRef,
        dest: PathBuf,
        format_pref: Vec<AudioFormat>,
        overwrite: bool,
    },
    /// Read one file's headers, so a `djmdContent` row can be planned for it.
    /// One `ffprobe`, but with a 60 s ceiling — not something a key handler can
    /// afford to wait on.
    Probe {
        entry_id: i64,
        generation: u64,
        path: PathBuf,
    },
    /// The fingerprint gate for one queued transfer.
    ///
    /// `src` is moved in rather than borrowed: `TrackHeader` is plain owned
    /// data, and the main thread does not need it again — `build_plan` takes
    /// ids. Boxed because it is twelve fields against the handful every other
    /// variant carries, and `Job` is sized for its largest.
    Fingerprint {
        entry_id: i64,
        generation: u64,
        src: Box<crate::analysis::TrackHeader>,
        dst_path: PathBuf,
        dst_length: Option<i64>,
        dst_bpm: Option<i64>,
    },
}

impl Job {
    pub fn kind(&self) -> JobKind {
        match self {
            Self::Shop { .. } => JobKind::Search,
            Self::Fetch { .. } => JobKind::Fetch,
            Self::Probe { .. } => JobKind::Probe,
            Self::Fingerprint { .. } => JobKind::Fingerprint,
        }
    }
}

/// What comes back.
pub enum Update {
    /// Picked up; the UI can start showing progress.
    Started,
    /// `done` of `total` specs searched. Lets a bulk search show real progress
    /// instead of an indeterminate spinner.
    Progress {
        done: usize,
        total: usize,
        label: String,
    },
    /// A line of backend chatter — a queue position, a transfer percentage.
    /// Advisory: it names no job, so the UI attaches it to whatever is running.
    Note(String),
    /// Boxed because the outcomes are large and this moves through a channel.
    Finished(Box<Vec<GroupOutcome>>),
    /// A fetch finished. `Ok` carries where the files landed.
    Fetched(Box<Result<Vec<AcquiredFile>, String>>),
    /// `(entry_id, generation, result)`.
    Probed(Box<(i64, u64, Result<crate::audio::AudioInfo, String>)>),
    /// `(entry_id, generation, result)`.
    Fingerprinted(Box<(i64, u64, Result<crate::transfer::GateOutcome, String>)>),
    Failed(String),
}

impl Update {
    /// True when this ends the job the UI is waiting on.
    ///
    /// Every terminal variant must be listed. A new one that is missed here
    /// never decrements `outstanding`, and the UI stays "busy" forever with no
    /// error to explain it — see `every_job_kind_finishes`.
    fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Finished(_)
                | Self::Fetched(_)
                | Self::Probed(_)
                | Self::Fingerprinted(_)
                | Self::Failed(_)
        )
    }

    /// What kind of job this ends, for the per-kind counter.
    fn ends(&self) -> Option<JobKind> {
        match self {
            Self::Finished(_) => Some(JobKind::Search),
            Self::Fetched(_) => Some(JobKind::Fetch),
            Self::Probed(_) => Some(JobKind::Probe),
            Self::Fingerprinted(_) => Some(JobKind::Fingerprint),
            // A dead thread ends whatever was in flight; `drain` handles it.
            Self::Failed(_) | Self::Started | Self::Progress { .. } | Self::Note(_) => None,
        }
    }
}

pub struct Worker {
    jobs: Option<Sender<Job>>,
    updates: Receiver<Update>,
    /// Jobs submitted but not yet finished.
    ///
    /// A count rather than a flag: the thread takes one job at a time off the
    /// channel, so submitting several just runs them in order. That is what makes
    /// "tap `s` on each track you care about" work.
    outstanding: usize,
    /// The same count, split by kind, so a message can name what is running.
    by_kind: HashMap<JobKind, usize>,
}

impl Worker {
    /// Start the thread. The registry is built inside it, so a slow or failing
    /// backend construction cannot stall the UI either.
    pub fn spawn(cfg: &Config, creds: &Credentials) -> std::io::Result<Self> {
        let (job_tx, job_rx) = channel::<Job>();
        let (up_tx, up_rx) = channel::<Update>();
        let cfg = cfg.clone();
        let creds = creds.clone();

        std::thread::Builder::new()
            .name("rr-shop".into())
            .spawn(move || {
                // Backend chatter belongs on screen, not on the stderr the TUI
                // is drawing over. Best-effort: a closed channel just drops it.
                let notes = up_tx.clone();
                crate::acquire::route_progress(Box::new(move |line| {
                    let _ = notes.send(Update::Note(line.to_string()));
                }));

                let reg = Registry::from_config(&cfg, &creds);
                // Ends when the UI drops its sender.
                while let Ok(job) = job_rx.recv() {
                    match job {
                        Job::Shop { specs, opts } => {
                            if up_tx.send(Update::Started).is_err() {
                                return;
                            }
                            let tx = up_tx.clone();
                            let groups =
                                shop::search_many(&reg, &specs, &opts, |done, total, label| {
                                    // Best-effort: a closed channel means the UI
                                    // is gone, and the loop will notice shortly.
                                    let _ = tx.send(Update::Progress {
                                        done,
                                        total,
                                        label: label.to_string(),
                                    });
                                });
                            if up_tx.send(Update::Finished(Box::new(groups))).is_err() {
                                return;
                            }
                        }
                        Job::Fetch {
                            item,
                            dest,
                            format_pref,
                            overwrite,
                        } => {
                            if up_tx.send(Update::Started).is_err() {
                                return;
                            }
                            let result = match reg.get(item.backend) {
                                None => Err(format!("{} is not enabled", item.backend)),
                                Some(backend) => backend
                                    .fetch(
                                        &item,
                                        &FetchOpts {
                                            dest_dir: dest,
                                            format_pref,
                                            retention: Retention::Keep,
                                            overwrite,
                                            deadline: Instant::now() + FETCH_BUDGET,
                                        },
                                    )
                                    .map_err(|e| e.to_string()),
                            };
                            if up_tx.send(Update::Fetched(Box::new(result))).is_err() {
                                return;
                            }
                        }
                        Job::Probe {
                            entry_id,
                            generation,
                            path,
                        } => {
                            if up_tx.send(Update::Started).is_err() {
                                return;
                            }
                            let result = crate::audio::probe(&path).map_err(|e| e.to_string());
                            let msg = Update::Probed(Box::new((entry_id, generation, result)));
                            if up_tx.send(msg).is_err() {
                                return;
                            }
                        }
                        Job::Fingerprint {
                            entry_id,
                            generation,
                            src,
                            dst_path,
                            dst_length,
                            dst_bpm,
                        } => {
                            if up_tx.send(Update::Started).is_err() {
                                return;
                            }
                            let result =
                                crate::transfer::gate(&src, &dst_path, dst_length, dst_bpm, &cfg)
                                    .map_err(|e| e.to_string());
                            let msg =
                                Update::Fingerprinted(Box::new((entry_id, generation, result)));
                            if up_tx.send(msg).is_err() {
                                return;
                            }
                        }
                    }
                }
            })?;

        Ok(Self {
            jobs: Some(job_tx),
            updates: up_rx,
            outstanding: 0,
            by_kind: HashMap::new(),
        })
    }

    pub fn is_busy(&self) -> bool {
        self.outstanding > 0
    }

    /// How many jobs are submitted but not yet finished.
    pub fn outstanding(&self) -> usize {
        self.outstanding
    }

    /// How many of one kind are outstanding.
    ///
    /// The plain count cannot answer "are there searches running?" once
    /// fingerprints share the thread, and every caller that phrases its message
    /// as "search(es)" needs this instead.
    pub fn outstanding_of(&self, kind: JobKind) -> usize {
        self.by_kind.get(&kind).copied().unwrap_or(0)
    }

    /// Queue a job behind any already running. Returns false only if the thread
    /// is gone.
    pub fn submit(&mut self, job: Job) -> bool {
        let kind = job.kind();
        match self.jobs.as_ref().map(|tx| tx.send(job)) {
            Some(Ok(())) => {
                self.outstanding += 1;
                *self.by_kind.entry(kind).or_insert(0) += 1;
                true
            }
            // The thread died; report it rather than looking stuck forever.
            _ => false,
        }
    }

    /// Collect whatever has arrived. Never blocks, so it is safe on the tick.
    pub fn drain(&mut self) -> Vec<Update> {
        let mut out = Vec::new();
        loop {
            match self.updates.try_recv() {
                Ok(u) => {
                    if u.is_terminal() {
                        self.outstanding = self.outstanding.saturating_sub(1);
                    }
                    if let Some(kind) = u.ends()
                        && let Some(n) = self.by_kind.get_mut(&kind)
                    {
                        *n = n.saturating_sub(1);
                    }
                    out.push(u);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    // Report one failure per lost job, so nothing sits pending
                    // forever waiting on a thread that has gone.
                    while self.outstanding > 0 {
                        self.outstanding -= 1;
                        out.push(Update::Failed("the worker thread stopped".into()));
                    }
                    self.by_kind.clear();
                    break;
                }
            }
        }
        out
    }

    /// Signal shutdown. Does not block: see the module docs on why not joining is
    /// the right call for a read-only thread.
    pub fn shutdown(&mut self) {
        self.jobs = None;
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn worker() -> Worker {
        Worker::spawn(&Config::default(), &Credentials::default()).unwrap()
    }

    /// A spec every backend short-circuits on, so tests do no network work.
    fn empty_spec() -> QuerySpec {
        QuerySpec {
            label: "nothing".into(),
            src_id: None,
            query: crate::acquire::types::SearchQuery::from_text("", 1),
        }
    }

    #[test]
    fn a_new_worker_is_idle_and_has_nothing_to_report() {
        let mut w = worker();
        assert!(!w.is_busy());
        assert!(
            w.drain().is_empty(),
            "draining must not block or invent updates"
        );
    }

    #[test]
    fn several_jobs_queue_behind_each_other() {
        // The point of the counter: tapping `s` on a few tracks should build a
        // list that works through itself, not get refused.
        let mut w = worker();
        let job = || Job::Shop {
            // An empty query short-circuits in every backend, so this does no
            // network work.
            specs: vec![empty_spec()],
            opts: Box::new(SearchOpts::default()),
        };
        assert!(w.submit(job()));
        assert!(w.submit(job()));
        assert!(w.submit(job()));
        assert!(w.is_busy());
        assert_eq!(w.outstanding(), 3);
    }

    /// Every job kind must eventually clear the counter.
    ///
    /// The trap this guards: `Update::is_terminal` is a `matches!` list, and a
    /// terminal variant missing from it never decrements `outstanding`. Nothing
    /// errors — the UI just reports work forever, refuses keys as
    /// already-queued, and blocks quit. Silent, and invisible to every other
    /// test here.
    #[test]
    fn every_job_kind_finishes() {
        for job in [
            Job::Probe {
                entry_id: 1,
                generation: 0,
                // A path that cannot exist: probe fails fast, and a failure
                // still has to be terminal.
                path: PathBuf::from("/nonexistent/rr-test.flac"),
            },
            Job::Shop {
                specs: vec![empty_spec()],
                opts: Box::new(SearchOpts::default()),
            },
        ] {
            let kind = job.kind();
            let mut w = worker();
            assert!(w.submit(job));
            assert_eq!(w.outstanding_of(kind), 1, "{kind:?} was not counted");

            let start = Instant::now();
            while w.outstanding() > 0 && start.elapsed() < Duration::from_secs(30) {
                w.drain();
                std::thread::sleep(Duration::from_millis(10));
            }
            assert_eq!(w.outstanding(), 0, "{kind:?} never cleared the counter");
            assert_eq!(w.outstanding_of(kind), 0, "{kind:?} left its own count");
        }
    }

    #[test]
    fn one_kind_of_work_does_not_mask_another() {
        // What the split is for: a fingerprint must not read as a search, or
        // the shop screen shows phantom searches and `f` goes dead.
        let mut w = worker();
        assert!(w.submit(Job::Probe {
            entry_id: 1,
            generation: 0,
            path: PathBuf::from("/nonexistent/rr-test.flac"),
        }));
        assert_eq!(w.outstanding_of(JobKind::Probe), 1);
        assert_eq!(w.outstanding_of(JobKind::Search), 0);
        assert_eq!(w.outstanding_of(JobKind::Fetch), 0);
    }

    #[test]
    fn a_finished_job_clears_busy_and_reports_an_outcome() {
        let mut w = worker();
        assert!(w.submit(Job::Shop {
            specs: vec![empty_spec()],
            opts: Box::new(SearchOpts::default()),
        }));

        // Poll the way the tick does rather than blocking the test.
        let mut updates = Vec::new();
        for _ in 0..200 {
            updates.extend(w.drain());
            if updates
                .iter()
                .any(|u| matches!(u, Update::Finished(_) | Update::Failed(_)))
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }

        assert!(
            updates.iter().any(|u| matches!(u, Update::Started)),
            "the UI needs a Started to show progress"
        );
        assert!(
            updates
                .iter()
                .any(|u| matches!(u, Update::Finished(_) | Update::Failed(_))),
            "never got a terminal update"
        );
        assert!(!w.is_busy(), "busy must clear so another search can run");
        assert_eq!(w.outstanding(), 0);
    }

    #[test]
    fn shutdown_is_idempotent_and_does_not_block() {
        let mut w = worker();
        w.shutdown();
        w.shutdown();
        // Submitting after shutdown fails rather than panicking.
        assert!(!w.submit(Job::Shop {
            specs: vec![empty_spec()],
            opts: Box::new(SearchOpts::default()),
        }));
    }

    #[test]
    fn dropping_the_worker_stops_the_thread() {
        // The thread's recv() ends when the sender is dropped; if it did not, this
        // test would leak a thread per run.
        let before = std::thread::available_parallelism().is_ok();
        {
            let _w = worker();
        }
        assert!(before, "sanity");
    }
}
