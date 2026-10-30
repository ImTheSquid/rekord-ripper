//! The library as a sidebar sees it: smart collections, playlists, and what
//! each track could be paired with.
//!
//! Collections are decided here rather than in a frontend so every frontend
//! counts them the same way.

use std::collections::{HashMap, HashSet};

use crate::acquire::shop::RankedOffer;
use crate::format::Origin;
use crate::library::TrackRow;
use crate::pending::Entry;
use crate::query::Query;

use super::Session;
use super::filter::fuzzy_match;

/// Seconds two lengths may differ by and still be the same recording; the
/// CLI's `--duration-tol-secs` default.
pub(super) const DURATION_TOL_SECS: i64 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Collection {
    Library,
    /// Streams and lossy files: worth shopping for.
    NeedsUpgrade,
    /// Audio files with no cues and no analysis lock.
    Unanalysed,
    /// Local files that are not on disk.
    Missing,
    /// Unanalysed tracks with exactly one analysed twin, as `copy --auto` finds.
    SuggestedTransfers,
}

impl Collection {
    pub const ALL: [Self; 5] = [
        Self::Library,
        Self::NeedsUpgrade,
        Self::Unanalysed,
        Self::Missing,
        Self::SuggestedTransfers,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Self::Library => "Library",
            Self::NeedsUpgrade => "Needs Upgrade",
            Self::Unanalysed => "Unanalysed",
            Self::Missing => "Missing",
            Self::SuggestedTransfers => "Suggested Transfers",
        }
    }

    /// `suggested` holds the destination ids of the auto matches.
    fn contains(self, row: &TrackRow, suggested: &HashSet<String>) -> bool {
        match self {
            Self::Library => true,
            Self::NeedsUpgrade => row.origin == Origin::Stream || row.lossless() == Some(false),
            Self::Unanalysed => row.is_unlocked_cueless_audio,
            Self::Missing => row.present == Some(false),
            Self::SuggestedTransfers => suggested.contains(&row.id),
        }
    }
}

/// What the library table is showing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Scope {
    Collection(Collection),
    /// A `djmdPlaylist` id; a folder shows everything under it.
    Playlist(String),
}

/// Which way analysis would go between a track and its twin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// From the selected track onto the twin.
    To,
    /// From the twin onto the selected track.
    From,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Suggestion {
    pub id: String,
    pub direction: Direction,
}

/// Analysis worth copying: what `find_auto_matches` treats as a source.
fn analysed(row: &TrackRow) -> bool {
    row.locked || row.cue_count > 0
}

/// Indices into `rows` matching `query`, limited to `members` when given and
/// in that order, otherwise to `collection` in library order.
fn scoped(
    rows: &[TrackRow],
    collection: Collection,
    members: Option<&[String]>,
    suggested: &HashSet<String>,
    query: &str,
) -> Vec<usize> {
    let q = Query::parse(query);
    let matches = |r: &TrackRow| q.matches(r.fields());
    match members {
        Some(ids) => {
            let index: HashMap<&str, usize> = rows
                .iter()
                .enumerate()
                .map(|(i, r)| (r.id.as_str(), i))
                .collect();
            ids.iter()
                .filter_map(|id| index.get(id.as_str()).copied())
                .filter(|&i| matches(&rows[i]))
                .collect()
        }
        None => rows
            .iter()
            .enumerate()
            .filter(|(_, r)| collection.contains(r, suggested) && matches(r))
            .map(|(i, _)| i)
            .collect(),
    }
}

/// Twins of `row` with analysis to give or take. Two analysed twins go the way
/// of the one with more cues; a tie, or neither analysed, is no suggestion.
fn twins(rows: &[TrackRow], row: &TrackRow) -> Vec<Suggestion> {
    rows.iter()
        .filter(|r| fuzzy_match(row, r, DURATION_TOL_SECS))
        .filter_map(|r| {
            let direction = match (analysed(row), analysed(r)) {
                (true, false) => Direction::To,
                (false, true) => Direction::From,
                (true, true) if row.cue_count > r.cue_count => Direction::To,
                (true, true) if row.cue_count < r.cue_count => Direction::From,
                _ => return None,
            };
            Some(Suggestion {
                id: r.id.clone(),
                direction,
            })
        })
        .collect()
}

impl Session {
    /// Indices into `rows` for the table: `scope` narrowed by `query`.
    pub fn scoped_visible(&self, scope: &Scope, query: &str) -> Vec<usize> {
        match scope {
            Scope::Collection(c) => scoped(&self.rows, *c, None, &self.suggested, query),
            Scope::Playlist(id) => {
                let members = crate::playlists::members(&self.db, id).unwrap_or_default();
                scoped(
                    &self.rows,
                    Collection::Library,
                    Some(&members),
                    &self.suggested,
                    query,
                )
            }
        }
    }

