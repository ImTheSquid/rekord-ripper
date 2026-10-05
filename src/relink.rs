//! Point playlist entries at a downloaded copy instead of the stream it replaced.
//!
//! A download and analysis transfer leaves the stream row where it was, so every
//! playlist keeps the copy a USB stick cannot play. A stream row and a file row
//! with the same title and artist, and lengths within two seconds, are taken to
//! be the same track. Each live playlist entry of the stream is changed in place
//! to name the file, keeping its position, ID and UUID; sync sees an edit.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::db::{MasterDb, now_db_string};
use crate::format::{self, Origin};

/// rekordbox stores whole seconds, and a re-encode can round either way.
const LENGTH_TOLERANCE_SECS: i64 = 2;

/// One playlist entry that will name the file instead of the stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// `djmdSongPlaylist.ID`.
    pub id: String,
    pub playlist: String,
}

#[derive(Debug, Clone)]
pub struct Pair {
    pub stream_id: String,
    pub file_id: String,
    pub label: String,
    pub file_path: String,
    pub entries: Vec<Entry>,
    /// Playlists that already hold the file as well, whose stream entry is left
    /// for you to remove: swapping it would list the track twice.
    pub already: Vec<String>,
}

pub struct Plan {
    pub pairs: Vec<Pair>,
    /// Streams with more than one matching file, left alone, with the count.
    pub ambiguous: Vec<(String, usize)>,
}

struct Track {
    id: String,
    key: (String, String),
    label: String,
    length: Option<i64>,
    folder_path: String,
}

/// Find every stream with exactly one downloaded twin, and its playlist entries.
pub fn plan(db: &MasterDb) -> Result<Plan> {
    let mut stmt = db.conn.prepare(
        "SELECT c.ID, c.Title, a.Name, c.Length, c.FileType, c.FolderPath, c.ServiceID
         FROM djmdContent c
         LEFT JOIN djmdArtist a ON a.ID = c.ArtistID
         WHERE c.rb_local_deleted = 0 OR c.rb_local_deleted IS NULL",
    )?;
    type Raw = (
        String,
        Option<String>,
        Option<String>,
        Option<i64>,
        Option<i64>,
        Option<String>,
        Option<i64>,
    );
    let raw: Vec<Raw> = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;

    let mut streams: Vec<Track> = Vec::new();
    let mut files: HashMap<(String, String), Vec<Track>> = HashMap::new();
    for (id, title, artist, length, file_type, path, service_id) in raw {
        let title = title.unwrap_or_default();
        let artist = artist.unwrap_or_default();
        if title.trim().is_empty() {
            continue;
        }
        let track = Track {
            id,
            key: (title.trim().to_lowercase(), artist.trim().to_lowercase()),
            label: format!("{artist} — {title}"),
            length,
            folder_path: path.clone().unwrap_or_default(),
        };
        let origin = format::origin(file_type, path.as_deref(), service_id);
        // A file only counts when something can play it: a local file on this
        // machine, or a cloud file sync delivers. FileType 0 plays nowhere.
        let playable = file_type.is_some_and(|ft| ft != crate::import::UNPLAYABLE_FILE_TYPE)
            && match origin {
                Origin::Local => crate::presence::check(origin, path.as_deref()) == Some(true),
                Origin::Cloud => true,
                Origin::Stream => false,
            };
        match origin {
            Origin::Stream => streams.push(track),
            _ if playable => files.entry(track.key.clone()).or_default().push(track),
            _ => {}
        }
    }

    let paths = crate::playlists::paths_by_id(db)?;
    let mut stmt = db.conn.prepare(
        "SELECT ID, PlaylistID, ContentID FROM djmdSongPlaylist
         WHERE rb_local_deleted = 0 OR rb_local_deleted IS NULL",
    )?;
    let live: Vec<(String, Option<String>, Option<String>)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut by_content: HashMap<&str, Vec<(&str, &str)>> = HashMap::new();
    let mut holds: HashSet<(&str, &str)> = HashSet::new();
    for (entry, playlist, content) in &live {
        let (Some(playlist), Some(content)) = (playlist.as_deref(), content.as_deref()) else {
            continue;
        };
        holds.insert((playlist, content));
        by_content
            .entry(content)
            .or_default()
            .push((entry.as_str(), playlist));
    }

    let mut out = Plan {
        pairs: Vec::new(),
        ambiguous: Vec::new(),
    };
    for stream in streams {
        let twins: Vec<&Track> = files
            .get(&stream.key)
            .map(|v| {
                v.iter()
                    .filter(|f| match (f.length, stream.length) {
                        (Some(a), Some(b)) => (a - b).abs() <= LENGTH_TOLERANCE_SECS,
                        _ => false,
                    })
                    .collect()
            })
            .unwrap_or_default();
        let file = match twins.as_slice() {
            [] => continue,
            [one] => *one,
            many => {
                out.ambiguous.push((stream.label, many.len()));
                continue;
            }
        };
        let mut entries = Vec::new();
        let mut already = Vec::new();
        for &(entry, playlist) in by_content.get(stream.id.as_str()).into_iter().flatten() {
            // An entry in a deleted playlist is not worth an edit.
            let Some(name) = paths.get(playlist) else {
                continue;
            };
            if holds.contains(&(playlist, file.id.as_str())) {
                already.push(name.clone());
            } else {
                entries.push(Entry {
                    id: entry.to_string(),
                    playlist: name.clone(),
                });
            }
        }
        if entries.is_empty() && already.is_empty() {
            continue;
        }
        out.pairs.push(Pair {
            stream_id: stream.id,
            file_id: file.id.clone(),
            label: stream.label,
            file_path: file.folder_path.clone(),
            entries,
            already,
        });
    }
    out.pairs.sort_by(|a, b| a.label.cmp(&b.label));
    Ok(out)
}

