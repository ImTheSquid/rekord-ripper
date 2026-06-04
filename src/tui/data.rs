use anyhow::Result;
use rusqlite::Row;

use crate::analysis::{artist_matches, normalize_title};
use crate::db::MasterDb;

/// A single row in the TUI's cached track list. Built once per `reload_rows`
/// call; recomputed only after a successful apply batch.
#[derive(Clone, Debug)]
pub struct TrackRow {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub bpm: Option<i64>,
    pub length: Option<i64>,
    pub cue_count: i64,
    pub analysed: i64,
    pub file_type: Option<i64>,

    // Derived once at load — keep TUI hot path branch-free:
    pub norm_title: String,
    pub locked: bool,
    pub is_unlocked_cueless_audio: bool,
    /// Lowercased `"{title} {artist}"` for substring search.
    pub search_blob: String,
}

const AUDIO_FILE_TYPES: &[i64] = &[0, 1, 4, 5, 11];

impl TrackRow {
    fn from_db(
        id: String,
        title: Option<String>,
        artist: Option<String>,
        bpm: Option<i64>,
        length: Option<i64>,
        analysed: Option<i64>,
        file_type: Option<i64>,
        cue_count: i64,
    ) -> Self {
        let title = title.unwrap_or_default();
        let artist = artist.unwrap_or_default();
        let norm_title = normalize_title(&title);
        let analysed = analysed.unwrap_or(0);
        let locked = analysed & 0x80 != 0;
        let is_audio = file_type
            .map(|ft| AUDIO_FILE_TYPES.contains(&ft))
            .unwrap_or(false);
        let is_unlocked_cueless_audio = !locked && cue_count == 0 && is_audio;
        let search_blob = format!("{} {}", title.to_lowercase(), artist.to_lowercase());
        Self {
            id,
            title,
            artist,
            bpm,
            length,
            cue_count,
            analysed,
            file_type,
            norm_title,
            locked,
            is_unlocked_cueless_audio,
            search_blob,
        }
    }
}

