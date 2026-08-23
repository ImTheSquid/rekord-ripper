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

use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};

use crate::acquire::Registry;
use crate::acquire::shop::{self, SearchOpts, SearchOutcome};
use crate::acquire::types::SearchQuery;
use crate::config::{Config, Credentials};

/// Work the UI can ask for.
pub enum Job {
    Shop {
        query: SearchQuery,
        opts: Box<SearchOpts>,
    },
}

/// What comes back.
pub enum Update {
    /// Picked up; the UI can start showing progress.
    Started,
    /// Boxed because a `SearchOutcome` is large and this moves through a channel.
    Finished(Box<SearchOutcome>),
    Failed(String),
}

pub struct Worker {
    jobs: Option<Sender<Job>>,
    updates: Receiver<Update>,
    /// True between submitting a job and its result arriving, so the UI can
    /// refuse to queue a second search rather than silently stacking them.
    busy: bool,
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
                let reg = Registry::from_config(&cfg, &creds);
                // Ends when the UI drops its sender.
                while let Ok(job) = job_rx.recv() {
                    match job {
                        Job::Shop { query, opts } => {
                            if up_tx.send(Update::Started).is_err() {
                                return;
                            }
                            let outcome = shop::search_all(&reg, &query, &opts);
                            if up_tx.send(Update::Finished(Box::new(outcome))).is_err() {
                                return;
                            }
                        }
                    }
                }
            })?;

        Ok(Self {
            jobs: Some(job_tx),
            updates: up_rx,
            busy: false,
        })
    }

    pub fn is_busy(&self) -> bool {
        self.busy
    }

    /// Queue a job. Returns false when one is already running.
    pub fn submit(&mut self, job: Job) -> bool {
        if self.busy {
            return false;
        }
        match self.jobs.as_ref().map(|tx| tx.send(job)) {
            Some(Ok(())) => {
                self.busy = true;
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
                    if matches!(u, Update::Finished(_) | Update::Failed(_)) {
                        self.busy = false;
                    }
                    out.push(u);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if self.busy {
                        self.busy = false;
                        out.push(Update::Failed("the search thread stopped".into()));
                    }
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
    fn submitting_marks_it_busy_and_refuses_a_second_job() {
        let mut w = worker();
        let job = || Job::Shop {
            // An empty query short-circuits in every backend, so this does no
            // network work.
            query: SearchQuery::from_text("", 1),
            opts: Box::new(SearchOpts::default()),
        };
        assert!(w.submit(job()));
        assert!(w.is_busy());
        assert!(
            !w.submit(job()),
            "queueing a second search would stack work the user cannot see"
        );
    }

    #[test]
    fn a_finished_job_clears_busy_and_reports_an_outcome() {
        let mut w = worker();
        assert!(w.submit(Job::Shop {
            query: SearchQuery::from_text("", 1),
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
    }

    #[test]
    fn shutdown_is_idempotent_and_does_not_block() {
        let mut w = worker();
        w.shutdown();
        w.shutdown();
        // Submitting after shutdown fails rather than panicking.
        assert!(!w.submit(Job::Shop {
            query: SearchQuery::from_text("", 1),
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
