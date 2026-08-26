//! The pending-transfer queue, as the TUI sees it.
//!
//! `entries` is a snapshot of the store. `work` is everything this screen has
//! learned since that snapshot and the store does not know yet — which makes one
//! map answer both "is this already running?" and "what did it come back with?".

use std::collections::HashMap;
use std::time::Instant;

use crate::pending::Entry;

/// What the screen has learned about one entry since the snapshot.
pub enum EntryWork {
    /// Reading the file's headers, so a row can be planned for it.
    Probing {
        since: Instant,
        generation: u64,
    },
    /// The probe came back; the row is planned but not written.
    Planned(Box<crate::import::NewContent>),
    Fingerprinting {
        since: Instant,
        generation: u64,
        /// Resolved when the job was submitted, and the plan is built against
        /// *this* id rather than a fresh lookup. Between submit and arrival the
        /// user can re-import, and `find_imported_row`'s filename+size fallback
        /// would happily name a different row — applying a verdict computed for
        /// one file to a plan built for another, with the only real check
        /// bypassed.
        dst_content_id: String,
    },
    /// The gate accepted. Held in memory rather than written back as `Matched`:
    /// quitting here would otherwise strand the entry in a state nothing moves
    /// it out of, invisible to both this screen and the CLI.
    Ready {
        plan: Box<crate::analysis::Plan>,
        verdict: crate::fingerprint::Verdict,
    },
    Failed(String),
}

impl EntryWork {
    /// True while the worker still owes an answer for this entry.
    pub fn in_flight(&self) -> bool {
        matches!(self, Self::Probing { .. } | Self::Fingerprinting { .. })
    }

    /// When the in-flight work started, for the spinner.
    pub fn since(&self) -> Option<Instant> {
        match self {
            Self::Probing { since, .. } | Self::Fingerprinting { since, .. } => Some(*since),
            _ => None,
        }
    }

    /// The short tag shown on the entry's second line.
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Probing { .. } => "reading the file",
            Self::Planned(_) => "ready to import",
            Self::Fingerprinting { .. } => "fingerprinting",
            Self::Ready { .. } => "ready to apply",
            Self::Failed(_) => "failed",
        }
    }
}

#[derive(Default)]
pub struct QueueState {
    /// Snapshot from `store.all()`, refreshed on open and after every write.
    pub entries: Vec<Entry>,
    pub cursor: usize,
    /// Bumped on every action. Results carrying a stale generation are dropped,
    /// which is what tells "press `a`, Esc, press `a`" apart from a retry.
    generation: u64,
    work: HashMap<i64, EntryWork>,
    /// Entries rekordbox now has a row for.
    ///
    /// The store's own `State` stays `AwaitingImport` until the transfer lands,
    /// so it cannot answer "does this file exist in the collection yet?" — and
    /// a row that had just been imported went on reading "awaiting import",
    /// which looked exactly like the import having done nothing. Resolved once
    /// per reload rather than per frame.
    has_row: std::collections::HashSet<i64>,
}

impl QueueState {
    /// Replace the snapshot, keeping only the work that still has an entry.
    ///
    /// Anything removed with `c`, or swept, takes its work with it — otherwise a
    /// reissued rowid (`remove` is a `DELETE`) would inherit it.
    pub fn reload(&mut self, entries: Vec<Entry>) {
        let live: std::collections::HashSet<i64> = entries.iter().map(|e| e.id).collect();
        self.work.retain(|id, _| live.contains(id));
        self.entries = entries;
        self.clamp_cursor();
    }

    pub fn clamp_cursor(&mut self) {
        if self.entries.is_empty() {
            self.cursor = 0;
        } else if self.cursor >= self.entries.len() {
            self.cursor = self.entries.len() - 1;
        }
    }

    pub fn move_by(&mut self, delta: isize) {
        if self.entries.is_empty() {
            self.cursor = 0;
            return;
        }
        let n = self.entries.len() as isize;
        self.cursor = (self.cursor as isize + delta).clamp(0, n - 1) as usize;
    }

    pub fn jump_top(&mut self) {
        self.cursor = 0;
    }

    pub fn jump_bottom(&mut self) {
        self.cursor = self.entries.len().saturating_sub(1);
    }

    pub fn selected(&self) -> Option<&Entry> {
        self.entries.get(self.cursor)
    }

    pub fn work_for(&self, id: i64) -> Option<&EntryWork> {
        self.work.get(&id)
    }

    /// Record which entries rekordbox has a row for.
    pub fn set_rows_present(&mut self, ids: std::collections::HashSet<i64>) {
        self.has_row = ids;
    }

    pub fn has_row(&self, id: i64) -> bool {
        self.has_row.contains(&id)
    }

    /// True when the worker already owes an answer for this entry, so a second
    /// key press is a no-op rather than a second job.
    pub fn in_flight(&self, id: i64) -> bool {
        self.work.get(&id).is_some_and(|w| w.in_flight())
    }

    pub fn any_in_flight(&self) -> bool {
        self.work.values().any(|w| w.in_flight())
    }

    /// Start a new round of work. Anything still in flight from an earlier
    /// round has its results discarded on arrival.
    pub fn next_generation(&mut self) -> u64 {
        self.generation += 1;
        self.generation
    }

    pub fn current_generation(&self) -> u64 {
        self.generation
    }

    pub fn set_work(&mut self, id: i64, work: EntryWork) {
        self.work.insert(id, work);
    }

    pub fn clear_work(&mut self, id: i64) {
        self.work.remove(&id);
    }

    /// Take the work out, so the caller owns what it carries.
    ///
    /// `Plan` is not `Clone`, so applying it means owning it — and taking it
    /// also clears the entry, which is the right thing on success.
    pub fn take_work(&mut self, id: i64) -> Option<EntryWork> {
        self.work.remove(&id)
    }

