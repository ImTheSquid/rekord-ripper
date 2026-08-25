//! Creating `djmdContent` rows, so an acquired file lands in rekordbox without
//! the manual drag.
//!
//! # What this actually costs
//!
//! Nothing else in this crate creates track rows, so this is the one place that
//! adds a track to your library. Worth being precise about the risk rather than
//! vague: `apply_plan` already inserts rows into five tables with `rb_local_usn`
//! set and clears `rb_local_synced` on `djmdContent`, so every `cp` already
//! writes rows the cloud agent will push. This is not a new category of thing.
//!
//! What *is* new is that a track row points at a **file**. Under Cloud Library
//! Sync the agent may upload that audio and rewrite `FolderPath` to a
//! `/contents_<dbid>/…` form. That is benign but it consumes quota and
//! invalidates any path we stored, so a pending pairing matches on
//! `OrgFolderPath` too.
//!
//! # Why two transactions rather than one
//!
//! The insert commits separately from the analysis transfer that follows it. A
//! failure in between leaves a bare, unanalysed track row — which is *exactly*
//! what dragging the file into rekordbox by hand produces. It is a normal,
//! recoverable state rather than corruption, and [`tombstone`] removes it
//! properly if you want it gone.
//!
//! # Why deletion is a tombstone
//!
//! A plain `DELETE` on a synced row would leave your other devices with a row
//! pointing at a file they do not have. Rekordbox marks removals with
//! `rb_local_deleted = 1` plus a USN bump so the deletion itself propagates, and
//! so does this.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use rusqlite::{OptionalExtension, params};

use crate::analysis::{derive_anlz_path, random_numeric_id};
use crate::audio::AudioInfo;
use crate::db::{MasterDb, now_db_string};

/// Everything needed to write one track row.
#[derive(Debug, Clone)]
pub struct NewContent {
    pub id: String,
    pub uuid: String,
    pub folder_path: String,
    pub file_name: String,
    pub title: String,
    pub artist_id: Option<String>,
    pub album_id: Option<String>,
    pub genre_id: Option<String>,
    /// Lookup rows that do not exist yet and must be created alongside the track.
    pub new_artist: Option<NewLookup>,
    pub new_album: Option<NewLookup>,
    pub new_genre: Option<NewLookup>,
    pub length: i64,
    pub file_type: i64,
    pub file_size: i64,
    pub sample_rate: Option<i64>,
    pub bit_depth: Option<i64>,
    pub bit_rate: Option<i64>,
    /// Copied from this device's most recent local row. Opaque — see the note in
    /// [`content_link`].
    pub content_link: Option<i64>,
    pub master_db_id: Option<String>,
    pub device_id: Option<String>,

    /// Tag-derived fields with a direct column on the track row.
    pub comment: Option<String>,
    pub release_year: Option<i64>,
    pub track_no: Option<i64>,
    pub disc_no: Option<i64>,
}

/// A row to mint in one of rekordbox's name-keyed lookup tables.
///
/// `djmdArtist`, `djmdAlbum` and `djmdGenre` share the same shape — id, name,
/// uuid and the standard sync columns — so one type covers all three.
#[derive(Debug, Clone)]
pub struct NewLookup {
    pub table: &'static str,
    pub id: String,
    pub uuid: String,
    pub name: String,
}

/// Backwards-compatible alias for the artist case.
pub type NewArtist = NewLookup;

