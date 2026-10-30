//! Playlist membership, flattened into the haystack a `p:` term matches.

use std::collections::HashMap;

use anyhow::Result;

use crate::db::MasterDb;

/// How far up a `ParentID` chain to walk before calling it a cycle.
const MAX_DEPTH: usize = 32;

/// Playlist membership per track id, lowercased, one path per line.
///
/// Paths are folder-qualified (`"jack night/jn4"`), so `p:` narrows by a whole
/// folder as easily as by one playlist. Smart playlists keep their membership as
/// a query rekordbox evaluates and store no rows here, so they never match.
pub fn blobs_by_track(db: &MasterDb) -> Result<HashMap<String, String>> {
    let paths = paths_by_id(db)?;

    let mut stmt = db.conn.prepare(
        "SELECT ContentID, PlaylistID FROM djmdSongPlaylist
         WHERE rb_local_deleted = 0 OR rb_local_deleted IS NULL",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, Option<String>>("ContentID")?,
            r.get::<_, Option<String>>("PlaylistID")?,
        ))
    })?;

    let mut blobs: HashMap<String, String> = HashMap::new();
    for row in rows {
        let (Some(content_id), Some(playlist_id)) = row? else {
            continue;
        };
        let Some(path) = paths.get(playlist_id.as_str()) else {
            continue;
        };
        let blob = blobs.entry(content_id).or_default();
        if !blob.is_empty() {
            blob.push('\n');
        }
        blob.push_str(path);
    }
    Ok(blobs)
}

/// Every live playlist's folder-qualified path, lowercased, by playlist ID.
pub(crate) fn paths_by_id(db: &MasterDb) -> Result<HashMap<String, String>> {
    let mut stmt = db.conn.prepare(
        "SELECT ID, Name, ParentID FROM djmdPlaylist
         WHERE rb_local_deleted = 0 OR rb_local_deleted IS NULL",
    )?;
    let nodes: HashMap<String, (String, Option<String>)> = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>("ID")?,
                (
                    r.get::<_, Option<String>>("Name")?.unwrap_or_default(),
                    r.get::<_, Option<String>>("ParentID")?,
                ),
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;

    Ok(nodes
        .keys()
        .map(|id| (id.clone(), path_of(&nodes, id)))
        .collect())
}

/// `"folder/sub/name"`, lowercased. Depth-capped, so a `ParentID` cycle in a
/// synced database cannot hang the load.
fn path_of(nodes: &HashMap<String, (String, Option<String>)>, id: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    let mut cur = id;
    for _ in 0..MAX_DEPTH {
        let Some((name, parent)) = nodes.get(cur) else {
            break;
        };
        parts.push(name.as_str());
        match parent.as_deref() {
            Some(p) if p != "root" && !p.is_empty() => cur = p,
            _ => break,
        }
    }
    parts.reverse();
    parts.join("/").to_lowercase()
}

/// One entry in rekordbox's playlist sidebar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlaylistNode {
    pub id: String,
    pub name: String,
    /// `None` at the top level.
    pub parent: Option<String>,
    pub folder: bool,
    /// Membership is a query rekordbox evaluates, so no tracks are listed here.
    pub smart: bool,
}

/// `Attribute` for a folder. Every other value is a list of tracks.
const FOLDER: i64 = 1;

/// Every live playlist and folder, parents before children, siblings in
/// rekordbox's own order.
pub fn tree(db: &MasterDb) -> Result<Vec<PlaylistNode>> {
    let mut stmt = db.conn.prepare(
        "SELECT ID, Name, ParentID, Attribute, SmartList FROM djmdPlaylist
         WHERE rb_local_deleted = 0 OR rb_local_deleted IS NULL
         ORDER BY Seq",
    )?;
    let nodes: Vec<PlaylistNode> = stmt
        .query_map([], |r| {
            Ok(PlaylistNode {
                id: r.get("ID")?,
                name: r.get::<_, Option<String>>("Name")?.unwrap_or_default(),
                parent: r
                    .get::<_, Option<String>>("ParentID")?
                    .filter(|p| p != "root" && !p.is_empty()),
                folder: r.get::<_, Option<i64>>("Attribute")? == Some(FOLDER),
                smart: r
                    .get::<_, Option<String>>("SmartList")?
                    .is_some_and(|s| !s.is_empty()),
            })
        })?
        .collect::<rusqlite::Result<_>>()?;

    // Depth-first from the top, so a frontend can indent as it goes. A node
    // whose parent is missing, or part of a cycle, is never reached.
    let mut out = Vec::with_capacity(nodes.len());
    fn walk<'a>(nodes: &'a [PlaylistNode], parent: Option<&str>, depth: usize, out: &mut Vec<&'a PlaylistNode>) {
        if depth > MAX_DEPTH {
            return;
        }
        for n in nodes.iter().filter(|n| n.parent.as_deref() == parent) {
            out.push(n);
            walk(nodes, Some(&n.id), depth + 1, out);
        }
    }
    walk(&nodes, None, 0, &mut out);
    Ok(out.into_iter().cloned().collect())
}