/// Re-issue the full track-list query. ~3600 rows, sub-100ms.
pub fn load_rows(db: &MasterDb) -> Result<Vec<TrackRow>> {
    let sql = "
        SELECT c.ID, c.Title, c.BPM, c.Length, c.Analysed, c.FileType,
               a.Name AS Artist,
               (SELECT COUNT(*) FROM djmdCue
                 WHERE ContentID = c.ID
                   AND (rb_local_deleted = 0 OR rb_local_deleted IS NULL)) AS cue_count
        FROM djmdContent c
        LEFT JOIN djmdArtist a ON a.ID = c.ArtistID
        ORDER BY c.Title COLLATE NOCASE";

    let mut stmt = db.conn.prepare(sql)?;
    let rows = stmt.query_map([], |r: &Row<'_>| {
        Ok(TrackRow::from_db(
            r.get("ID")?,
            r.get("Title")?,
            r.get("Artist")?,
            r.get("BPM")?,
            r.get("Length")?,
            r.get("Analysed")?,
            r.get("FileType")?,
            r.get("cue_count")?,
        ))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Filter composition for the destination column. AND of three independent
/// predicates: typed search, auto-mode, fuzzy-match-from-source.
pub fn dst_visible(
    rows: &[TrackRow],
    query: &str,
    auto: bool,
    fuzzy_src: Option<&TrackRow>,
    duration_tol_secs: i64,
) -> Vec<usize> {
    let q = query.trim().to_lowercase();
    rows.iter()
        .enumerate()
        .filter(|(_, r)| q.is_empty() || r.search_blob.contains(&q))
        .filter(|(_, r)| !auto || r.is_unlocked_cueless_audio)
        .filter(|(_, r)| match fuzzy_src {
            None => true,
            Some(src) => fuzzy_match(src, r, duration_tol_secs),
        })
        .map(|(i, _)| i)
        .collect()
}

/// Source column filter: just the typed search.
pub fn src_visible(rows: &[TrackRow], query: &str) -> Vec<usize> {
    let q = query.trim().to_lowercase();
    rows.iter()
        .enumerate()
        .filter(|(_, r)| q.is_empty() || r.search_blob.contains(&q))
        .map(|(i, _)| i)
        .collect()
}

/// Per-pair fuzzy-match predicate: same normalized title, artist substring
/// match either direction, length within ±tol seconds. Mirrors
/// `analysis::find_auto_matches` per-dst gating.
pub fn fuzzy_match(src: &TrackRow, dst: &TrackRow, tol_secs: i64) -> bool {
    if src.id == dst.id {
        return false;
    }
    if src.norm_title.is_empty() || dst.norm_title.is_empty() {
        return false;
    }
    if src.norm_title != dst.norm_title {
        return false;
    }
    match (src.length, dst.length) {
        (Some(a), Some(b)) if (a - b).abs() <= tol_secs => {}
        _ => return false,
    }
    artist_matches(Some(&src.artist), Some(&dst.artist))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, title: &str, artist: &str, bpm: i64, length: i64, cue_count: i64, file_type: i64, locked: bool) -> TrackRow {
        TrackRow::from_db(
            id.to_string(),
            Some(title.to_string()),
            Some(artist.to_string()),
            Some(bpm),
            Some(length),
            Some(if locked { 233 } else { 105 }),
            Some(file_type),
            cue_count,
        )
    }

    #[test]
    fn substring_search_is_case_insensitive_on_title_and_artist() {
        let rows = vec![
            row("1", "Apple", "Banana", 12000, 200, 0, 5, false),
            row("2", "Orange", "Tangerine", 12000, 200, 0, 5, false),
        ];
        assert_eq!(src_visible(&rows, "apple"), vec![0]);
        assert_eq!(src_visible(&rows, "BANANA"), vec![0]);
        assert_eq!(src_visible(&rows, "tang"), vec![1]);
        assert_eq!(src_visible(&rows, ""), vec![0, 1]);
        assert_eq!(src_visible(&rows, "  "), vec![0, 1]);
    }

    #[test]
    fn dst_auto_mode_filters_to_unlocked_cueless_audio() {
        let rows = vec![
            row("1", "Track", "A", 12000, 200, 0, 5, false),  // eligible
            row("2", "Track", "B", 12000, 200, 3, 5, false),  // has cues
            row("3", "Track", "C", 12000, 200, 0, 5, true),   // locked
            row("4", "Track", "D", 12000, 200, 0, 19, false), // streaming
        ];
        assert_eq!(dst_visible(&rows, "", true, None, 1), vec![0]);
        assert_eq!(dst_visible(&rows, "", false, None, 1), vec![0, 1, 2, 3]);
    }

    #[test]
    fn fuzzy_from_src_narrows_to_normalized_title_and_length() {
        let src = row("1", "Ritual Pharmacy", "porf0d", 14600, 221, 4, 19, false);
        let rows = vec![
            row("2", "Ritual Pharmacy (Edit)", "porf0d", 15000, 221, 0, 5, false), // matches: parens stripped
            row("3", "Other Song", "porf0d", 14600, 221, 0, 5, false),              // wrong title
            row("4", "Ritual Pharmacy", "Different", 14600, 221, 0, 5, false),      // wrong artist
            row("5", "Ritual Pharmacy", "porf0d", 14600, 230, 0, 5, false),         // length too far
        ];
        let vis = dst_visible(&rows, "", false, Some(&src), 1);
        assert_eq!(vis, vec![0]); // only row index 0 (id=2) matches
    }

    #[test]
    fn filters_compose_as_and() {
        let src = row("1", "Foo", "Bar", 12000, 200, 4, 19, false);
        let rows = vec![
            row("2", "Foo", "Bar", 12000, 200, 0, 5, false),    // matches all
            row("3", "Foo", "Bar", 12000, 200, 3, 5, false),    // fails auto
            row("4", "Foo", "Bar", 12000, 200, 0, 5, true),     // fails auto (locked)
            row("5", "Baz", "Bar", 12000, 200, 0, 5, false),    // fails fuzzy
            row("6", "Foo", "Bar", 12000, 200, 0, 5, false),    // matches; will fail text
        ];
        // Text filter: just "Foo".
        let vis = dst_visible(&rows, "foo", true, Some(&src), 1);
        assert_eq!(vis, vec![0, 4]);
    }
}