/// This device's identity, as rekordbox records it.
fn device_identity(db: &MasterDb) -> Result<(Option<String>, Option<String>)> {
    db.conn
        .query_row("SELECT DBID, DeviceID FROM djmdProperty LIMIT 1", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .map_err(|e| anyhow!("reading djmdProperty: {e}"))
}

/// `ContentLink` from this device's most recent local row.
///
/// The column is opaque: its values are shared across hundreds of rows (1261 rows
/// share one value in this library), so it is not a per-track identifier —
/// plausibly a device or import-batch discriminator. Copying the prevailing local
/// value is the closest thing to "what rekordbox would have written"; guessing a
/// fresh number would be worse.
fn content_link(db: &MasterDb) -> Result<Option<i64>> {
    Ok(db
        .conn
        .query_row(
            "SELECT c.ContentLink FROM djmdContent c, djmdProperty p
             WHERE c.DeviceID = p.DeviceID AND c.ServiceID = 0
               AND c.rb_local_deleted = 0 AND c.ContentLink IS NOT NULL
             ORDER BY c.created_at DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()?)
}

/// Find an existing row by exact name, or describe the one to mint.
///
/// Reusing an existing row matters: minting a second "Burial" would split the
/// artist in rekordbox's browser.
fn resolve_lookup(
    db: &MasterDb,
    table: &'static str,
    name: Option<&str>,
) -> Result<(Option<String>, Option<NewLookup>)> {
    let Some(name) = name.map(str::trim).filter(|n| !n.is_empty()) else {
        return Ok((None, None));
    };
    let existing: Option<String> = db
        .conn
        .query_row(
            &format!(
                "SELECT ID FROM {table}
                 WHERE Name = ?1 AND (rb_local_deleted = 0 OR rb_local_deleted IS NULL)
                 LIMIT 1"
            ),
            params![name],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(id) = existing {
        return Ok((Some(id), None));
    }
    let id = random_numeric_id();
    Ok((
        Some(id.clone()),
        Some(NewLookup {
            table,
            id,
            uuid: uuid::Uuid::new_v4().to_string(),
            name: name.to_string(),
        }),
    ))
}

/// True when a row already references this path, so we never create a duplicate.
pub fn existing_row_for_path(db: &MasterDb, path: &Path) -> Result<Option<String>> {
    let p = path.to_string_lossy().to_string();
    Ok(db
        .conn
        .query_row(
            "SELECT ID FROM djmdContent
             WHERE (rb_local_deleted = 0 OR rb_local_deleted IS NULL)
               AND (FolderPath = ?1 OR OrgFolderPath = ?1)
             LIMIT 1",
            params![p],
            |r| r.get(0),
        )
        .optional()?)
}

/// Build the row for `path` without writing anything.
///
/// Fails rather than inventing values for anything rekordbox needs: a file whose
/// format rekordbox cannot open, or which ffprobe cannot read, has no business
/// becoming a track row.
pub fn plan_insert(
    db: &MasterDb,
    path: &Path,
    info: &AudioInfo,
    title: Option<&str>,
    artist: Option<&str>,
) -> Result<NewContent> {
    let abs =
        std::fs::canonicalize(path).with_context(|| format!("resolving {}", path.display()))?;
    if let Some(existing) = existing_row_for_path(db, &abs)? {
        bail!(
            "{} is already in rekordbox as track {existing}",
            abs.display()
        );
    }

    let file_type = info.rekordbox_file_type(&abs).ok_or_else(|| {
        anyhow!(
            "rekordbox cannot read {} (codec {:?})",
            abs.display(),
            info.codec
        )
    })?;

    let file_name = abs
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| anyhow!("{} has no filename", abs.display()))?;

    // Explicit override, then the file's own tags, then the filename stem — which
    // is the order rekordbox itself effectively uses.
    let title = title
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .or_else(|| info.tags.title.clone())
        .unwrap_or_else(|| {
            abs.file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| file_name.clone())
        });

    let artist_name = artist
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .map(str::to_string)
        .or_else(|| info.tags.artist.clone())
        .or_else(|| info.tags.album_artist.clone());
    let (artist_id, new_artist) = resolve_lookup(db, "djmdArtist", artist_name.as_deref())?;
    let (album_id, new_album) = resolve_lookup(db, "djmdAlbum", info.tags.album.as_deref())?;
    let (genre_id, new_genre) = resolve_lookup(db, "djmdGenre", info.tags.genre.as_deref())?;

    let (master_db_id, device_id) = device_identity(db)?;

    Ok(NewContent {
        id: fresh_content_id(db)?,
        uuid: uuid::Uuid::new_v4().to_string(),
        folder_path: abs.to_string_lossy().into_owned(),
        file_name,
        title,
        artist_id,
        new_artist,
        length: info.length_secs(),
        file_type,
        file_size: info.file_size as i64,
        sample_rate: info.sample_rate,
        bit_depth: info.bit_depth,
        // Rekordbox stores 0 here for lossless; keep whatever ffprobe said for
        // lossy and 0 otherwise.
        bit_rate: info.bit_rate.map(|b| b / 1000),
        content_link: content_link(db)?,
        master_db_id,
        device_id,
        album_id,
        genre_id,
        new_album,
        new_genre,
        comment: info.tags.comment.clone(),
        release_year: info.tags.year,
        track_no: info.tags.track_no,
        disc_no: info.tags.disc_no,
    })
}

/// A `djmdContent.ID` not already in use.
fn fresh_content_id(db: &MasterDb) -> Result<String> {
    for _ in 0..64 {
        let candidate = random_numeric_id();
        let taken: Option<String> = db
            .conn
            .query_row(
                "SELECT ID FROM djmdContent WHERE ID = ?1",
                params![candidate],
                |r| r.get(0),
            )
            .optional()?;
        if taken.is_none() {
            return Ok(candidate);
        }
    }
    bail!("could not find an unused djmdContent.ID after 64 tries")
}

/// A human-readable summary, for the confirmation prompt.
///
/// Every value that will be written, so nothing is inserted that the user has not
/// seen.
pub fn render(new: &NewContent) -> String {
    let mut s = String::new();
    s.push_str("would insert into djmdContent:\n");
    let mut row = |k: &str, v: String| s.push_str(&format!("  {k:<18} {v}\n"));
    row("ID", new.id.clone());
    row("UUID", new.uuid.clone());
    row("FolderPath", new.folder_path.clone());
    row("FileNameL", new.file_name.clone());
    row("Title", new.title.clone());
    let lookup = |id: &Option<String>, minted: &Option<NewLookup>| match (id, minted) {
        (Some(id), Some(m)) => format!("{id}  (new {} row: {:?})", m.table, m.name),
        (Some(id), None) => format!("{id}  (existing)"),
        _ => "NULL".into(),
    };
    row("ArtistID", lookup(&new.artist_id, &new.new_artist));
    row("AlbumID", lookup(&new.album_id, &new.new_album));
    row("GenreID", lookup(&new.genre_id, &new.new_genre));
    row("Length", format!("{}s", new.length));
    row(
        "TrackNo / DiscNo",
        format!("{} / {}", opt(new.track_no), opt(new.disc_no)),
    );
    row("ReleaseYear", opt(new.release_year));
    if let Some(c) = &new.comment {
        row("Commnt", format!("{c:?}"));
    }
    row(
        "FileType",
        format!(
            "{} ({})",
            new.file_type,
            crate::format::file_type_name(Some(new.file_type))
        ),
    );
    row("FileSize", new.file_size.to_string());
    row("SampleRate", opt(new.sample_rate));
    row("BitDepth", opt(new.bit_depth));
    row("BitRate", opt(new.bit_rate));
    row(
        "MasterDBID",
        new.master_db_id.clone().unwrap_or("NULL".into()),
    );
    row("MasterSongID", new.id.clone());
    row("DeviceID", new.device_id.clone().unwrap_or("NULL".into()));
    row("ContentLink", opt(new.content_link));
    row("Analysed", "0  (the transfer sets this)".into());
    row("ServiceID", "0  (local file, not cloud-managed)".into());
    row(
        "rb_local_synced",
        "0  (so rekordbox syncs it like any edit)".into(),
    );
    s
}

fn opt(v: Option<i64>) -> String {
    v.map(|x| x.to_string()).unwrap_or_else(|| "NULL".into())
}

/// The record written beside the backup so an insert can be undone without
/// archaeology.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct UndoNote {
    pub content_id: String,
    pub content_uuid: String,
    pub folder_path: String,
    pub artist_id: Option<String>,
    /// Only set when we created the artist row, so undo does not remove an artist
    /// that was already there.
    pub created_artist: bool,
    pub inserted_at: String,
    pub backup: Option<String>,
}

