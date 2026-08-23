//! Pending old→new pairings, waiting for rekordbox to import the new file.
//!
//! # Why this exists
//!
//! Nothing in this crate creates `djmdContent` rows, and **rekordbox has no
//! watch-folder or auto-import feature** — confirmed against Pioneer's own
//! forums and FAQ; it is a long-standing feature request, not a setting. So a
//! freshly downloaded file is not in rekordbox at all, and the transfer cannot
//! happen in the same run as the download.
//!
//! Hence one stable download directory that you drag in once per batch, plus this
//! store, which remembers which old track each new file is meant to inherit from
//! and fires the transfer once the row appears.
//!
//! # Why SQLite rather than a JSON file
//!
//! A `watch` loop and a manual run both mutate this, so state transitions need to
//! be atomic. `rusqlite` is already a dependency — a connection with no
//! `PRAGMA key` is an ordinary unencrypted SQLite database, so this costs nothing
//! new. It is deliberately *not* stored in `master.db`: injecting foreign tables
//! into a database rekordbox syncs to the cloud invites problems that have
//! nothing to do with this feature.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};

use crate::db::{MasterDb, now_db_string};

/// Where a pairing has got to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Downloaded, waiting for you to drag the folder into rekordbox.
    AwaitingImport,
    /// Row found and the fingerprint gate passed; ready to apply.
    Matched,
    /// Transfer done.
    Applied,
    /// The gate refused. Kept so `watch` does not retry it forever.
    Rejected,
    /// Timed out waiting for an import.
    Expired,
    /// The file or the source track went away.
    Cancelled,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AwaitingImport => "awaiting_import",
            Self::Matched => "matched",
            Self::Applied => "applied",
            Self::Rejected => "rejected",
            Self::Expired => "expired",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "awaiting_import" => Self::AwaitingImport,
            "matched" => Self::Matched,
            "applied" => Self::Applied,
            "rejected" => Self::Rejected,
            "expired" => Self::Expired,
            "cancelled" => Self::Cancelled,
            _ => return None,
        })
    }

    /// True when nothing further will happen to this entry.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Applied | Self::Expired | Self::Cancelled)
    }
}

impl std::fmt::Display for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub id: i64,
    /// The already-analysed track whose cues should move.
    pub src_content_id: String,
    /// Guards against a recycled `djmdContent.ID` silently repointing this.
    pub src_uuid: String,
    pub src_title: Option<String>,
    pub src_artist: Option<String>,
    pub acquired_path: PathBuf,
    pub acquired_size: u64,
    pub acquired_mtime_ns: i64,
    pub provider: Option<String>,
    /// Defaults to true: rekordbox auto-analyses on import and leaves cues
    /// behind, which `build_plan` would otherwise refuse to overwrite.
    pub replace: bool,
    pub lock: bool,
    pub state: State,
    pub dst_content_id: Option<String>,
    pub verdict: Option<String>,
    pub created_at: String,
    pub expires_at: String,
}

pub struct PendingStore {
    conn: Connection,
}

impl PendingStore {
    /// Open (and create) the store.
    pub fn open() -> Result<Self> {
        let path = crate::paths::pending_db_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Self::open_at(&path)
    }