    /// Accept an arriving result only if it is for a live entry and the round
    /// that is still current.
    ///
    /// `entry_id` alone is not enough to dispatch on: it is a SQLite rowid and
    /// `remove` is a `DELETE`, so an id can be handed to a later insert.
    pub fn accepts(&self, id: i64, generation: u64) -> bool {
        generation == self.generation
            && self.entries.iter().any(|e| e.id == id)
            && self.work.get(&id).is_some_and(|w| w.in_flight())
    }

    /// The worker thread died, so nothing in flight will ever answer.
    ///
    /// Without this every row keeps its "fingerprinting" tag forever and every
    /// key is refused as already-queued.
    pub fn abandon_in_flight(&mut self, why: &str) {
        for work in self.work.values_mut() {
            if work.in_flight() {
                *work = EntryWork::Failed(why.to_string());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pending::State;
    use std::path::PathBuf;

    fn entry(id: i64) -> Entry {
        Entry {
            id,
            src_content_id: format!("src{id}"),
            src_uuid: format!("uuid{id}"),
            src_title: Some(format!("Track {id}")),
            src_artist: Some("Artist".into()),
            acquired_path: PathBuf::from(format!("/tmp/{id}.flac")),
            acquired_size: 1,
            acquired_mtime_ns: 1,
            provider: None,
            replace: true,
            lock: false,
            state: State::AwaitingImport,
            dst_content_id: None,
            verdict: None,
            created_at: "now".into(),
            expires_at: "later".into(),
        }
    }

    fn probing(generation: u64) -> EntryWork {
        EntryWork::Probing {
            since: Instant::now(),
            generation,
        }
    }

    #[test]
    fn the_cursor_clamps_at_both_ends_and_on_an_empty_queue() {
        let mut q = QueueState::default();
        q.move_by(1);
        assert_eq!(q.cursor, 0);
        assert!(q.selected().is_none());

        q.reload(vec![entry(1), entry(2), entry(3)]);
        q.move_by(-5);
        assert_eq!(q.cursor, 0);
        q.move_by(99);
        assert_eq!(q.cursor, 2);
        q.jump_top();
        assert_eq!(q.cursor, 0);
        q.jump_bottom();
        assert_eq!(q.cursor, 2);
        assert_eq!(q.selected().map(|e| e.id), Some(3));
    }

    #[test]
    fn a_shorter_reload_pulls_the_cursor_back_into_range() {
        let mut q = QueueState::default();
        q.reload(vec![entry(1), entry(2), entry(3)]);
        q.jump_bottom();
        q.reload(vec![entry(1)]);
        assert_eq!(q.cursor, 0);
    }

    #[test]
    fn work_does_not_outlive_its_entry() {
        // `remove` is a DELETE, so the rowid can come back on a later insert.
        // Work left behind would silently attach to whatever inherits the id.
        let mut q = QueueState::default();
        q.reload(vec![entry(1), entry(2)]);
        q.set_work(1, probing(1));
        q.set_work(2, probing(1));

        q.reload(vec![entry(2)]);
        assert!(q.work_for(1).is_none(), "dropped entry kept its work");
        assert!(q.work_for(2).is_some());
    }

    #[test]
    fn a_result_is_accepted_only_for_a_live_entry_in_the_current_round() {
        let mut q = QueueState::default();
        q.reload(vec![entry(1)]);
        let round = q.next_generation();
        q.set_work(1, probing(round));

        assert!(q.accepts(1, round));
        // A round that has been superseded — "press a, Esc, press a".
        assert!(!q.accepts(1, round - 1));
        // An entry that was never queued, or has already answered.
        assert!(!q.accepts(99, round));
        q.set_work(1, EntryWork::Failed("nope".into()));
        assert!(!q.accepts(1, round), "an answered entry accepts no more");
    }

    #[test]
    fn a_second_key_press_finds_the_entry_already_in_flight() {
        let mut q = QueueState::default();
        q.reload(vec![entry(1)]);
        assert!(!q.in_flight(1));
        q.set_work(1, probing(1));
        assert!(q.in_flight(1));
        assert!(q.any_in_flight());

        // A finished result is no longer in flight, so a retry is allowed.
        q.set_work(1, EntryWork::Failed("bad codec".into()));
        assert!(!q.in_flight(1));
        assert!(!q.any_in_flight());
    }

    #[test]
    fn a_dead_worker_releases_everything_it_was_carrying() {
        let mut q = QueueState::default();
        q.reload(vec![entry(1), entry(2)]);
        q.set_work(1, probing(1));
        q.set_work(2, EntryWork::Planned(Box::new(new_content())));

        q.abandon_in_flight("the worker thread stopped");
        assert!(!q.in_flight(1), "still waiting on a thread that is gone");
        assert!(matches!(q.work_for(1), Some(EntryWork::Failed(_))));
        // Work that had already answered is untouched.
        assert!(matches!(q.work_for(2), Some(EntryWork::Planned(_))));
    }

    fn new_content() -> crate::import::NewContent {
        crate::import::NewContent {
            id: "1".into(),
            uuid: "u".into(),
            folder_path: "/tmp/1.flac".into(),
            file_name: "1.flac".into(),
            title: "One".into(),
            artist_id: None,
            album_id: None,
            genre_id: None,
            new_artist: None,
            new_album: None,
            new_genre: None,
            comment: None,
            release_year: None,
            track_no: None,
            disc_no: None,
            length: 100,
            file_type: 5,
            file_size: 1,
            sample_rate: None,
            bit_depth: None,
            bit_rate: None,
            content_link: None,
            master_db_id: None,
            device_id: None,
        }
    }
}
