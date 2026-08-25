//! How the transfer screen's two columns narrow the library.
//!
//! The rows themselves, and loading them, live in `crate::library` — anything
//! that filters tracks wants those, TUI or not. What is left here is genuinely
//! about this screen: a destination cannot be the current source, and the auto
//! and fuzzy toggles only mean something next to a chosen source.

use crate::analysis::artist_matches;
use crate::library::TrackRow;
use crate::query::Query;

/// Filter composition for the destination column. AND of four predicates:
/// typed search, auto-mode, fuzzy-match-from-source, and "not the current src"
/// (you can't copy a track onto itself, so it never belongs in the dst list).
pub fn dst_visible(
    rows: &[TrackRow],
    query: &str,
    auto: bool,
    src: Option<&TrackRow>,
    fuzzy_from_src: bool,
    duration_tol_secs: i64,
) -> Vec<usize> {
    let q = Query::parse(query);
    let src_id = src.map(|s| s.id.as_str());
    rows.iter()
        .enumerate()
        .filter(|(_, r)| src_id.is_none_or(|id| r.id != id))
        .filter(|(_, r)| q.matches(r.fields()))
        .filter(|(_, r)| !auto || r.is_unlocked_cueless_audio)
        .filter(|(_, r)| match (fuzzy_from_src, src) {
            (true, Some(s)) => fuzzy_match(s, r, duration_tol_secs),
            _ => true,
        })
        .map(|(i, _)| i)
        .collect()
}

/// Source column filter: just the typed search.
pub fn src_visible(rows: &[TrackRow], query: &str) -> Vec<usize> {
    let q = Query::parse(query);
    rows.iter()
        .enumerate()
        .filter(|(_, r)| q.matches(r.fields()))
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
    fn playlist_terms_filter_both_columns() {
        let rows = vec![
            row("1", "Apple", "Banana", 12000, 200, 0, 5, false)
                .with_playlists(&["Jack Night/JN4"]),
            row("2", "Apple", "Cherry", 12000, 200, 0, 5, false).with_playlists(&["Archive"]),
            row("3", "Orange", "Banana", 12000, 200, 0, 5, false),
        ];
        assert_eq!(src_visible(&rows, "p:jn4"), vec![0]);
        assert_eq!(src_visible(&rows, "p:\"jack night\" apple"), vec![0]);
        assert_eq!(src_visible(&rows, "apple"), vec![0, 1]);
        assert_eq!(src_visible(&rows, "p:jn4 orange"), Vec::<usize>::new());
        assert_eq!(
            dst_visible(&rows, "p:archive", false, None, false, 1),
            vec![1]
        );
    }

    #[test]
    fn keyword_terms_filter_the_columns_too() {
        let rows = vec![
            row("1", "Apple", "Banana", 12000, 200, 0, 5, false)
                .with_playlists(&["Jack Night/JN4"])
                .with_tags(&["local", "present", "flac", "lossless"]),
            row("2", "Apple", "Cherry", 12000, 200, 0, 19, false)
                .with_playlists(&["Jack Night/JN4"])
                .with_tags(&["stream"]),
            row("3", "Orange", "Banana", 12000, 200, 0, 5, false).with_tags(&["cloud", "flac"]),
            row("4", "Pear", "Damson", 12000, 200, 0, 5, false)
                .with_tags(&["local", "missing", "flac"]),
        ];
        assert_eq!(src_visible(&rows, "p:jn4 is:stream"), vec![1]);
        assert_eq!(src_visible(&rows, "-is:stream"), vec![0, 2, 3]);
        assert_eq!(src_visible(&rows, "type:flac apple"), vec![0]);
        // Presence is its own axis. A stream has no file and a cloud path
        // cannot be checked, so neither is swept up by `is:missing`.
        assert_eq!(src_visible(&rows, "is:missing"), vec![3]);
        assert_eq!(src_visible(&rows, "is:present"), vec![0]);
        assert_eq!(src_visible(&rows, "is:local -is:present"), vec![3]);
        assert_eq!(
            dst_visible(&rows, "is:cloud", false, None, false, 1),
            vec![2]
        );
    }

    #[test]
    fn dst_auto_mode_filters_to_unlocked_cueless_audio() {
        let rows = vec![
            row("1", "Track", "A", 12000, 200, 0, 5, false),  // eligible
            row("2", "Track", "B", 12000, 200, 3, 5, false),  // has cues
            row("3", "Track", "C", 12000, 200, 0, 5, true),   // locked
            row("4", "Track", "D", 12000, 200, 0, 19, false), // streaming
        ];
        assert_eq!(dst_visible(&rows, "", true, None, false, 1), vec![0]);
        assert_eq!(
            dst_visible(&rows, "", false, None, false, 1),
            vec![0, 1, 2, 3]
        );
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
        let vis = dst_visible(&rows, "", false, Some(&src), true, 1);
        assert_eq!(vis, vec![0]); // only row index 0 (id=2) matches
    }

    #[test]
    fn src_is_excluded_from_dst_even_when_fuzzy_off() {
        let src = row("1", "Same", "A", 12000, 200, 0, 5, false);
        let rows = vec![
            row("1", "Same", "A", 12000, 200, 0, 5, false), // the src itself
            row("2", "Other", "A", 12000, 200, 0, 5, false),
        ];
        let vis = dst_visible(&rows, "", false, Some(&src), false, 1);
        assert_eq!(vis, vec![1]);
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
        let vis = dst_visible(&rows, "foo", true, Some(&src), true, 1);
        assert_eq!(vis, vec![0, 4]);
    }
}
