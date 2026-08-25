//! The rekordbox library, loaded once into rows anything can filter.
//!
//! Not TUI-specific, though the TUI is its heaviest user: `dump` and
//! `shop --match` want the same rows, and a second loader would be a second set
//! of answers to the same question.

use anyhow::Result;
use rusqlite::Row;

use crate::analysis::normalize_title;
use crate::db::MasterDb;
use crate::query::Fields;

/// A single row in the cached track list. Built once per load; recomputed only
/// after a successful apply batch.
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

    // Derived once at load — keeps the filter hot path branch-free:
    pub norm_title: String,
    pub locked: bool,
    pub is_unlocked_cueless_audio: bool,
    /// Lowercased `"{title} {artist}"` for substring search.
    pub search_blob: String,
    /// Lowercased folder-qualified playlist paths this track is in, one per
    /// line (`"jack night/jn4"`). Joined rather than kept as a list so a `p:`
    /// term stays one substring test per row, like `search_blob`.
    pub playlist_blob: String,
    /// Space-padded keywords for `is:` / `has:` / `type:`.
    pub tags: String,
}

const AUDIO_FILE_TYPES: &[i64] = &[0, 1, 4, 5, 11];

impl TrackRow {
    /// `pub(crate)` for the sake of test fixtures in the modules that filter
    /// these rows; `load_rows` is the only caller that matters.
    pub(crate) fn from_db(
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
        let search_blob = crate::query::text_blob(&title, &artist);
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
            playlist_blob: String::new(),
            tags: String::new(),
        }
    }

    /// The haystacks the filter language searches.
    pub fn fields(&self) -> Fields<'_> {
        Fields {
            text: &self.search_blob,
            playlists: &self.playlist_blob,
            tags: &self.tags,
            bpm: self.bpm,
            length: self.length,
        }
    }

    /// A row with just an identity and a title, for tests that only care about
    /// which rows a rule picks.
    #[cfg(test)]
    pub(crate) fn stub(id: &str, title: &str) -> Self {
        Self::from_db(
            id.to_string(),
            Some(title.to_string()),
            None,
            None,
            None,
            None,
            None,
            0,
        )
    }

    #[cfg(test)]
    pub(crate) fn with_playlists(mut self, paths: &[&str]) -> Self {
        self.playlist_blob = paths.join("\n").to_lowercase();
        self
    }

    #[cfg(test)]
    pub(crate) fn with_tags(mut self, tags: &[&str]) -> Self {
        self.tags = format!(" {} ", tags.join(" "));
        self
    }
}

/// Re-issue the full track-list query. ~3600 rows, sub-100ms.
pub fn load_rows(db: &MasterDb) -> Result<Vec<TrackRow>> {
    let sql = "
        SELECT c.ID, c.Title, c.BPM, c.Length, c.Analysed, c.FileType,
               c.FolderPath, c.ServiceID, a.Name AS Artist,
               (SELECT COUNT(*) FROM djmdCue
                 WHERE ContentID = c.ID
                   AND (rb_local_deleted = 0 OR rb_local_deleted IS NULL)) AS cue_count
        FROM djmdContent c
        LEFT JOIN djmdArtist a ON a.ID = c.ArtistID
        WHERE c.rb_local_deleted = 0 OR c.rb_local_deleted IS NULL
        ORDER BY c.Title COLLATE NOCASE";

    let mut stmt = db.conn.prepare(sql)?;
    let rows = stmt.query_map([], |r: &Row<'_>| {
        let mut row = TrackRow::from_db(
            r.get("ID")?,
            r.get("Title")?,
            r.get("Artist")?,
            r.get("BPM")?,
            r.get("Length")?,
            r.get("Analysed")?,
            r.get("FileType")?,
            r.get("cue_count")?,
        );
        let path = r.get::<_, Option<String>>("FolderPath")?;
        let path = path.as_deref();
        let origin = crate::format::origin(row.file_type, path, r.get("ServiceID")?);
        row.tags = crate::format::track_tags(crate::format::TrackFacts {
            origin,
            file_type: row.file_type,
            cue_count: row.cue_count,
            locked: row.locked,
            present: crate::presence::check(origin, path),
        });
        Ok(row)
    })?;
    let mut out = rows.collect::<rusqlite::Result<Vec<_>>>()?;

    // A rekordbox version without the playlist tables costs `p:` filtering, not
    // the track list.
    let mut playlists = crate::playlists::blobs_by_track(db).unwrap_or_default();
    for row in &mut out {
        row.playlist_blob = playlists.remove(&row.id).unwrap_or_default();
    }
    Ok(out)
}