    pub fn collection_counts(&self) -> Vec<(Collection, usize)> {
        Collection::ALL
            .iter()
            .map(|&c| {
                let n = self
                    .rows
                    .iter()
                    .filter(|r| c.contains(r, &self.suggested))
                    .count();
                (c, n)
            })
            .collect()
    }

    pub fn suggestions(&self, id: &str) -> Vec<Suggestion> {
        self.row(id)
            .map(|row| twins(&self.rows, row))
            .unwrap_or_default()
    }

    /// The top-ranked offer found for `src_id`, if it has been searched.
    pub fn best_offer(&self, src_id: &str) -> Option<&RankedOffer> {
        self.shop
            .flattened()
            .find(|(g, _)| g.src_id.as_deref() == Some(src_id))
            .map(|(_, o)| o)
    }

    /// Queued downloads for `src_id`, oldest first.
    pub fn entries_for<'a>(&'a self, src_id: &'a str) -> impl Iterator<Item = &'a Entry> + 'a {
        self.queue
            .entries
            .iter()
            .filter(move |e| e.src_content_id == src_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::RowInput;

    fn row(id: &str, title: &str, cues: i64) -> TrackRow {
        TrackRow::from_db(RowInput {
            id: id.into(),
            title: Some(title.into()),
            artist: Some("Artist".into()),
            bpm: Some(12000),
            length: Some(200),
            analysed: Some(105),
            file_type: Some(5),
            cue_count: cues,
        })
    }

    fn none() -> HashSet<String> {
        HashSet::new()
    }

    #[test]
    fn needs_upgrade_is_streams_and_lossy_files() {
        let flac = row("1", "a", 0);
        let mut mp3 = row("2", "b", 0);
        mp3.file_type = Some(1);
        let mut stream = row("3", "c", 0);
        stream.origin = Origin::Stream;
        let mut cloud_flac = row("4", "d", 0);
        cloud_flac.origin = Origin::Cloud;
        let rows = [flac, mp3, stream, cloud_flac];
        let got = scoped(&rows, Collection::NeedsUpgrade, None, &none(), "");
        assert_eq!(got, [1, 2]);
    }

    #[test]
    fn missing_is_only_files_known_to_be_gone() {
        let mut gone = row("1", "a", 0);
        gone.present = Some(false);
        let mut here = row("2", "b", 0);
        here.present = Some(true);
        let unknown = row("3", "c", 0);
        let rows = [gone, here, unknown];
        assert_eq!(scoped(&rows, Collection::Missing, None, &none(), ""), [0]);
    }

    #[test]
    fn a_playlist_keeps_its_order_and_skips_unknown_ids() {
        let rows = [
            row("1", "apple", 0),
            row("2", "banana", 0),
            row("3", "apricot", 0),
        ];
        let members = ["3".to_string(), "gone".into(), "1".into(), "2".into()];
        let got = scoped(&rows, Collection::Library, Some(&members), &none(), "");
        assert_eq!(got, [2, 0, 1]);
        let got = scoped(&rows, Collection::Library, Some(&members), &none(), "ap");
        assert_eq!(got, [2, 0], "the query still narrows a playlist");
    }

    #[test]
    fn suggested_transfers_are_the_auto_match_destinations() {
        let rows = [row("1", "a", 0), row("2", "b", 0)];
        let suggested = HashSet::from(["2".to_string()]);
        assert_eq!(
            scoped(&rows, Collection::SuggestedTransfers, None, &suggested, ""),
            [1]
        );
    }

    #[test]
    fn twins_point_from_the_analysed_side() {
        let rows = [
            row("cued", "Song", 4),
            row("bare", "Song", 0),
            row("other", "Song", 0),
            row("more", "Song", 6),
            row("unrelated", "Else", 9),
        ];
        let from_bare = twins(&rows, &rows[1]);
        assert_eq!(
            from_bare,
            [
                Suggestion {
                    id: "cued".into(),
                    direction: Direction::From
                },
                Suggestion {
                    id: "more".into(),
                    direction: Direction::From
                },
            ],
            "two unanalysed twins have nothing to give each other"
        );
        let from_cued = twins(&rows, &rows[0]);
        assert!(from_cued.contains(&Suggestion {
            id: "bare".into(),
            direction: Direction::To
        }));
        assert!(from_cued.contains(&Suggestion {
            id: "more".into(),
            direction: Direction::From
        }));
    }
}