/// What a relink changed, written beside the backup so [`undo`] can reverse it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelinkNote {
    /// (entry, stream it named, file it names now)
    pub swaps: Vec<(String, String, String)>,
    pub relinked_at: String,
    pub backup: String,
}

fn note_path(backup: &Path) -> PathBuf {
    let mut name = backup.file_name().unwrap_or_default().to_os_string();
    name.push(".relink.json");
    backup.with_file_name(name)
}

/// Swap every planned entry from its stream to its file, in one transaction
/// with one USN per entry. Writes the undo note first.
pub fn apply(db: &MasterDb, plan: &Plan, backup: &Path) -> Result<PathBuf> {
    let swaps: Vec<(String, String, String)> = plan
        .pairs
        .iter()
        .flat_map(|p| {
            p.entries
                .iter()
                .map(|e| (e.id.clone(), p.stream_id.clone(), p.file_id.clone()))
        })
        .collect();
    let note = RelinkNote {
        swaps,
        relinked_at: now_db_string(),
        backup: backup.to_string_lossy().into_owned(),
    };
    let path = note_path(backup);
    std::fs::write(&path, serde_json::to_vec_pretty(&note)?)
        .with_context(|| format!("writing {}", path.display()))?;
    set_content(
        db,
        note.swaps
            .iter()
            .map(|(e, from, to)| (e.as_str(), from.as_str(), to.as_str())),
    )?;
    Ok(path)
}

/// Point each entry from `from` to `to`, refusing the lot if any entry has
/// changed since it was planned.
fn set_content<'a>(
    db: &MasterDb,
    swaps: impl Iterator<Item = (&'a str, &'a str, &'a str)>,
) -> Result<usize> {
    let base = db.read_local_usn()?;
    let now = now_db_string();
    let tx = db.conn.unchecked_transaction()?;
    let mut n = 0i64;
    for (entry, from, to) in swaps {
        n += 1;
        let changed = tx.execute(
            "UPDATE djmdSongPlaylist
             SET ContentID = ?3, rb_local_synced = 0, rb_local_usn = ?4, updated_at = ?5
             WHERE ID = ?1 AND ContentID = ?2
               AND (rb_local_deleted = 0 OR rb_local_deleted IS NULL)",
            params![entry, from, to, base + n, now],
        )?;
        if changed != 1 {
            bail!("playlist entry {entry} no longer names track {from}; nothing was changed");
        }
    }
    if n > 0 {
        db.write_local_usn(base + n)?;
    }
    tx.commit()?;
    Ok(n as usize)
}

