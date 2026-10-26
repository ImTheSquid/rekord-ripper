//! Moving downloads along without a key press per stage.
//!
//! Off unless a frontend asks for it. It takes the same steps a user would —
//! import, then the fingerprint gate — and stops wherever they would have to
//! decide: the import confirmation, a rejection, a failure, or importing turned
//! off in the config. Nothing here writes; the steps it starts do, through
//! their usual gates.

use std::path::Path;

use crate::pending::State;

use super::Session;
use super::confirm::ConfirmKind;
use super::queue::QueueState;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Idle,
    /// Probe the downloads rekordbox has no row for, then ask to create them.
    Import,
    /// Fingerprint the downloads that now have a row, then apply what passes.
    Check,
}

/// The next stage worth starting, if any.
///
/// Only one stage at a time, and never while work is in flight: both stages
/// share the worker, and a result from a superseded round is dropped.
pub fn next_step(queue: &QueueState, import_enabled: bool, import_waiting: bool) -> Step {
    if queue.any_in_flight() {
        return Step::Idle;
    }
    // Anything already decided waits for a person: a failure, or rows planned
    // and sitting in a confirmation.
    let untouched = |id: i64| queue.work_for(id).is_none();
    let waiting = queue
        .entries
        .iter()
        .filter(|e| e.state == State::AwaitingImport && untouched(e.id));
    let (mut needs_row, mut needs_check) = (false, false);
    for e in waiting {
        if queue.has_row(e.id) {
            needs_check = true;
        } else {
            needs_row = true;
        }
    }
    if needs_row && import_enabled && !import_waiting {
        Step::Import
    } else if needs_check {
        Step::Check
    } else {
        Step::Idle
    }
}

impl Session {
    pub fn set_auto_advance(&mut self, on: bool) {
        self.auto_advance = on;
        if on {
            self.reload_queue();
        }
    }

    /// Start whatever the queue is ready for. Called after every worker drain.
    pub(super) fn advance(&mut self) {
        if !self.auto_advance {
            return;
        }
        let step = next_step(
            &self.queue,
            self.cfg.import.insert_content_rows,
            self.confirmations.is_open(ConfirmKind::ImportRows),
        );
        match step {
            Step::Idle => {}
            Step::Import => self.start_import(),
            Step::Check => {
                self.queue.next_generation();
                // `has_row` is as of the last reload. When it was stale there is
                // nothing to submit, and a reload corrects it rather than this
                // trying again every tick.
                if !self.submit_next_gate() {
                    self.reload_queue();
                }
            }
        }
    }

    /// Queue `path` as a better copy of `src_id`, as a finished download is.
    pub fn queue_file(&mut self, src_id: &str, path: &Path) -> Option<i64> {
        let Some(title) = self.row(src_id).map(|r| r.title.clone()) else {
            self.status.err("that track is no longer in the library.");
            return None;
        };
        if !path.is_file() {
            self.status.err(format!("{} is not a file.", path.display()));
            return None;
        }
        let queued = self.queue_transfer_for(Some(src_id.to_string()), &[path.to_path_buf()]);
        match queued {
            Some(id) => {
                self.status.ok(format!("queued transfer #{id} for {title}."));
                self.after_queueing();
            }
            None => self
                .status
                .err(format!("could not queue {} — is it audio?", path.display())),
        }
        queued
    }

    /// A new entry exists in the store; the driver can only see it once the
    /// snapshot is re-read.
    pub(super) fn after_queueing(&mut self) {
        if self.auto_advance {
            self.reload_queue();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pending::Entry;
    use crate::session::queue::EntryWork;
    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::time::Instant;

    fn entry(id: i64, state: State) -> Entry {
        Entry {
            id,
            src_content_id: format!("src{id}"),
            src_uuid: format!("uuid{id}"),
            src_title: None,
            src_artist: None,
            acquired_path: PathBuf::from(format!("/tmp/{id}.flac")),
            acquired_size: 1,
            acquired_mtime_ns: 1,
            provider: None,
            replace: true,
            lock: false,
            state,
            dst_content_id: None,
            verdict: None,
            created_at: "now".into(),
            expires_at: "later".into(),
        }
    }

    fn queue(entries: Vec<Entry>, with_rows: &[i64]) -> QueueState {
        let mut q = QueueState::default();
        q.reload(entries);
        q.set_rows_present(with_rows.iter().copied().collect::<HashSet<_>>());
        q
    }

    #[test]
    fn a_fresh_download_is_imported_first() {
        let q = queue(vec![entry(1, State::AwaitingImport)], &[]);
        assert_eq!(next_step(&q, true, false), Step::Import);
    }

    #[test]
    fn an_imported_download_is_checked() {
        let q = queue(vec![entry(1, State::AwaitingImport)], &[1]);
        assert_eq!(next_step(&q, true, false), Step::Check);
    }

    #[test]
    fn importing_waits_for_rows_already_awaiting_a_yes() {
        // A second import would replace the confirmation being read.
        let q = queue(vec![entry(1, State::AwaitingImport), entry(2, State::AwaitingImport)], &[2]);
        assert_eq!(next_step(&q, true, true), Step::Check, "the imported one can still go");
        let q = queue(vec![entry(1, State::AwaitingImport)], &[]);
        assert_eq!(next_step(&q, true, true), Step::Idle);
    }

    #[test]
    fn importing_turned_off_in_the_config_stops_there() {
        let q = queue(vec![entry(1, State::AwaitingImport)], &[]);
        assert_eq!(next_step(&q, false, false), Step::Idle);
    }

    #[test]
    fn nothing_starts_while_work_is_in_flight() {
        let mut q = queue(vec![entry(1, State::AwaitingImport), entry(2, State::AwaitingImport)], &[2]);
        q.set_work(
            1,
            EntryWork::Probing {
                since: Instant::now(),
                generation: 1,
            },
        );
        assert_eq!(next_step(&q, true, false), Step::Idle);
    }

    #[test]
    fn a_failure_is_left_for_a_person() {
        // Retrying it every tick would hammer whatever failed.
        let mut q = queue(vec![entry(1, State::AwaitingImport)], &[1]);
        q.set_work(1, EntryWork::Failed("no".into()));
        assert_eq!(next_step(&q, true, false), Step::Idle);
    }

    #[test]
    fn settled_entries_are_left_alone() {
        let q = queue(
            vec![entry(1, State::Applied), entry(2, State::Rejected), entry(3, State::Expired)],
            &[1, 2, 3],
        );
        assert_eq!(next_step(&q, true, false), Step::Idle);
    }
}
