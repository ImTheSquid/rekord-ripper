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

    let paths: HashMap<&str, String> = nodes
        .keys()
        .map(|id| (id.as_str(), path_of(&nodes, id)))
        .collect();

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