/// Track ids in a playlist, in its own order. A folder lists every track in
/// the playlists under it, once each.
pub fn members(db: &MasterDb, id: &str) -> Result<Vec<String>> {
    let nodes = tree(db)?;
    let mut lists = vec![id.to_string()];
    let mut i = 0;
    while i < lists.len() {
        let parent = lists[i].clone();
        lists.extend(nodes.iter().filter(|n| n.parent.as_deref() == Some(parent.as_str())).map(|n| n.id.clone()));
        i += 1;
    }

    let mut stmt = db.conn.prepare(
        "SELECT ContentID FROM djmdSongPlaylist
         WHERE PlaylistID = ?1 AND (rb_local_deleted = 0 OR rb_local_deleted IS NULL)
         ORDER BY TrackNo",
    )?;
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for list in &lists {
        for id in stmt.query_map([list], |r| r.get::<_, Option<String>>(0))? {
            if let Some(id) = id?
                && seen.insert(id.clone())
            {
                out.push(id);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn db() -> MasterDb {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE djmdPlaylist (ID TEXT, Seq INTEGER, Name TEXT, Attribute INTEGER,
                ParentID TEXT, SmartList TEXT, rb_local_deleted INTEGER);
             CREATE TABLE djmdSongPlaylist (ID TEXT, PlaylistID TEXT, ContentID TEXT,
                TrackNo INTEGER, rb_local_deleted INTEGER);
             INSERT INTO djmdPlaylist VALUES
                ('f', 2, 'Gigs', 1, 'root', '', 0),
                ('a', 1, 'Warmup', 0, 'f', '', 0),
                ('b', 2, 'Peak', -128, 'f', '', 0),
                ('t', 1, 'Inbox', 0, 'root', NULL, 0),
                ('s', 3, 'Recent', 4, 'root', '<smart/>', 0),
                ('x', 4, 'Gone', 0, 'root', '', 1),
                ('o', 1, 'Orphan', 0, 'nowhere', '', 0);
             INSERT INTO djmdSongPlaylist VALUES
                ('1', 'a', 't2', 2, 0), ('2', 'a', 't1', 1, 0),
                ('3', 'b', 't1', 1, 0), ('4', 'b', 't3', 2, 0),
                ('5', 'b', 't4', 3, 1);",
        )
        .unwrap();
        MasterDb {
            conn,
            app_dir: PathBuf::from("."),
        }
    }

    #[test]
    fn the_tree_puts_parents_first_in_rekordbox_order() {
        let ids: Vec<String> = tree(&db()).unwrap().into_iter().map(|n| n.id).collect();
        assert_eq!(ids, ["t", "f", "a", "b", "s"], "deleted and orphaned nodes are left out");
    }

    #[test]
    fn folders_and_smart_lists_are_told_apart() {
        let nodes = tree(&db()).unwrap();
        let get = |id: &str| nodes.iter().find(|n| n.id == id).unwrap();
        assert!(get("f").folder && !get("f").smart);
        // -128 is a playlist with tracks, not a folder.
        assert!(!get("b").folder);
        assert!(get("s").smart);
        assert_eq!(get("a").parent.as_deref(), Some("f"));
        assert_eq!(get("t").parent, None);
    }

    #[test]
    fn a_playlist_lists_its_tracks_in_order() {
        assert_eq!(members(&db(), "a").unwrap(), ["t1", "t2"]);
        assert_eq!(members(&db(), "b").unwrap(), ["t1", "t3"], "deleted entries are skipped");
    }

    #[test]
    fn a_folder_lists_every_track_under_it_once() {
        assert_eq!(members(&db(), "f").unwrap(), ["t1", "t2", "t3"]);
    }
}
