//! The pending-transfer queue, as a frontend sees it.
//!
//! `entries` is a snapshot of the store. `work` is everything this screen has
//! learned since that snapshot and the store does not know yet — which makes one
//! map answer both "is this already running?" and "what did it come back with?".

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use crate::analysis::CopyOpts;
use crate::pending::Entry;

use super::confirm::{Batch, ConfirmKind};
use super::{NoticeAction, Session, worker};

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
    has_row: HashSet<i64>,
}

impl QueueState {
    /// Replace the snapshot, keeping only the work that still has an entry.
    ///
    /// Anything removed with `c`, or swept, takes its work with it — otherwise a
    /// reissued rowid (`remove` is a `DELETE`) would inherit it.
    pub fn reload(&mut self, entries: Vec<Entry>) {
        let live: HashSet<i64> = entries.iter().map(|e| e.id).collect();
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
    pub fn set_rows_present(&mut self, ids: HashSet<i64>) {
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

/// Rows a confirmed import will write.
///
/// A snapshot, taken when the confirmation opens and never added to. The worker
/// is pumped on every tick regardless, so a probe landing mid-confirmation would
/// otherwise grow the batch between the user reading it and saying yes — which
/// is exactly the gate that exists to get a yes *having seen* the rows.
pub struct ImportBatch {
    pub rows: Vec<crate::import::NewContent>,
    /// The entries those rows belong to, in the same order.
    pub entry_ids: Vec<i64>,
}

/// The one entry behind `ConfirmKind::ForceApply`.
///
/// The plan is built when the confirmation opens, so the yes is given against
/// the actual writes — the same discipline as every other gate here, minus the
/// one check being waived.
pub struct ForceBatch {
    pub entry_id: i64,
    pub dst_content_id: String,
    pub plan: Box<crate::analysis::Plan>,
    pub verdict: crate::fingerprint::Verdict,
}

impl Session {
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
                let present: HashSet<i64> = entries
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

    fn entry(&self, id: i64) -> Option<Entry> {
        self.queue.entries.iter().find(|e| e.id == id).cloned()
    }

    /// Probe every queued download that rekordbox has no row for.
    ///
    /// Gate 1 is checked here rather than at the confirmation: probing a queue
    /// takes real time, and spending it only to refuse with "edit your config
    /// file" is not an instruction anyone can act on from inside an app.
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
                self.queue
                    .set_work(entry_id, EntryWork::Failed("file is gone".into()));
                continue;
            }
            let job = worker::Job::Probe {
                entry_id,
                generation,
                path,
            };
            if self.worker.as_mut().is_some_and(|w| w.submit(job)) {
                self.queue.set_work(
                    entry_id,
                    EntryWork::Probing {
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

    /// A probe came back: plan the row, and ask once all are in.
    pub(super) fn on_probed(
        &mut self,
        entry_id: i64,
        generation: u64,
        result: Result<crate::audio::AudioInfo, String>,
    ) {
        if !self.queue.accepts(entry_id, generation) {
            return;
        }
        let Some(entry) = self.entry(entry_id) else {
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
                .set_work(entry_id, EntryWork::Planned(Box::new(new))),
            Err(why) => self.queue.set_work(entry_id, EntryWork::Failed(why)),
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
            if let Some(EntryWork::Planned(new)) = self.queue.work_for(entry.id) {
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

        self.confirmations
            .open(Batch::ImportRows(ImportBatch { rows, entry_ids }));
    }

    /// Gates 2 and 3 are behind us: write the rows.
    pub fn apply_import_batch(&mut self) -> bool {
        let Some(Batch::ImportRows(batch)) = self.confirmations.take(ConfirmKind::ImportRows) else {
            return false;
        };

        if let Err(e) = crate::db::safety_preflight(self.safety) {
            self.status.err(format!("{e}"));
            return false;
        }
        let backup = match self.db.backup() {
            Ok(p) => p,
            Err(e) => {
                self.status
                    .err(format!("backup failed, nothing written: {e}"));
                return false;
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
                        .set_work(*entry_id, EntryWork::Failed(e.to_string()));
                }
            }
        }

        self.reload_queue();
        let total = batch.rows.len();
        if let Some(first) = failed.first() {
            self.unresolved_errors = true;
            self.status
                .err(format!("imported {done}/{total}. Failed → {first}"));
            return true;
        }
        self.status.ok(format!(
            "imported {done} row(s) — checking fingerprints now. Backup: {}",
            backup.display()
        ));
        // The rows exist only so the transfer can happen, so go straight on to
        // it rather than leaving the user on a screen that looks unchanged with
        // no word about what to do. The fingerprint is still the gate, and it
        // still fails closed.
        if done > 0 {
            self.start_apply();
        }
        true
    }

    /// Fingerprint the queued downloads that now have a row, one at a time.
    ///
    /// One at a time on purpose. A gate against a streaming source rips it
    /// before decoding, so ten entries could hold the single worker thread for
    /// the better part of an hour — and `Worker` has no way to drop queued
    /// jobs, so there would be no cancel short of quitting.
    pub fn start_apply(&mut self) {
        if self.queue.any_in_flight() {
            self.status.info("already working — give it a moment.");
            return;
        }
        self.queue.next_generation();
        if !self.submit_next_gate() {
            self.status
                .info("nothing to check — import the downloads first.");
            self.status.act(NoticeAction::ImportDownloads);
        }
    }

    /// Submit the next entry needing a verdict. False when there are none left.
    pub(super) fn submit_next_gate(&mut self) -> bool {
        let generation = self.queue.current_generation();
        let candidates: Vec<Entry> = self
            .queue
            .entries
            .iter()
            .filter(|e| e.state == crate::pending::State::AwaitingImport)
            .filter(|e| {
                !matches!(
                    self.queue.work_for(e.id),
                    Some(EntryWork::Ready { .. } | EntryWork::Failed(_))
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
                    EntryWork::Failed("the source track is gone".into()),
                );
                continue;
            };
            if src.uuid != entry.src_uuid {
                self.queue.set_work(
                    entry.id,
                    EntryWork::Failed("the source track was replaced".into()),
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

            let job = worker::Job::Fingerprint {
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
                    EntryWork::Fingerprinting {
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
    pub(super) fn on_fingerprinted(
        &mut self,
        entry_id: i64,
        generation: u64,
        result: Result<crate::transfer::GateOutcome, String>,
    ) {
        if !self.queue.accepts(entry_id, generation) {
            return;
        }
        let Some(EntryWork::Fingerprinting { dst_content_id, .. }) = self.queue.work_for(entry_id)
        else {
            return;
        };
        let dst_content_id = dst_content_id.clone();
        let Some(entry) = self.entry(entry_id) else {
            return;
        };

        match result {
            Err(why) => self.queue.set_work(entry_id, EntryWork::Failed(why)),
            Ok(outcome) if !outcome.verdict.is_accept() => {
                let why = outcome.verdict.summary();
                if let Some(store) = self.store.as_ref() {
                    let _ = store.set_rejected(entry_id, &why);
                }
                self.queue.set_work(entry_id, EntryWork::Failed(why));
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
                        EntryWork::Failed(
                            "the rekordbox row changed while checking — check it again".into(),
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
                            EntryWork::Ready {
                                plan: Box::new(plan),
                                verdict: outcome.verdict,
                            },
                        ),
                        Err(e) => self
                            .queue
                            .set_work(entry_id, EntryWork::Failed(e.to_string())),
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
            .filter(|id| matches!(self.queue.work_for(*id), Some(EntryWork::Ready { .. })))
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
            let Some(EntryWork::Ready { plan, verdict }) = self.queue.take_work(*id) else {
                continue;
            };
            let entry = self.entry(*id);
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
                    self.queue.set_work(*id, EntryWork::Failed(e.to_string()));
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
    pub fn retry_entry(&mut self, entry_id: i64) {
        let Some(entry) = self.entry(entry_id) else {
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
    /// with no undo, so a bulk forget would be a foot-gun.
    pub fn forget_entry(&mut self, entry_id: i64) {
        let Some(entry) = self.entry(entry_id) else {
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

    /// Build the plan for a transfer with the fingerprint gate skipped, and ask.
    ///
    /// For the sources the gate cannot answer for at all — a DRM stream, a file
    /// on another machine — and for one it rejected that you disagree with.
    /// Everything else still holds: the row must exist, the source must still be
    /// the track this was queued for, master.db is backed up, and the entry is
    /// recorded as UNVERIFIED rather than as having passed.
    pub fn start_force(&mut self, entry_id: i64) {
        let Some(entry) = self.entry(entry_id) else {
            return;
        };
        if self.queue.in_flight(entry.id) {
            self.status
                .info("still checking that one — give it a moment.");
            return;
        }
        let Some(dst_content_id) =
            crate::pending::find_imported_row(&self.db, &entry.acquired_path)
                .ok()
                .flatten()
        else {
            self.status
                .err("rekordbox has no row for this file yet — import it first.");
            self.status.act(NoticeAction::ImportDownloads);
            return;
        };
        let Ok(src) = crate::analysis::load_track(&self.db, &entry.src_content_id) else {
            self.status.err("the source track is gone.");
            return;
        };
        if src.uuid != entry.src_uuid {
            self.status.err("the source track was replaced.");
            self.status.act(NoticeAction::ForgetEntry { entry_id: entry.id });
            return;
        }
        let opts = CopyOpts {
            replace: entry.replace,
            lock: entry.lock,
        };
        match crate::analysis::build_plan(&self.db, &entry.src_content_id, &dst_content_id, &opts) {
            Ok(plan) => {
                self.confirmations.open(Batch::ForceApply(ForceBatch {
                    entry_id: entry.id,
                    dst_content_id,
                    plan: Box::new(plan),
                    verdict: crate::transfer::bypass_verdict(&src),
                }));
            }
            Err(e) => self.status.err(format!("no plan for it: {e}")),
        }
    }

    /// Write the forced plan.
    pub fn apply_force_batch(&mut self) -> bool {
        let Some(Batch::ForceApply(batch)) = self.confirmations.take(ConfirmKind::ForceApply) else {
            return false;
        };
        // The row the plan was built against may have been replaced since the
        // confirmation opened. Skipping the fingerprint does not mean writing to
        // whatever happens to be there now.
        let entry = self.entry(batch.entry_id);
        let still = entry.as_ref().and_then(|e| {
            crate::pending::find_imported_row(&self.db, &e.acquired_path)
                .ok()
                .flatten()
        });
        if still.as_deref() != Some(batch.dst_content_id.as_str()) {
            self.status
                .err("the rekordbox row changed since you were shown the plan.");
            self.status.act(NoticeAction::ForceEntry {
                entry_id: batch.entry_id,
            });
            return false;
        }
        if let Err(e) = crate::db::safety_preflight(self.safety) {
            self.status.err(format!("{e}"));
            return false;
        }
        match crate::analysis::apply_plan(&mut self.db, &batch.plan) {
            Ok(_backup) => {
                if let (Some(store), Some(entry)) = (self.store.as_ref(), entry.as_ref()) {
                    let _ = store.set_matched(
                        batch.entry_id,
                        &batch.plan.dst.id,
                        &batch.verdict.summary(),
                    );
                    let _ = crate::transfer::mark_applied(store, entry);
                }
                self.queue.clear_work(batch.entry_id);
                self.reload_queue();
                self.status.warn(format!(
                    "applied #{} unverified — check the cues in rekordbox.",
                    batch.entry_id
                ));
            }
            Err(e) => {
                self.unresolved_errors = true;
                self.status.err(format!("could not apply it: {e}"));
            }
        }
        true
    }

    /// Ask before emptying the queue.
    pub fn start_clear_queue(&mut self) {
        if self.queue.entries.is_empty() {
            self.status.info("the queue is already empty.");
            return;
        }
        self.confirmations.open(Batch::ClearQueue);
    }

    /// Forget every entry. The downloaded files are left on disk.
    pub fn clear_queue(&mut self) -> bool {
        self.confirmations.take(ConfirmKind::ClearQueue);
        let Some(store) = self.store.as_ref() else {
            return false;
        };
        match store.clear_all() {
            Ok(n) => {
                self.reload_queue();
                self.status.info(format!(
                    "forgot {n} entr(y/ies). The files are still there."
                ));
            }
            Err(e) => self.status.err(format!("could not clear the queue: {e}")),
        }
        true
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
            artwork: None,
        }
    }
}