impl UndoNote {
    /// Write next to `backup` as `<backup>.<content_id>.inserted.json`.
    ///
    /// Named per row, not per backup: one backup covers a whole batch of
    /// inserts, so a name derived from the backup alone meant each note
    /// overwrote the last and only the final row stayed undoable.
    pub fn write_beside(&self, backup: &Path) -> Result<PathBuf> {
        let mut name = backup.file_name().unwrap_or_default().to_os_string();
        name.push(format!(".{}.inserted.json", self.content_id));
        let path = backup.with_file_name(name);
        std::fs::write(&path, serde_json::to_vec_pretty(self)?)
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(path)
    }
}

/// Collapse artist/album/genre rows that several plans would each mint.
///
/// `resolve_lookup` only asks the *database* whether a name exists, so planning
/// a batch before inserting any of it means two files by the same new artist
/// each mint their own `djmdArtist` row — the split-browser outcome
/// `resolve_lookup` exists to prevent. Planning is always done for the whole
/// batch up front (the confirmation has to show every row), so the batch has to
/// be reconciled against itself before the first insert.
///
/// Matching is by exact trimmed name, mirroring `resolve_lookup`'s `Name = ?1`.
pub fn dedupe_lookups(planned: &mut [NewContent]) {
    let mut seen: HashMap<(&'static str, String), String> = HashMap::new();
    for p in planned.iter_mut() {
        // Three disjoint field pairs, so each is reconciled on its own.
        fold(&mut seen, &mut p.artist_id, &mut p.new_artist);
        fold(&mut seen, &mut p.album_id, &mut p.new_album);
        fold(&mut seen, &mut p.genre_id, &mut p.new_genre);
    }
}

/// Point `id` at the first plan to mint this name, and drop the duplicate.
fn fold(
    seen: &mut HashMap<(&'static str, String), String>,
    id: &mut Option<String>,
    minted: &mut Option<NewLookup>,
) {
    let Some(lookup) = minted.as_ref() else {
        return;
    };
    let key = (lookup.table, lookup.name.clone());
    match seen.get(&key) {
        Some(first) => {
            *id = Some(first.clone());
            *minted = None;
        }
        None => {
            seen.insert(key, lookup.id.clone());
        }
    }
}

/// Insert the row (and its artist, if new).
///
/// Takes `&mut MasterDb` and does its own transaction and USN allocation, in the
/// same shape as `apply_plan`. The caller is responsible for the backup and the
/// running-rekordbox preflight.
pub fn insert(db: &mut MasterDb, new: &NewContent) -> Result<UndoNote> {
    // Re-check inside the write path: something may have imported the file
    // between the plan and the confirmation.
    if let Some(existing) = existing_row_for_path(db, Path::new(&new.folder_path))? {
        bail!("{} was imported already as {existing}", new.folder_path);
    }

    let base_usn = db.read_local_usn()?;
    let mut next_usn = base_usn;
    let mut allocate = || {
        next_usn += 1;
        next_usn
    };
    let now = now_db_string();
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();

    let tx = db.conn.unchecked_transaction()?;

    // The three lookup tables share a column shape, so one statement shape does.
    for lookup in [&new.new_artist, &new.new_album, &new.new_genre]
        .into_iter()
        .flatten()
    {
        tx.execute(
            &format!(
                "INSERT INTO {} (ID, Name, SearchStr, UUID,
                    rb_data_status, rb_local_data_status, rb_local_deleted, rb_local_synced,
                    rb_local_usn, created_at, updated_at)
                 VALUES (?1, ?2, NULL, ?3, 256, 0, 0, 0, ?4, ?5, ?5)",
                lookup.table
            ),
            params![lookup.id, lookup.name, lookup.uuid, allocate(), now],
        )?;
    }

    // Column set and constants taken from a row rekordbox created on this device:
    // rb_data_status 256, SearchStr NULL, ExtInfo "null", ColorID/VideoAssociate
    // "0", HotCueAutoLoad/DeliveryControl "on", OrgFolderPath NULL for local files.
    // `usn` is left NULL because that counter is server-assigned.
    // Named parameters, not positional: this writes 46 columns into the user's
    // library and a misaligned `?n` would put an album name in DiscNo without
    // anything failing.
    tx.execute(
        "INSERT INTO djmdContent
           (ID, FolderPath, FileNameL, FileNameS, Title,
            ArtistID, AlbumID, GenreID,
            BPM, Length, TrackNo, DiscNo, BitRate, BitDepth, FileType, Rating,
            ReleaseYear, Commnt,
            StockDate, DateCreated, ColorID, DJPlayCount,
            MasterDBID, MasterSongID,
            AnalysisDataPath, SearchStr, FileSize, SampleRate,
            Analysed, ContentLink, HotCueAutoLoad, DeliveryControl,
            SamplerTrackInfo, SamplerPlayOffset, SamplerGain, VideoAssociate,
            LyricStatus, ServiceID, OrgFolderPath, ExtInfo, DeviceID, UUID,
            rb_data_status, rb_local_data_status, rb_local_deleted, rb_local_synced,
            usn, rb_local_usn, created_at, updated_at)
         VALUES
           (:id, :folder_path, :file_name, NULL, :title,
            :artist_id, :album_id, :genre_id,
            NULL, :length, :track_no, :disc_no, :bit_rate, :bit_depth, :file_type, 0,
            :release_year, :comment,
            :today, :today, '0', 0,
            :master_db_id, :id,
            NULL, NULL, :file_size, :sample_rate,
            0, :content_link, 'on', 'on',
            0, 0, 0.0, '0',
            0, 0, NULL, 'null', :device_id, :uuid,
            256, 0, 0, 0,
            NULL, :usn, :now, :now)",
        rusqlite::named_params! {
            ":id": new.id,
            ":folder_path": new.folder_path,
            ":file_name": new.file_name,
            ":title": new.title,
            ":artist_id": new.artist_id,
            ":album_id": new.album_id,
            ":genre_id": new.genre_id,
            ":length": new.length,
            ":track_no": new.track_no.unwrap_or(0),
            ":disc_no": new.disc_no.unwrap_or(0),
            ":bit_rate": new.bit_rate.unwrap_or(0),
            ":bit_depth": new.bit_depth,
            ":file_type": new.file_type,
            ":release_year": new.release_year,
            ":comment": new.comment,
            ":today": today,
            ":master_db_id": new.master_db_id,
            ":file_size": new.file_size,
            ":sample_rate": new.sample_rate,
            ":content_link": new.content_link,
            ":device_id": new.device_id,
            ":uuid": new.uuid,
            ":usn": allocate(),
            ":now": now,
        },
    )?;

    db.write_local_usn(next_usn)?;
    tx.commit()?;

    Ok(UndoNote {
        content_id: new.id.clone(),
        content_uuid: new.uuid.clone(),
        folder_path: new.folder_path.clone(),
        artist_id: new.artist_id.clone(),
        created_artist: new.new_artist.is_some(),
        inserted_at: now,
        backup: None,
    })
}

/// Mark an inserted row deleted, the way rekordbox does.
///
/// Not a `DELETE`: on a synced library a hard delete would leave the other
/// devices holding a row for a file they do not have. Setting
/// `rb_local_deleted = 1` with a fresh USN makes the removal itself something
/// rekordbox propagates.
pub fn tombstone(db: &mut MasterDb, content_id: &str, expect_uuid: Option<&str>) -> Result<()> {
    let found: Option<(String, Option<i64>)> = db
        .conn
        .query_row(
            "SELECT UUID, rb_local_deleted FROM djmdContent WHERE ID = ?1",
            params![content_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;

    let Some((uuid, deleted)) = found else {
        bail!("track {content_id} is not in the database");
    };
    // Guard against a recycled ID pointing at somebody else's track by now.
    if let Some(expected) = expect_uuid
        && uuid != expected
    {
        bail!(
            "track {content_id} is no longer the row that was inserted \
             (uuid {uuid} != {expected}) — refusing to touch it"
        );
    }
    if deleted == Some(1) {
        return Ok(());
    }

    let usn = db.read_local_usn()? + 1;
    let now = now_db_string();
    let tx = db.conn.unchecked_transaction()?;
    let n = tx.execute(
        "UPDATE djmdContent
         SET rb_local_deleted = 1, rb_local_synced = 0, rb_local_usn = ?2, updated_at = ?3
         WHERE ID = ?1",
        params![content_id, usn, now],
    )?;
    if n != 1 {
        bail!("expected to update 1 row, updated {n}");
    }
    db.write_local_usn(usn)?;
    tx.commit()?;
    Ok(())
}

/// The ANLZ path a freshly inserted row would use, for reporting.
pub fn anlz_path_for(new: &NewContent) -> String {
    derive_anlz_path(&new.uuid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::AudioInfo;

    fn info() -> AudioInfo {
        AudioInfo {
            duration_secs: 310.588,
            sample_rate: Some(44100),
            bit_depth: Some(16),
            channels: Some(2),
            bit_rate: Some(1_064_321),
            codec: Some("flac".into()),
            file_size: 41_330_706,
            tags: crate::audio::Tags {
                title: Some("TELL ME".into()),
                artist: Some("OJC".into()),
                album: Some("cursed003".into()),
                year: Some(2026),
                track_no: Some(8),
                ..Default::default()
            },
        }
    }

    fn content() -> NewContent {
        NewContent {
            id: "227191147".into(),
            uuid: "14cc296b-0338-49de-88fe-41662820bdc4".into(),
            folder_path: "/Users/x/Music/OJC - TELL ME.flac".into(),
            file_name: "OJC - TELL ME.flac".into(),
            title: "TELL ME".into(),
            artist_id: Some("666000868".into()),
            album_id: Some("1189904700".into()),
            genre_id: None,
            new_artist: None,
            new_album: None,
            new_genre: None,
            comment: Some("Visit https://prodojc.bandcamp.com".into()),
            release_year: Some(2026),
            track_no: Some(8),
            disc_no: None,
            length: 311,
            file_type: 5,
            file_size: 41_330_706,
            sample_rate: Some(44100),
            bit_depth: Some(16),
            bit_rate: Some(1064),
            content_link: Some(2885134),
            master_db_id: Some("2768718261".into()),
            device_id: Some("f742efc6-df09-4a29-876e-fdc38806710b".into()),
        }
    }

    /// A plan that mints a brand-new artist row, as `resolve_lookup` does when
    /// the name is not already in `djmdArtist`.
    fn minting(id: &str, artist_row_id: &str, artist: &str) -> NewContent {
        NewContent {
            id: id.into(),
            artist_id: Some(artist_row_id.into()),
            new_artist: Some(NewLookup {
                table: "djmdArtist",
                id: artist_row_id.into(),
                uuid: format!("uuid-{artist_row_id}"),
                name: artist.into(),
            }),
            ..content()
        }
    }

    #[test]
    fn one_new_artist_across_a_batch_becomes_one_row() {
        // The split-browser bug: planning happens before inserting, so both
        // files asked the database, neither saw the other, and each minted.
        let mut batch = [
            minting("1", "aaa", "Burial"),
            minting("2", "bbb", "Burial"),
            minting("3", "ccc", "Zomby"),
        ];
        dedupe_lookups(&mut batch);

        assert!(batch[0].new_artist.is_some(), "the first one still mints");
        assert!(batch[1].new_artist.is_none(), "the second must not mint");
        assert_eq!(batch[1].artist_id.as_deref(), Some("aaa"));
        // A different name is untouched.
        assert!(batch[2].new_artist.is_some());
        assert_eq!(batch[2].artist_id.as_deref(), Some("ccc"));
    }

    #[test]
    fn dedupe_keeps_the_three_lookup_kinds_apart() {
        // Same name, different table: an album called "Burial" is not the
        // artist Burial, and collapsing them would repoint the wrong column.
        let mut a = minting("1", "aaa", "Burial");
        a.new_album = Some(NewLookup {
            table: "djmdAlbum",
            id: "alb".into(),
            uuid: "u".into(),
            name: "Burial".into(),
        });
        a.album_id = Some("alb".into());
        let mut batch = [a];
        dedupe_lookups(&mut batch);
        assert!(batch[0].new_artist.is_some());
        assert!(batch[0].new_album.is_some());
        assert_eq!(batch[0].album_id.as_deref(), Some("alb"));
    }

    #[test]
    fn an_undo_note_is_named_per_row_not_per_backup() {
        // One backup covers a whole batch, so notes named after it overwrote
        // each other and only the last row stayed undoable.
        let dir = std::env::temp_dir().join(format!("rr-note-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let backup = dir.join("master.db.20260101T000000000Z.bak");

        let note = |content_id: &str| UndoNote {
            content_id: content_id.into(),
            content_uuid: "u".into(),
            folder_path: "/x.flac".into(),
            artist_id: None,
            created_artist: false,
            inserted_at: "now".into(),
            backup: None,
        };
        let first = note("111").write_beside(&backup).unwrap();
        let second = note("222").write_beside(&backup).unwrap();

        assert_ne!(first, second, "two inserts must not share a note file");
        assert!(first.exists() && second.exists());
        assert!(first.to_string_lossy().contains("111"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_confirmation_shows_every_value_that_will_be_written() {
        let s = render(&content());
        for expected in [
            "227191147",
            "14cc296b",
            "TELL ME",
            "FileType",
            "FLAC",
            "2768718261",
            "ContentLink",
            "2885134",
        ] {
            assert!(s.contains(expected), "{expected:?} missing from:\n{s}");
        }
    }

    #[test]
    fn the_confirmation_explains_the_fields_a_user_cannot_interpret() {
        let s = render(&content());
        assert!(
            s.contains("the transfer sets this"),
            "Analysed needs context"
        );
        assert!(
            s.contains("local file, not cloud-managed"),
            "ServiceID needs context"
        );
        assert!(
            s.contains("rb_local_synced"),
            "the sync flag must be disclosed"
        );
    }

    #[test]
    fn a_new_artist_is_called_out_distinctly_from_an_existing_one() {
        let mut c = content();
        assert!(render(&c).contains("(existing)"));
        c.new_artist = Some(NewLookup {
            table: "djmdArtist",
            id: "666000868".into(),
            uuid: "u".into(),
            name: "OJC".into(),
        });
        let s = render(&c);
        assert!(s.contains("new djmdArtist row"), "got:\n{s}");
        assert!(s.contains("\"OJC\""), "got:\n{s}");
    }

    #[test]
    fn album_and_genre_get_their_own_lookup_rows() {
        // Putting an album name in a text column like Subtitle would show up
        // wrong in rekordbox; it belongs in djmdAlbum behind AlbumID.
        let mut c = content();
        c.new_album = Some(NewLookup {
            table: "djmdAlbum",
            id: "1189904700".into(),
            uuid: "u".into(),
            name: "cursed003".into(),
        });
        let s = render(&c);
        assert!(s.contains("new djmdAlbum row"), "got:\n{s}");
        assert!(s.contains("cursed003"), "got:\n{s}");
        // Whitespace-insensitive so column padding is not part of the contract.
        assert!(
            s.lines()
                .any(|l| l.split_whitespace().eq(["GenreID", "NULL"])),
            "absent genre stays NULL:\n{s}"
        );
    }

    #[test]
    fn tag_derived_fields_are_shown() {
        let s = render(&content());
        assert!(s.contains("ReleaseYear"), "got:\n{s}");
        assert!(s.contains("8 / NULL"), "track/disc:\n{s}");
        assert!(s.contains("prodojc.bandcamp.com"), "comment:\n{s}");
    }

    #[test]
    fn a_missing_artist_renders_as_null_not_as_an_empty_row() {
        let mut c = content();
        c.artist_id = None;
        c.new_artist = None;
        assert!(
            render(&c)
                .lines()
                .any(|l| l.split_whitespace().eq(["ArtistID", "NULL"]))
        );
    }

    #[test]
    fn the_anlz_path_is_derived_from_the_new_uuid() {
        // Matches the real row observed: UUID 14cc296b-0338-... ->
        // /PIONEER/USBANLZ/14c/c296b-0338-.../ANLZ0000.DAT
        let p = anlz_path_for(&content());
        assert!(p.starts_with("/PIONEER/USBANLZ/14c/c296b-0338-"), "got {p}");
        assert!(p.ends_with("ANLZ0000.DAT"), "got {p}");
    }

    #[test]
    fn bit_rate_is_stored_in_kbps() {
        // ffprobe reports bits per second; rekordbox stores kbps.
        let c = content();
        assert_eq!(c.bit_rate, Some(1064));
    }

    #[test]
    fn an_undo_note_round_trips_and_records_whether_it_made_an_artist() {
        let note = UndoNote {
            content_id: "1".into(),
            content_uuid: "u".into(),
            folder_path: "/x.flac".into(),
            artist_id: Some("2".into()),
            created_artist: true,
            inserted_at: "2026-08-23 00:00:00.000 +00:00".into(),
            backup: Some("/b.bak".into()),
        };
        let back: UndoNote = serde_json::from_slice(&serde_json::to_vec(&note).unwrap()).unwrap();
        assert_eq!(back.content_id, "1");
        assert!(
            back.created_artist,
            "undo must not delete a pre-existing artist"
        );
    }

    #[test]
    fn the_undo_note_lands_beside_the_backup() {
        let dir = std::env::temp_dir().join(format!("rr-undo-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let backup = dir.join("master.db.20260823T000000Z.bak");
        std::fs::write(&backup, b"x").unwrap();

        let note = UndoNote {
            content_id: "42".into(),
            content_uuid: "u".into(),
            folder_path: "/x.flac".into(),
            artist_id: None,
            created_artist: false,
            inserted_at: "now".into(),
            backup: Some(backup.to_string_lossy().into_owned()),
        };
        let written = note.write_beside(&backup).unwrap();
        assert!(written.exists());
        assert!(
            written.to_string_lossy().ends_with(".inserted.json"),
            "got {}",
            written.display()
        );
        let back: UndoNote = serde_json::from_slice(&std::fs::read(&written).unwrap()).unwrap();
        assert_eq!(back.content_id, "42");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn probed_info_maps_onto_the_row_fields() {
        let i = info();
        assert_eq!(i.length_secs(), 311);
        assert_eq!(
            i.rekordbox_file_type(Path::new("/x/a.flac")),
            Some(5),
            "flac is FileType 5"
        );
    }
}