    pub fn open_at(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("opening pending store {}", path.display()))?;
        // A pending entry is worth less than the analysis data it guards, so
        // durability matters more than speed here.
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS pending_transfer (
               id                INTEGER PRIMARY KEY,
               src_content_id    TEXT NOT NULL,
               src_uuid          TEXT NOT NULL,
               src_title         TEXT,
               src_artist        TEXT,
               acquired_path     TEXT NOT NULL,
               acquired_size     INTEGER NOT NULL,
               acquired_mtime_ns INTEGER NOT NULL,
               provider          TEXT,
               copy_replace      INTEGER NOT NULL DEFAULT 1,
               copy_lock         INTEGER NOT NULL DEFAULT 0,
               state             TEXT NOT NULL,
               dst_content_id    TEXT,
               verdict           TEXT,
               created_at        TEXT NOT NULL,
               updated_at        TEXT NOT NULL,
               expires_at        TEXT NOT NULL
             );
             CREATE UNIQUE INDEX IF NOT EXISTS pending_awaiting_path
               ON pending_transfer(acquired_path) WHERE state = 'awaiting_import';
             CREATE INDEX IF NOT EXISTS pending_state ON pending_transfer(state);",
        )?;
        Ok(Self { conn })
    }

    /// Record a downloaded file and the track it should inherit from.
    pub fn add(
        &self,
        src: &crate::analysis::TrackHeader,
        acquired: &Path,
        provider: Option<&str>,
        replace: bool,
        lock: bool,
        ttl_days: i64,
    ) -> Result<i64> {
        let meta = std::fs::metadata(acquired)
            .with_context(|| format!("stat {}", acquired.display()))?;
        let now = chrono::Utc::now();
        let expires = now + chrono::Duration::days(ttl_days.max(1));

        self.conn.execute(
            "INSERT INTO pending_transfer
               (src_content_id, src_uuid, src_title, src_artist, acquired_path,
                acquired_size, acquired_mtime_ns, provider, copy_replace, copy_lock,
                state, created_at, updated_at, expires_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?12,?13)",
            params![
                src.id,
                src.uuid,
                src.title,
                src.artist,
                canonical(acquired),
                meta.len() as i64,
                mtime_ns(&meta),
                provider,
                replace as i64,
                lock as i64,
                State::AwaitingImport.as_str(),
                now_db_string(),
                expires.format("%Y-%m-%d %H:%M:%S%.3f +00:00").to_string(),
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn all(&self) -> Result<Vec<Entry>> {
        self.query("SELECT * FROM pending_transfer ORDER BY id DESC", params![])
    }

    pub fn in_state(&self, state: State) -> Result<Vec<Entry>> {
        self.query(
            "SELECT * FROM pending_transfer WHERE state = ?1 ORDER BY id",
            params![state.as_str()],
        )
    }

    fn query(&self, sql: &str, p: impl rusqlite::Params) -> Result<Vec<Entry>> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(p, |r| {
            Ok(Entry {
                id: r.get("id")?,
                src_content_id: r.get("src_content_id")?,
                src_uuid: r.get("src_uuid")?,
                src_title: r.get("src_title")?,
                src_artist: r.get("src_artist")?,
                acquired_path: PathBuf::from(r.get::<_, String>("acquired_path")?),
                acquired_size: r.get::<_, i64>("acquired_size")? as u64,
                acquired_mtime_ns: r.get("acquired_mtime_ns")?,
                provider: r.get("provider")?,
                replace: r.get::<_, i64>("copy_replace")? != 0,
                lock: r.get::<_, i64>("copy_lock")? != 0,
                state: State::parse(&r.get::<_, String>("state")?)
                    .unwrap_or(State::Cancelled),
                dst_content_id: r.get("dst_content_id")?,
                verdict: r.get("verdict")?,
                created_at: r.get("created_at")?,
                expires_at: r.get("expires_at")?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn set_state(&self, id: i64, state: State) -> Result<()> {
        self.conn.execute(
            "UPDATE pending_transfer SET state = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, state.as_str(), now_db_string()],
        )?;
        Ok(())
    }

    pub fn set_matched(&self, id: i64, dst_content_id: &str, verdict: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE pending_transfer
             SET state = ?2, dst_content_id = ?3, verdict = ?4, updated_at = ?5
             WHERE id = ?1",
            params![
                id,
                State::Matched.as_str(),
                dst_content_id,
                verdict,
                now_db_string()
            ],
        )?;
        Ok(())
    }

    pub fn set_rejected(&self, id: i64, verdict: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE pending_transfer SET state = ?2, verdict = ?3, updated_at = ?4 WHERE id = ?1",
            params![id, State::Rejected.as_str(), verdict, now_db_string()],
        )?;
        Ok(())
    }

    pub fn remove(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM pending_transfer WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Retire entries that can no longer make progress.
    ///
    /// Returns what changed, so the caller can say so rather than silently
    /// dropping work the user was waiting on.
    pub fn sweep(&self, db: &MasterDb) -> Result<Vec<(i64, State, String)>> {
        let mut changed = Vec::new();
        let now = chrono::Utc::now();

        for e in self.in_state(State::AwaitingImport)? {
            // The file went away — you deleted the download.
            if !e.acquired_path.exists() {
                self.set_state(e.id, State::Cancelled)?;
                changed.push((e.id, State::Cancelled, "the downloaded file is gone".into()));
                continue;
            }
            // The file changed since we recorded it, so it is no longer the file
            // this pairing was reasoned about.
            if let Ok(meta) = std::fs::metadata(&e.acquired_path) {
                if meta.len() != e.acquired_size || mtime_ns(&meta) != e.acquired_mtime_ns {
                    self.set_state(e.id, State::Expired)?;
                    changed.push((
                        e.id,
                        State::Expired,
                        "the downloaded file changed on disk".into(),
                    ));
                    continue;
                }
            }
            // The source track was deleted, or its ID was recycled onto another
            // track — the UUID check is what catches the second case.
            match source_still_valid(db, &e)? {
                true => {}
                false => {
                    self.set_state(e.id, State::Cancelled)?;
                    changed.push((
                        e.id,
                        State::Cancelled,
                        format!("source track {} is gone or changed", e.src_content_id),
                    ));
                    continue;
                }
            }
            if parse_db_time(&e.expires_at).map(|t| now > t).unwrap_or(false) {
                self.set_state(e.id, State::Expired)?;
                changed.push((
                    e.id,
                    State::Expired,
                    "waited past its expiry without being imported".into(),
                ));
            }
        }
        Ok(changed)
    }
}

/// Whether the source row still exists and is still the same track.
fn source_still_valid(db: &MasterDb, e: &Entry) -> Result<bool> {
    let uuid: Option<String> = db
        .conn
        .query_row(
            "SELECT UUID FROM djmdContent
             WHERE ID = ?1 AND (rb_local_deleted = 0 OR rb_local_deleted IS NULL)",
            params![e.src_content_id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(uuid.as_deref() == Some(e.src_uuid.as_str()))
}

/// Find the `djmdContent` row for an imported file.
///
/// Path matching, in order of confidence:
///
/// 1. Exact match on `FolderPath` **or `OrgFolderPath`** — the second arm is not
///    optional. Cloud Library Sync rewrites `FolderPath` to a
///    `/contents_<dbid>/…` form and keeps the original in `OrgFolderPath`, so a
///    `FolderPath`-only query silently misses every cloud-managed row.
/// 2. The same, case-insensitively. APFS is case-insensitive by default, so the
///    path rekordbox stored may differ in case from the one we wrote.
/// 3. Filename plus size, as a last resort.
///
/// Loose fallbacks are safe here *only* because the fingerprint gate is the real
/// check: this picks a candidate, it never authorises a write.
pub fn find_imported_row(db: &MasterDb, path: &Path) -> Result<Option<String>> {
    let p = canonical(path);
    let exact: Option<String> = db
        .conn
        .query_row(
            "SELECT ID FROM djmdContent
             WHERE (rb_local_deleted = 0 OR rb_local_deleted IS NULL)
               AND (FolderPath = ?1 OR OrgFolderPath = ?1)
             LIMIT 1",
            params![p],
            |r| r.get(0),
        )
        .optional()?;
    if exact.is_some() {
        return Ok(exact);
    }

    let ci: Option<String> = db
        .conn
        .query_row(
            "SELECT ID FROM djmdContent
             WHERE (rb_local_deleted = 0 OR rb_local_deleted IS NULL)
               AND (FolderPath = ?1 COLLATE NOCASE OR OrgFolderPath = ?1 COLLATE NOCASE)
             LIMIT 1",
            params![p],
            |r| r.get(0),
        )
        .optional()?;
    if ci.is_some() {
        return Ok(ci);
    }

    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return Ok(None);
    };
    let size = std::fs::metadata(path).map(|m| m.len() as i64).unwrap_or(-1);
    Ok(db
        .conn
        .query_row(
            "SELECT ID FROM djmdContent
             WHERE (rb_local_deleted = 0 OR rb_local_deleted IS NULL)
               AND FileNameL = ?1 AND FileSize = ?2
             LIMIT 1",
            params![name, size],
            |r| r.get(0),
        )
        .optional()?)
}

/// Absolute path as a string, resolving symlinks when possible.
fn canonical(p: &Path) -> String {
    std::fs::canonicalize(p)
        .unwrap_or_else(|_| p.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

fn mtime_ns(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

/// Parse the timestamp format `master.db` uses, e.g.
/// `2026-08-23 14:05:06.789 +00:00`.
///
/// `%:z` for the colon-separated offset; falls back to the leading
/// `YYYY-MM-DD HH:MM:SS`, treated as UTC, so a slightly different tail cannot
/// make an entry un-expirable.
fn parse_db_time(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    let s = s.trim();
    if let Ok(dt) = chrono::DateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.3f %:z") {
        return Some(dt.with_timezone(&chrono::Utc));
    }
    if s.len() >= 19 {
        if let Ok(n) = chrono::NaiveDateTime::parse_from_str(&s[..19], "%Y-%m-%d %H:%M:%S") {
            return Some(n.and_utc());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::TrackHeader;

    fn header(id: &str, uuid: &str) -> TrackHeader {
        TrackHeader {
            id: id.into(),
            uuid: uuid.into(),
            title: Some("Old Rip".into()),
            artist: Some("Artist".into()),
            bpm: Some(12800),
            length: Some(210),
            analysed: Some(105),
            analysis_data_path: Some("/PIONEER/USBANLZ/a/b/ANLZ0000.DAT".into()),
            file_type: Some(0),
            cue_count: 4,
            folder_path: Some("/music/old.mp3".into()),
            org_folder_path: None,
        }
    }

    fn store() -> (PendingStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!("rr-pending-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("pending.sqlite");
        (PendingStore::open_at(&db).unwrap(), dir)
    }

    fn a_file(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, b"audio bytes").unwrap();
        p
    }

    #[test]
    fn records_and_reads_back_a_pairing() {
        let (s, dir) = store();
        let f = a_file(&dir, "new.flac");
        let id = s.add(&header("101", "u-101"), &f, Some("bandcamp"), true, false, 14).unwrap();

        let entries = s.all().unwrap();
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.id, id);
        assert_eq!(e.src_content_id, "101");
        assert_eq!(e.src_uuid, "u-101");
        assert_eq!(e.state, State::AwaitingImport);
        assert_eq!(e.provider.as_deref(), Some("bandcamp"));
        assert_eq!(e.acquired_size, 11);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn replace_defaults_on_because_rekordbox_auto_analyses_imports() {
        // Without this, build_plan refuses with "destination has N existing cues".
        let (s, dir) = store();
        let f = a_file(&dir, "new.flac");
        s.add(&header("1", "u1"), &f, None, true, false, 14).unwrap();
        assert!(s.all().unwrap()[0].replace);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn state_transitions_are_recorded() {
        let (s, dir) = store();
        let f = a_file(&dir, "new.flac");
        let id = s.add(&header("1", "u1"), &f, None, true, false, 14).unwrap();

        s.set_matched(id, "202", "score 1.2, shift 0ms").unwrap();
        let e = &s.all().unwrap()[0];
        assert_eq!(e.state, State::Matched);
        assert_eq!(e.dst_content_id.as_deref(), Some("202"));
        assert!(e.verdict.as_deref().unwrap().contains("shift 0ms"));

        s.set_state(id, State::Applied).unwrap();
        assert_eq!(s.all().unwrap()[0].state, State::Applied);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_rejection_is_remembered_so_watch_does_not_retry_forever() {
        let (s, dir) = store();
        let f = a_file(&dir, "new.flac");
        let id = s.add(&header("1", "u1"), &f, None, true, false, 14).unwrap();
        s.set_rejected(id, "time-shifted by 3220ms").unwrap();

        let e = &s.all().unwrap()[0];
        assert_eq!(e.state, State::Rejected);
        assert!(e.verdict.as_deref().unwrap().contains("3220ms"));
        // And it is no longer picked up as awaiting import.
        assert!(s.in_state(State::AwaitingImport).unwrap().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_same_file_cannot_be_queued_twice_while_awaiting_import() {
        let (s, dir) = store();
        let f = a_file(&dir, "new.flac");
        s.add(&header("1", "u1"), &f, None, true, false, 14).unwrap();
        assert!(
            s.add(&header("2", "u2"), &f, None, true, false, 14).is_err(),
            "the partial unique index should prevent a duplicate"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_same_file_can_be_requeued_once_the_first_attempt_is_done() {
        let (s, dir) = store();
        let f = a_file(&dir, "new.flac");
        let id = s.add(&header("1", "u1"), &f, None, true, false, 14).unwrap();
        s.set_state(id, State::Rejected).unwrap();
        // The index is partial on awaiting_import, so a retry is allowed.
        assert!(s.add(&header("2", "u2"), &f, None, true, false, 14).is_ok());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn terminal_states_are_identified() {
        assert!(State::Applied.is_terminal());
        assert!(State::Expired.is_terminal());
        assert!(State::Cancelled.is_terminal());
        assert!(!State::AwaitingImport.is_terminal());
        assert!(!State::Matched.is_terminal());
        // Rejected is not terminal: you may fix the pairing and retry by hand.
        assert!(!State::Rejected.is_terminal());
    }

    #[test]
    fn states_round_trip_through_their_string_form() {
        for s in [
            State::AwaitingImport,
            State::Matched,
            State::Applied,
            State::Rejected,
            State::Expired,
            State::Cancelled,
        ] {
            assert_eq!(State::parse(s.as_str()), Some(s));
        }
        assert_eq!(State::parse("nonsense"), None);
    }

    #[test]
    fn parses_the_timestamp_format_the_database_uses() {
        let t = parse_db_time("2026-08-23 14:05:06.789 +00:00").expect("should parse");
        assert_eq!(t.format("%Y-%m-%d %H:%M:%S").to_string(), "2026-08-23 14:05:06");
        assert!(parse_db_time("not a time").is_none());
    }

    #[test]
    fn mtime_is_captured_so_a_changed_file_can_be_detected() {
        let (s, dir) = store();
        let f = a_file(&dir, "new.flac");
        s.add(&header("1", "u1"), &f, None, true, false, 14).unwrap();
        let before = s.all().unwrap()[0].acquired_mtime_ns;
        assert!(before > 0, "mtime should have been recorded");

        // Rewriting the file changes size, which the sweep looks at.
        std::fs::write(&f, b"different contents entirely").unwrap();
        let meta = std::fs::metadata(&f).unwrap();
        assert_ne!(meta.len(), s.all().unwrap()[0].acquired_size);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn removing_an_entry_works() {
        let (s, dir) = store();
        let f = a_file(&dir, "new.flac");
        let id = s.add(&header("1", "u1"), &f, None, true, false, 14).unwrap();
        s.remove(id).unwrap();
        assert!(s.all().unwrap().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reopening_the_store_keeps_its_contents() {
        let dir = std::env::temp_dir().join(format!("rr-pending-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("pending.sqlite");
        let f = a_file(&dir, "new.flac");
        {
            let s = PendingStore::open_at(&db_path).unwrap();
            s.add(&header("1", "u1"), &f, None, true, false, 14).unwrap();
        }
        let s = PendingStore::open_at(&db_path).unwrap();
        assert_eq!(s.all().unwrap().len(), 1, "state must survive a restart");
        std::fs::remove_dir_all(&dir).ok();
    }
}