/// The most recent relink note in `backup_dir`.
pub fn latest_note(backup_dir: &Path) -> Result<(PathBuf, RelinkNote)> {
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(backup_dir)
        .with_context(|| format!("reading {}", backup_dir.display()))?
    {
        let entry = entry?;
        let name = entry.file_name();
        if !name.to_string_lossy().ends_with(".relink.json") {
            continue;
        }
        let modified = entry.metadata()?.modified()?;
        if newest.as_ref().is_none_or(|(t, _)| modified > *t) {
            newest = Some((modified, entry.path()));
        }
    }
    let (_, path) = newest.ok_or_else(|| anyhow!("no relink to undo"))?;
    let note = serde_json::from_slice(&std::fs::read(&path)?)
        .with_context(|| format!("reading {}", path.display()))?;
    Ok((path, note))
}

/// Point every entry a relink moved back at its stream.
pub fn undo(db: &MasterDb, note: &RelinkNote) -> Result<usize> {
    set_content(
        db,
        note.swaps
            .iter()
            .map(|(e, stream, file)| (e.as_str(), file.as_str(), stream.as_str())),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> MasterDb {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE djmdContent (ID TEXT, Title TEXT, ArtistID TEXT, Length INTEGER,
                FileType INTEGER, FolderPath TEXT, ServiceID INTEGER, rb_local_deleted INTEGER);
             CREATE TABLE djmdArtist (ID TEXT, Name TEXT);
             CREATE TABLE djmdPlaylist (ID TEXT, Name TEXT, ParentID TEXT,
                rb_local_deleted INTEGER);
             CREATE TABLE djmdSongPlaylist (ID TEXT, PlaylistID TEXT, ContentID TEXT,
                TrackNo INTEGER, rb_local_deleted INTEGER, rb_local_synced INTEGER,
                rb_local_usn INTEGER, updated_at TEXT);
             CREATE TABLE agentRegistry (registry_id TEXT, int_1 INTEGER, updated_at TEXT);
             INSERT INTO agentRegistry VALUES ('localUpdateCount', 10, '');
             INSERT INTO djmdArtist VALUES ('a', 'Addison Rae');
             INSERT INTO djmdPlaylist VALUES ('p1', 'Fridays', 'root', 0),
                                             ('p2', 'Mix', 'root', 0),
                                             ('p3', 'Master', 'root', 0);",
        )
        .unwrap();
        MasterDb {
            conn,
            app_dir: PathBuf::from("."),
        }
    }

    fn track(db: &MasterDb, id: &str, title: &str, len: i64, ft: i64, path: &str, sid: i64) {
        db.conn
            .execute(
                "INSERT INTO djmdContent VALUES (?1, ?2, 'a', ?3, ?4, ?5, ?6, 0)",
                params![id, title, len, ft, path, sid],
            )
            .unwrap();
    }

    fn entry(db: &MasterDb, id: &str, playlist: &str, content: &str) {
        db.conn
            .execute(
                "INSERT INTO djmdSongPlaylist VALUES (?1, ?2, ?3, 1, 0, 1, 0, '')",
                params![id, playlist, content],
            )
            .unwrap();
    }

    fn content_of(db: &MasterDb, entry: &str) -> String {
        db.conn
            .query_row(
                "SELECT ContentID FROM djmdSongPlaylist WHERE ID = ?1",
                params![entry],
                |r| r.get(0),
            )
            .unwrap()
    }

    /// The shape of the real case: a SoundCloud stream in two playlists, and the
    /// downloaded copy, synced to the cloud, in a third.
    fn stream_and_download(db: &MasterDb) {
        track(db, "s", "Bad (VIP)", 101, 19, "soundcloud:tracks:1", 0);
        track(db, "f", "Bad (VIP)", 101, 4, "/contents_1/a/b.m4a", 2);
        entry(db, "e1", "p1", "s");
        entry(db, "e2", "p2", "s");
        entry(db, "e3", "p3", "f");
    }

    #[test]
    fn a_stream_with_one_downloaded_twin_has_its_entries_swapped() {
        let db = db();
        stream_and_download(&db);
        let plan = plan(&db).unwrap();
        assert_eq!(plan.pairs.len(), 1);
        let pair = &plan.pairs[0];
        assert_eq!((pair.stream_id.as_str(), pair.file_id.as_str()), ("s", "f"));
        assert_eq!(pair.entries.len(), 2);

        let dir = std::env::temp_dir().join(format!("rr-relink-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let note = apply(&db, &plan, &dir.join("master.db.bak")).unwrap();
        assert_eq!(content_of(&db, "e1"), "f");
        assert_eq!(content_of(&db, "e2"), "f");
        assert_eq!(db.read_local_usn().unwrap(), 12);

        let (_, found) = latest_note(&dir).unwrap();
        assert_eq!(found.swaps.len(), 2);
        assert_eq!(undo(&db, &found).unwrap(), 2);
        assert_eq!(content_of(&db, "e1"), "s");
        // Undoing twice finds nothing pointing at the file, and changes nothing.
        assert!(undo(&db, &found).is_err());
        assert_eq!(content_of(&db, "e2"), "s");
        let _ = std::fs::remove_file(note);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_playlist_already_holding_the_file_keeps_its_stream_entry() {
        let db = db();
        stream_and_download(&db);
        entry(&db, "e4", "p3", "s");
        let plan = plan(&db).unwrap();
        assert_eq!(plan.pairs[0].entries.len(), 2);
        assert_eq!(plan.pairs[0].already, vec!["master".to_string()]);
    }

    #[test]
    fn a_different_mix_or_length_is_not_a_twin() {
        let db = db();
        // The bracketed part is the whole difference between a remix and the
        // original, so it must count.
        track(&db, "s", "Bad (VIP)", 101, 19, "soundcloud:tracks:1", 0);
        track(&db, "f1", "Bad", 101, 4, "/contents_1/a/b.m4a", 2);
        track(&db, "f2", "Bad (VIP)", 140, 4, "/contents_1/a/c.m4a", 2);
        entry(&db, "e1", "p1", "s");
        assert!(plan(&db).unwrap().pairs.is_empty());
    }

    #[test]
    fn two_candidate_files_leave_the_stream_alone() {
        let db = db();
        stream_and_download(&db);
        track(&db, "g", "Bad (VIP)", 102, 1, "/contents_1/a/c.mp3", 2);
        let plan = plan(&db).unwrap();
        assert!(plan.pairs.is_empty());
        assert_eq!(plan.ambiguous.len(), 1);
    }

    #[test]
    fn a_local_file_that_is_not_here_or_cannot_play_is_no_twin() {
        let db = db();
        track(&db, "s", "Song", 100, 19, "soundcloud:tracks:1", 0);
        track(&db, "f", "Song", 100, 5, "/nope/not/here.flac", 0);
        track(&db, "g", "Song", 100, 0, "/contents_1/broken.mp3", 2);
        entry(&db, "e1", "p1", "s");
        assert!(plan(&db).unwrap().pairs.is_empty());
    }

    #[test]
    fn an_entry_changed_since_planning_aborts_the_whole_batch() {
        let db = db();
        stream_and_download(&db);
        let plan = plan(&db).unwrap();
        db.conn
            .execute(
                "UPDATE djmdSongPlaylist SET ContentID = 'x' WHERE ID = 'e2'",
                [],
            )
            .unwrap();
        let dir = std::env::temp_dir().join(format!("rr-relink-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(apply(&db, &plan, &dir.join("master.db.bak")).is_err());
        assert_eq!(content_of(&db, "e1"), "s", "the first swap was rolled back");
        assert_eq!(db.read_local_usn().unwrap(), 10);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
