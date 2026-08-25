//! How the transfer screen's two columns narrow the library.
//!
//! The rows themselves, and loading them, live in `crate::library` — anything
//! that filters tracks wants those, TUI or not. What is left here is genuinely
//! about this screen: a destination cannot be the current source, and the auto
//! and fuzzy toggles only mean something next to a chosen source.

use crate::analysis::artist_matches;
use crate::library::TrackRow;
use crate::query::Query;

#[cfg(test)]
use crate::library::RowInput;

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

    /// An ordinary track: unlocked, cueless, an audio file, 120.00 BPM, 200s.
    /// Tests override only the fields whose rule they are exercising.
    fn input(id: &str, title: &str, artist: &str) -> RowInput {
        RowInput {
            id: id.to_string(),
            title: Some(title.to_string()),
            artist: Some(artist.to_string()),
            bpm: Some(12000),
            length: Some(200),
            analysed: Some(UNLOCKED),
            file_type: Some(AUDIO),
            cue_count: 0,
        }
    }

    fn row(id: &str, title: &str, artist: &str) -> TrackRow {
        TrackRow::from_db(input(id, title, artist))
    }

    /// `Analysed` with bit 7 set, and without.
    const LOCKED: i64 = 233;
    const UNLOCKED: i64 = 105;
    /// A FLAC, and rekordbox's "this is a stream" file type.
    const AUDIO: i64 = 5;
    const STREAM: i64 = 19;

    #[test]
    fn substring_search_is_case_insensitive_on_title_and_artist() {
        let rows = vec![row("1", "Apple", "Banana"), row("2", "Orange", "Tangerine")];
        assert_eq!(src_visible(&rows, "apple"), vec![0]);
        assert_eq!(src_visible(&rows, "BANANA"), vec![0]);
        assert_eq!(src_visible(&rows, "tang"), vec![1]);
        assert_eq!(src_visible(&rows, ""), vec![0, 1]);
        assert_eq!(src_visible(&rows, "  "), vec![0, 1]);
    }

    #[test]
    fn playlist_terms_filter_both_columns() {
        let rows = vec![
            row("1", "Apple", "Banana").with_playlists(&["Jack Night/JN4"]),
            row("2", "Apple", "Cherry").with_playlists(&["Archive"]),
            row("3", "Orange", "Banana"),
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
            row("1", "Apple", "Banana")
                .with_playlists(&["Jack Night/JN4"])
                .with_tags(&["local", "present", "flac", "lossless"]),
            row("2", "Apple", "Cherry")
                .with_playlists(&["Jack Night/JN4"])
                .with_tags(&["stream"]),
            row("3", "Orange", "Banana").with_tags(&["cloud", "flac"]),
            row("4", "Pear", "Damson").with_tags(&["local", "missing", "flac"]),
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
            row("1", "Track", "A"), // eligible
            TrackRow::from_db(RowInput {
                cue_count: 3,
                ..input("2", "Track", "B")
            }),
            TrackRow::from_db(RowInput {
                analysed: Some(LOCKED),
                ..input("3", "Track", "C")
            }),
            TrackRow::from_db(RowInput {
                file_type: Some(STREAM),
                ..input("4", "Track", "D")
            }),
        ];
        assert_eq!(dst_visible(&rows, "", true, None, false, 1), vec![0]);
        assert_eq!(
            dst_visible(&rows, "", false, None, false, 1),
            vec![0, 1, 2, 3]
        );
    }

    #[test]
    fn fuzzy_from_src_narrows_to_normalized_title_and_length() {
        // Only length is compared, so the BPMs differ on purpose: a fuzzy match
        // must not depend on them.
        let at = |id, title, artist, length| RowInput {
            length: Some(length),
            ..input(id, title, artist)
        };
        let src = TrackRow::from_db(RowInput {
            bpm: Some(14600),
            cue_count: 4,
            file_type: Some(STREAM),
            ..at("1", "Ritual Pharmacy", "porf0d", 221)
        });
        let rows = vec![
            // Matches: the parenthetical is stripped from the title.
            TrackRow::from_db(RowInput {
                bpm: Some(15000),
                ..at("2", "Ritual Pharmacy (Edit)", "porf0d", 221)
            }),
            TrackRow::from_db(at("3", "Other Song", "porf0d", 221)), // wrong title
            TrackRow::from_db(at("4", "Ritual Pharmacy", "Different", 221)), // wrong artist
            TrackRow::from_db(at("5", "Ritual Pharmacy", "porf0d", 230)), // length too far
        ];
        let vis = dst_visible(&rows, "", false, Some(&src), true, 1);
        assert_eq!(vis, vec![0]); // only row index 0 (id=2) matches
    }

    #[test]
    fn src_is_excluded_from_dst_even_when_fuzzy_off() {
        let src = row("1", "Same", "A");
        let rows = vec![
            row("1", "Same", "A"), // the src itself
            row("2", "Other", "A"),
        ];
        let vis = dst_visible(&rows, "", false, Some(&src), false, 1);
        assert_eq!(vis, vec![1]);
    }

    #[test]
    fn filters_compose_as_and() {
        let src = TrackRow::from_db(RowInput {
            cue_count: 4,
            file_type: Some(STREAM),
            ..input("1", "Foo", "Bar")
        });
        let rows = vec![
            row("2", "Foo", "Bar"), // matches all
            TrackRow::from_db(RowInput {
                cue_count: 3,
                ..input("3", "Foo", "Bar")
            }), // fails auto
            TrackRow::from_db(RowInput {
                analysed: Some(LOCKED),
                ..input("4", "Foo", "Bar")
            }), // fails auto (locked)
            row("5", "Baz", "Bar"), // fails fuzzy and text
            row("6", "Foo", "Bar"), // matches all
        ];
        // Text filter: just "Foo".
        let vis = dst_visible(&rows, "foo", true, Some(&src), true, 1);
        assert_eq!(vis, vec![0, 4]);
    }
}
