use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};
use owo_colors::OwoColorize;
use rusqlite::{Row, params};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::db::{MasterDb, now_db_string};
use crate::format::{file_type_name, format_bpm, format_length};

const ANLZ_EXTENSIONS: &[&str] = &["DAT", "EXT", "2EX", "3EX"];

#[derive(Clone, Copy, Default)]
pub struct CopyOpts {
    pub replace: bool,
    pub lock: bool,
}

pub struct Plan {
    pub src: TrackHeader,
    pub dst: TrackHeader,
    pub opts: CopyOpts,

    pub file_copies: Vec<FileCopy>,
    pub new_analysis_data_path: String,

    pub delete_existing_cues: bool,
    pub delete_existing_censors: bool,

    new_cues: Vec<NewCue>,
    new_censors: Vec<NewActiveCensor>,
    new_mixer_param: Option<NewMixerParam>,
    new_content_cue: Option<NewContentCue>,
    new_content_active_censor: Option<NewContentActiveCensor>,

    pub set_bpm: Option<i64>,
    pub set_key_id: Option<String>,
    pub set_length: Option<i64>,
    pub set_analysed: i64,

    pub warnings: Vec<String>,
}

pub struct TrackHeader {
    pub id: String,
    pub uuid: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub bpm: Option<i64>,
    pub length: Option<i64>,
    pub analysed: Option<i64>,
    pub analysis_data_path: Option<String>,
    pub file_type: Option<i64>,
    pub cue_count: i64,
    /// The audio file's path. For a cloud-synced row this is rewritten to a
    /// `/contents_<dbid>/…` form, with the original kept in `org_folder_path` —
    /// so both have to be consulted when matching a path to a row.
    pub folder_path: Option<String>,
    pub org_folder_path: Option<String>,
}

impl TrackHeader {
    /// The first path that looks like a real local file.
    ///
    /// A streaming row holds a service URI here instead (`soundcloud:tracks:…`),
    /// which is not a path, so this returns `None` for those.
    pub fn local_path(&self) -> Option<&str> {
        [self.folder_path.as_deref(), self.org_folder_path.as_deref()]
            .into_iter()
            .flatten()
            .find(|p| p.starts_with('/') || p.contains(":\\") || p.contains(":/"))
    }

    /// The service URI of a streaming row, e.g. `soundcloud:tracks:123`.
    pub fn streaming_uri(&self) -> Option<&str> {
        self.folder_path
            .as_deref()
            .filter(|p| !p.starts_with('/') && p.contains(':') && !p.contains('\\'))
    }
}

pub struct FileCopy {
    pub src: PathBuf,
    pub dst: PathBuf,
}

struct TrackSnapshot {
    header: TrackHeader,
    key_id: Option<String>,
    cue_count: i64,
    censor_count: i64,
}

struct NewCue {
    id: String,   // 10-digit uint32 string, fresh
    uuid: String, // v4
    in_msec: Option<i64>,
    in_frame: Option<i64>,
    in_mpeg_frame: Option<i64>,
    in_mpeg_abs: Option<i64>,
    out_msec: Option<i64>,
    out_frame: Option<i64>,
    out_mpeg_frame: Option<i64>,
    out_mpeg_abs: Option<i64>,
    kind: Option<i64>,
    color: Option<i64>,
    color_table_index: Option<i64>,
    active_loop: Option<i64>,
    comment: Option<String>,
    beat_loop_size: Option<i64>,
    cue_microsec: Option<i64>,
    in_point_seek_info: Option<String>,
    out_point_seek_info: Option<String>,
}

struct NewActiveCensor {
    id: String,
    uuid: String,
    in_msec: i64,
    out_msec: i64,
    info: Option<i64>,
    parameter_list: Option<String>,
}

struct NewMixerParam {
    id: String, // UUID
    uuid: String,
    gain_high: Option<i64>,
    gain_low: Option<i64>,
    peak_high: Option<i64>,
    peak_low: Option<i64>,
}

struct NewContentCue {
    id: String,        // = dst.UUID per the schema linkage
    uuid: String,      // table-row UUID
    cues_json: String, // regenerated to reference dst UUIDs
}

struct NewContentActiveCensor {
    id: String,
    uuid: String,
    active_censors_json: String,
}

// ----------------------------------------------------------------------------
// Plan-building
// ----------------------------------------------------------------------------

pub fn build_plan(db: &MasterDb, src_id: &str, dst_id: &str, opts: &CopyOpts) -> Result<Plan> {
    if src_id == dst_id {
        bail!("source and destination are the same track ({src_id})");
    }

    let src = load_snapshot(db, src_id).map_err(|e| anyhow!("source {src_id}: {e}"))?;
    let dst = load_snapshot(db, dst_id).map_err(|e| anyhow!("destination {dst_id}: {e}"))?;

    let src_has_analysis_path = !src
        .header
        .analysis_data_path
        .as_deref()
        .unwrap_or("")
        .is_empty();
    if src.cue_count == 0 && !src_has_analysis_path {
        bail!(
            "source {src_id} ({}) has no analysis to copy",
            src.header.title.as_deref().unwrap_or("?")
        );
    }

    if dst.cue_count > 0 && !opts.replace {
        bail!(
            "destination {dst_id} ({}) has {} existing cues; pass --replace to overwrite",
            dst.header.title.as_deref().unwrap_or("?"),
            dst.cue_count
        );
    }

    let mut warnings = Vec::new();

    // Length: only copy when within 1 integer second.
    let set_length = match (src.header.length, dst.header.length) {
        (Some(sl), Some(dl)) if (sl - dl).abs() <= 1 => Some(sl),
        (Some(sl), Some(dl)) => {
            warnings.push(format!(
                "length differs by {}s (src={sl}s, dst={dl}s) — leaving destination Length unchanged",
                (sl - dl).abs()
            ));
            None
        }
        (Some(sl), None) => Some(sl),
        (None, _) => None,
    };

    // Analysed bits: copy src's analysis bits, but preserve dst's lock bit
    // unless --lock is set (in which case we force it on).
    let src_a = src.header.analysed.unwrap_or(0);
    let dst_a = dst.header.analysed.unwrap_or(0);
    let new_analysed = if opts.lock {
        (src_a & !0x80) | 0x80
    } else {
        (src_a & !0x80) | (dst_a & 0x80)
    };

    // Derive a fresh AnalysisDataPath under dst's UUID.
    let new_path_relative = derive_anlz_path(&dst.header.uuid);

    // Schedule file copies for any ANLZ variant present on disk.
    let mut file_copies = Vec::new();
    if let Some(src_rel) = src.header.analysis_data_path.as_deref() {
        let src_abs = db.resolve_analysis_path(src_rel);
        if let Some(src_dir) = src_abs.parent() {
            let dst_abs = db.resolve_analysis_path(&new_path_relative);
            let dst_dir = dst_abs.parent().unwrap().to_path_buf();
            for ext in ANLZ_EXTENSIONS {
                let candidate = src_dir.join(format!("ANLZ0000.{ext}"));
                if candidate.exists() {
                    file_copies.push(FileCopy {
                        src: candidate,
                        dst: dst_dir.join(format!("ANLZ0000.{ext}")),
                    });
                }
            }
            if file_copies.is_empty() {
                warnings.push(format!(
                    "no ANLZ files found on disk under {} — destination will reference an empty analysis dir",
                    src_dir.display()
                ));
            }
        }
    } else {
        warnings.push(
            "source has no AnalysisDataPath — only DB cues will be copied, no beat grid".into(),
        );
    }

    // Clone src's djmdCue rows with fresh IDs/UUIDs pointed at dst.
    let new_cues = load_new_cues_from_src(db, &src.header.id, &dst.header.uuid)?;
    let new_censors = load_new_censors_from_src(db, &src.header.id, &dst.header.uuid)?;
    let new_mixer_param = load_new_mixer_param_from_src(db, &src.header.id)?;

    // Rebuild the contentCue JSON blob from the freshly-minted cues.
    let new_content_cue = if !new_cues.is_empty() {
        Some(NewContentCue {
            id: dst.header.uuid.clone(),
            uuid: Uuid::new_v4().to_string(),
            cues_json: build_content_cue_json(&new_cues, &dst.header.id, &dst.header.uuid),
        })
    } else {
        None
    };

    let new_content_active_censor = if !new_censors.is_empty() {
        Some(NewContentActiveCensor {
            id: dst.header.uuid.clone(),
            uuid: Uuid::new_v4().to_string(),
            active_censors_json: build_active_censor_json(
                &new_censors,
                &dst.header.id,
                &dst.header.uuid,
            ),
        })
    } else {
        None
    };

    let set_bpm = src.header.bpm;
    Ok(Plan {
        src: src.header,
        dst: dst.header,
        opts: *opts,
        file_copies,
        new_analysis_data_path: new_path_relative,
        delete_existing_cues: dst.cue_count > 0 && opts.replace,
        delete_existing_censors: dst.censor_count > 0 && opts.replace,
        new_cues,
        new_censors,
        new_mixer_param,
        new_content_cue,
        new_content_active_censor,
        set_bpm,
        set_key_id: src.key_id,
        set_length,
        set_analysed: new_analysed,
        warnings,
    })
}

/// Load one track's header by `djmdContent.ID`.
///
/// The public single-track read the acquisition path needs to turn a local track
/// into a search query.
pub fn load_track(db: &MasterDb, id: &str) -> Result<TrackHeader> {
    load_snapshot(db, id)
        .map(|s| s.header)
        .map_err(|e| anyhow!("track {id}: {e}"))
}

fn load_snapshot(db: &MasterDb, id: &str) -> Result<TrackSnapshot> {
    let cue_count: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM djmdCue WHERE ContentID = ?1 AND (rb_local_deleted = 0 OR rb_local_deleted IS NULL)",
        params![id],
        |r| r.get(0),
    )?;

    let header: TrackHeader = db
        .conn
        .query_row(
            "SELECT c.ID, c.UUID, c.Title, c.BPM, c.Length, c.Analysed,
                    c.AnalysisDataPath, c.FileType, c.FolderPath, c.OrgFolderPath,
                    a.Name AS Artist
             FROM djmdContent c
             LEFT JOIN djmdArtist a ON a.ID = c.ArtistID
             WHERE c.ID = ?1",
            params![id],
            |r: &Row<'_>| {
                Ok(TrackHeader {
                    id: r.get("ID")?,
                    uuid: r.get::<_, String>("UUID")?,
                    title: r.get("Title")?,
                    artist: r.get("Artist")?,
                    bpm: r.get("BPM")?,
                    length: r.get("Length")?,
                    analysed: r.get("Analysed")?,
                    analysis_data_path: r.get("AnalysisDataPath")?,
                    file_type: r.get("FileType")?,
                    cue_count,
                    folder_path: r.get("FolderPath")?,
                    org_folder_path: r.get("OrgFolderPath")?,
                })
            },
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => anyhow!("not found"),
            other => anyhow!(other),
        })?;

    let key_id: Option<String> = db.conn.query_row(
        "SELECT KeyID FROM djmdContent WHERE ID = ?1",
        params![id],
        |r| r.get(0),
    )?;

    let censor_count: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM djmdActiveCensor WHERE ContentID = ?1 AND (rb_local_deleted = 0 OR rb_local_deleted IS NULL)",
        params![id],
        |r| r.get(0),
    )?;

    Ok(TrackSnapshot {
        header,
        key_id,
        cue_count,
        censor_count,
    })
}

fn load_new_cues_from_src(db: &MasterDb, src_id: &str, _dst_uuid: &str) -> Result<Vec<NewCue>> {
    let mut stmt = db.conn.prepare(
        "SELECT InMsec, InFrame, InMpegFrame, InMpegAbs,
                OutMsec, OutFrame, OutMpegFrame, OutMpegAbs,
                Kind, Color, ColorTableIndex, ActiveLoop,
                Comment, BeatLoopSize, CueMicrosec,
                InPointSeekInfo, OutPointSeekInfo
         FROM djmdCue
         WHERE ContentID = ?1 AND (rb_local_deleted = 0 OR rb_local_deleted IS NULL)
         ORDER BY InMsec",
    )?;
    let rows = stmt.query_map(params![src_id], |r| {
        Ok(NewCue {
            id: random_numeric_id(),
            uuid: Uuid::new_v4().to_string(),
            in_msec: r.get(0)?,
            in_frame: r.get(1)?,
            in_mpeg_frame: r.get(2)?,
            in_mpeg_abs: r.get(3)?,
            out_msec: r.get(4)?,
            out_frame: r.get(5)?,
            out_mpeg_frame: r.get(6)?,
            out_mpeg_abs: r.get(7)?,
            kind: r.get(8)?,
            color: r.get(9)?,
            color_table_index: r.get(10)?,
            active_loop: r.get(11)?,
            comment: r.get(12)?,
            beat_loop_size: r.get(13)?,
            cue_microsec: r.get(14)?,
            in_point_seek_info: r.get(15)?,
            out_point_seek_info: r.get(16)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn load_new_censors_from_src(
    db: &MasterDb,
    src_id: &str,
    _dst_uuid: &str,
) -> Result<Vec<NewActiveCensor>> {
    let mut stmt = db.conn.prepare(
        "SELECT InMsec, OutMsec, Info, ParameterList
         FROM djmdActiveCensor
         WHERE ContentID = ?1 AND (rb_local_deleted = 0 OR rb_local_deleted IS NULL)
         ORDER BY InMsec",
    )?;
    let rows = stmt.query_map(params![src_id], |r| {
        Ok(NewActiveCensor {
            id: Uuid::new_v4().to_string(),
            uuid: Uuid::new_v4().to_string(),
            in_msec: r.get(0)?,
            out_msec: r.get(1)?,
            info: r.get(2)?,
            parameter_list: r.get(3)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn load_new_mixer_param_from_src(db: &MasterDb, src_id: &str) -> Result<Option<NewMixerParam>> {
    let mut stmt = db.conn.prepare(
        "SELECT GainHigh, GainLow, PeakHigh, PeakLow
         FROM djmdMixerParam
         WHERE ContentID = ?1 AND (rb_local_deleted = 0 OR rb_local_deleted IS NULL)
         LIMIT 1",
    )?;
    let row = stmt
        .query_row(params![src_id], |r| {
            Ok(NewMixerParam {
                id: Uuid::new_v4().to_string(),
                uuid: Uuid::new_v4().to_string(),
                gain_high: r.get(0)?,
                gain_low: r.get(1)?,
                peak_high: r.get(2)?,
                peak_low: r.get(3)?,
            })
        })
        .ok();
    Ok(row)
}

fn build_content_cue_json(cues: &[NewCue], dst_content_id: &str, dst_uuid: &str) -> String {
    let now_iso = now_iso_string();
    let arr: Vec<Value> = cues
        .iter()
        .map(|c| {
            json!({
                "ID": c.id,
                "UUID": c.uuid,
                "ContentID": dst_content_id,
                "ContentUUID": dst_uuid,
                "InMsec": c.in_msec,
                "InFrame": c.in_frame,
                "InMpegFrame": c.in_mpeg_frame,
                "InMpegAbs": c.in_mpeg_abs,
                "OutMsec": c.out_msec,
                "OutFrame": c.out_frame,
                "OutMpegFrame": c.out_mpeg_frame,
                "OutMpegAbs": c.out_mpeg_abs,
                "Kind": c.kind,
                "Color": c.color,
                "created_at": now_iso,
                "updated_at": now_iso,
            })
        })
        .collect();
    Value::Array(arr).to_string()
}

fn build_active_censor_json(
    censors: &[NewActiveCensor],
    dst_content_id: &str,
    dst_uuid: &str,
) -> String {
    let now_iso = now_iso_string();
    let arr: Vec<Value> = censors
        .iter()
        .map(|c| {
            json!({
                "ID": c.id,
                "UUID": c.uuid,
                "ContentID": dst_content_id,
                "ContentUUID": dst_uuid,
                "InMsec": c.in_msec,
                "OutMsec": c.out_msec,
                "Info": c.info,
                "ParameterList": c.parameter_list,
                "created_at": now_iso,
                "updated_at": now_iso,
            })
        })
        .collect();
    Value::Array(arr).to_string()
}

// ----------------------------------------------------------------------------
// Plan application
// ----------------------------------------------------------------------------

pub fn apply_plan(db: &mut MasterDb, plan: &Plan) -> Result<PathBuf> {
    // Count total USN-allocated actions for the global counter bump.
    let action_count = plan_action_count(plan) as i64;

    // Take a backup of master.db before any mutation.
    let backup_path = db.backup()?;

    let tx = db.conn.transaction()?;

    let start_usn: i64 = tx.query_row(
        "SELECT int_1 FROM agentRegistry WHERE registry_id = 'localUpdateCount'",
        [],
        |r| r.get(0),
    )?;
    let mut allocator = UsnAllocator::new(start_usn);

    let now = now_db_string();

    // 1. Delete pre-existing rows.
    //
    // djmdCue / djmdActiveCensor are random-keyed multi-row tables; only
    // touch them under --replace when the dst actually has live rows.
    if plan.delete_existing_cues {
        tx.execute(
            "DELETE FROM djmdCue WHERE ContentID = ?1",
            params![plan.dst.id],
        )?;
    }
    if plan.delete_existing_censors {
        tx.execute(
            "DELETE FROM djmdActiveCensor WHERE ContentID = ?1",
            params![plan.dst.id],
        )?;
    }
    // contentCue, contentActiveCensor, and djmdMixerParam are single-row-per-dst
    // tables keyed by dst.UUID (and/or ContentID). Always clear any existing row
    // — including soft-deleted or empty-blob orphans — before INSERT so a
    // re-apply doesn't hit a UNIQUE constraint on the row's ID column.
    if plan.new_content_cue.is_some() {
        tx.execute(
            "DELETE FROM contentCue WHERE ContentID = ?1 OR ID = ?2",
            params![plan.dst.id, plan.dst.uuid],
        )?;
    }
    if plan.new_content_active_censor.is_some() {
        tx.execute(
            "DELETE FROM contentActiveCensor WHERE ContentID = ?1 OR ID = ?2",
            params![plan.dst.id, plan.dst.uuid],
        )?;
    }
    if plan.new_mixer_param.is_some() {
        tx.execute(
            "DELETE FROM djmdMixerParam WHERE ContentID = ?1",
            params![plan.dst.id],
        )?;
    }

    // 2. Inserts. Deterministic order matches the USN allocator's order so a
    //    later read of `rb_local_usn` reflects the actions sequence.
    for cue in &plan.new_cues {
        let usn = allocator.next();
        tx.execute(
            "INSERT INTO djmdCue
                (ID, ContentID, InMsec, InFrame, InMpegFrame, InMpegAbs,
                 OutMsec, OutFrame, OutMpegFrame, OutMpegAbs,
                 Kind, Color, ColorTableIndex, ActiveLoop,
                 Comment, BeatLoopSize, CueMicrosec,
                 InPointSeekInfo, OutPointSeekInfo,
                 ContentUUID, UUID,
                 rb_data_status, rb_local_data_status, rb_local_deleted, rb_local_synced,
                 rb_local_usn, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                     ?15, ?16, ?17, ?18, ?19, ?20, ?21, 256, 0, 0, 0, ?22, ?23, ?23)",
            params![
                cue.id,
                plan.dst.id,
                cue.in_msec,
                cue.in_frame,
                cue.in_mpeg_frame,
                cue.in_mpeg_abs,
                cue.out_msec,
                cue.out_frame,
                cue.out_mpeg_frame,
                cue.out_mpeg_abs,
                cue.kind,
                cue.color,
                cue.color_table_index,
                cue.active_loop,
                cue.comment,
                cue.beat_loop_size,
                cue.cue_microsec,
                cue.in_point_seek_info,
                cue.out_point_seek_info,
                plan.dst.uuid,
                cue.uuid,
                usn,
                now,
            ],
        )?;
    }

    for censor in &plan.new_censors {
        let usn = allocator.next();
        tx.execute(
            "INSERT INTO djmdActiveCensor
                (ID, ContentID, InMsec, OutMsec, Info, ParameterList,
                 ContentUUID, UUID,
                 rb_data_status, rb_local_data_status, rb_local_deleted, rb_local_synced,
                 rb_local_usn, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 256, 0, 0, 0, ?9, ?10, ?10)",
            params![
                censor.id,
                plan.dst.id,
                censor.in_msec,
                censor.out_msec,
                censor.info,
                censor.parameter_list,
                plan.dst.uuid,
                censor.uuid,
                usn,
                now,
            ],
        )?;
    }

    if let Some(blob) = &plan.new_content_cue {
        let usn = allocator.next();
        tx.execute(
            "INSERT INTO contentCue
                (ID, ContentID, Cues, rb_cue_count, UUID,
                 rb_data_status, rb_local_data_status, rb_local_deleted, rb_local_synced,
                 rb_local_usn, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 256, 0, 0, 0, ?6, ?7, ?7)",
            params![
                blob.id,
                plan.dst.id,
                blob.cues_json,
                plan.new_cues.len() as i64,
                blob.uuid,
                usn,
                now,
            ],
        )?;
    }

    if let Some(blob) = &plan.new_content_active_censor {
        let usn = allocator.next();
        tx.execute(
            "INSERT INTO contentActiveCensor
                (ID, ContentID, ActiveCensors, rb_activecensor_count, UUID,
                 rb_data_status, rb_local_data_status, rb_local_deleted, rb_local_synced,
                 rb_local_usn, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 256, 0, 0, 0, ?6, ?7, ?7)",
            params![
                blob.id,
                plan.dst.id,
                blob.active_censors_json,
                plan.new_censors.len() as i64,
                blob.uuid,
                usn,
                now,
            ],
        )?;
    }

    if let Some(mp) = &plan.new_mixer_param {
        let usn = allocator.next();
        tx.execute(
            "INSERT INTO djmdMixerParam
                (ID, ContentID, GainHigh, GainLow, PeakHigh, PeakLow, UUID,
                 rb_data_status, rb_local_data_status, rb_local_deleted, rb_local_synced,
                 rb_local_usn, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 256, 0, 0, 0, ?8, ?9, ?9)",
            params![
                mp.id,
                plan.dst.id,
                mp.gain_high,
                mp.gain_low,
                mp.peak_high,
                mp.peak_low,
                mp.uuid,
                usn,
                now,
            ],
        )?;
    }

    // 3. djmdContent update — last so it carries the max USN.
    let content_usn = allocator.next();
    tx.execute(
        "UPDATE djmdContent SET
            BPM = COALESCE(?1, BPM),
            KeyID = COALESCE(?2, KeyID),
            Length = COALESCE(?3, Length),
            Analysed = ?4,
            AnalysisDataPath = ?5,
            AnalysisUpdated = COALESCE(CAST(CAST(AnalysisUpdated AS INTEGER) + 1 AS TEXT), '1'),
            CueUpdated = COALESCE(CAST(CAST(CueUpdated AS INTEGER) + 1 AS TEXT), '1'),
            rb_local_usn = ?6,
            rb_local_synced = 0,
            updated_at = ?7
         WHERE ID = ?8",
        params![
            plan.set_bpm,
            plan.set_key_id,
            plan.set_length,
            plan.set_analysed,
            plan.new_analysis_data_path,
            content_usn,
            now,
            plan.dst.id,
        ],
    )?;

    // 4. Global USN counter.
    let final_usn = allocator.current();
    debug_assert_eq!(final_usn - start_usn, action_count);
    let n = tx.execute(
        "UPDATE agentRegistry SET int_1 = ?1, updated_at = ?2
         WHERE registry_id = 'localUpdateCount'",
        params![final_usn, now],
    )?;
    if n != 1 {
        bail!("agentRegistry localUpdateCount missing or updated {n} rows");
    }

    tx.commit()?;

    // 5. File copies AFTER the DB commit, so a rollback can't leave us with
    //    overwritten analysis files referenced by stale rows.
    for fc in &plan.file_copies {
        if let Some(parent) = fc.dst.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&fc.src, &fc.dst)
            .map_err(|e| anyhow!("copy {} -> {}: {e}", fc.src.display(), fc.dst.display()))?;
    }

    Ok(backup_path)
}

fn plan_action_count(plan: &Plan) -> u32 {
    let mut n = 0u32;
    n += plan.new_cues.len() as u32;
    n += plan.new_censors.len() as u32;
    if plan.new_content_cue.is_some() {
        n += 1;
    }
    if plan.new_content_active_censor.is_some() {
        n += 1;
    }
    if plan.new_mixer_param.is_some() {
        n += 1;
    }
    n += 1; // djmdContent update
    n
}

struct UsnAllocator {
    cur: i64,
}

impl UsnAllocator {
    fn new(start: i64) -> Self {
        Self { cur: start }
    }
    fn next(&mut self) -> i64 {
        self.cur += 1;
        self.cur
    }
    fn current(&self) -> i64 {
        self.cur
    }
}

// ----------------------------------------------------------------------------
// Display
// ----------------------------------------------------------------------------

fn format_header_line(h: &TrackHeader) -> String {
    let title = h.title.as_deref().unwrap_or("?");
    let artist = h.artist.as_deref().unwrap_or("?");
    let lock = match h.analysed {
        Some(a) if a & 0x80 != 0 => "🔒",
        _ => "  ",
    };
    format!(
        "\"{title}\" — {artist}   {ft}   {len}   {bpm} BPM   {cues} cues   {lock}",
        ft = file_type_name(h.file_type),
        len = format_length(h.length),
        bpm = format_bpm(h.bpm),
        cues = h.cue_count,
    )
}

impl Plan {
    pub fn render(&self) -> String {
        use std::fmt::Write;
        let mut s = String::new();
        let _ = writeln!(
            s,
            "{} {} → {}",
            "copy".cyan().bold(),
            self.src.id.bold(),
            self.dst.id.bold(),
        );
        let _ = writeln!(s, "  {} {}", "src".dimmed(), format_header_line(&self.src));
        let _ = writeln!(s, "  {} {}", "dst".dimmed(), format_header_line(&self.dst));
        let _ = writeln!(
            s,
            "  {}",
            format!(
                "opts: replace={} lock={}",
                self.opts.replace, self.opts.lock
            )
            .dimmed()
        );

        if !self.warnings.is_empty() {
            for w in &self.warnings {
                let _ = writeln!(s, "  {} {w}", "warn:".yellow());
            }
        }

        let _ = writeln!(s, "  {}", "files:".dimmed());
        for fc in &self.file_copies {
            let _ = writeln!(s, "    {} → {}", fc.src.display(), fc.dst.display());
        }
        if self.file_copies.is_empty() {
            let _ = writeln!(s, "    (none)");
        }

        let _ = writeln!(s, "  {}", "djmdContent updates:".dimmed());
        if let Some(v) = self.set_bpm {
            let _ = writeln!(s, "    BPM              = {:.2}", v as f64 / 100.0);
        }
        if let Some(v) = &self.set_key_id {
            let _ = writeln!(s, "    KeyID            = {v}");
        }
        if let Some(v) = self.set_length {
            let _ = writeln!(s, "    Length           = {v}");
        }
        let _ = writeln!(
            s,
            "    Analysed         = {} (0x{:02x})",
            self.set_analysed, self.set_analysed
        );
        let _ = writeln!(s, "    AnalysisDataPath = {}", self.new_analysis_data_path);

        let _ = writeln!(s, "  {}", "row writes:".dimmed());
        if self.delete_existing_cues {
            let _ = writeln!(s, "    DELETE djmdCue / contentCue for dst");
        }
        if self.delete_existing_censors {
            let _ = writeln!(
                s,
                "    DELETE djmdActiveCensor / contentActiveCensor for dst"
            );
        }
        let _ = writeln!(
            s,
            "    INSERT djmdCue              x{}",
            self.new_cues.len()
        );
        let _ = writeln!(
            s,
            "    INSERT djmdActiveCensor     x{}",
            self.new_censors.len()
        );
        let _ = writeln!(
            s,
            "    INSERT contentCue           x{}",
            if self.new_content_cue.is_some() { 1 } else { 0 }
        );
        let _ = writeln!(
            s,
            "    INSERT contentActiveCensor  x{}",
            if self.new_content_active_censor.is_some() {
                1
            } else {
                0
            }
        );
        let _ = writeln!(
            s,
            "    INSERT djmdMixerParam       x{}",
            if self.new_mixer_param.is_some() { 1 } else { 0 }
        );

        let _ = writeln!(s, "  {} {}", "usn delta:".dimmed(), plan_action_count(self));
        s
    }
}

// ----------------------------------------------------------------------------
// Auto-mode matching
// ----------------------------------------------------------------------------

pub struct AutoFilter {
    pub duration_tol_secs: i64,
    pub include_cued: bool,
    pub limit: Option<u32>,
}

#[derive(Clone, Debug)]
pub struct AutoMatch {
    pub src_id: String,
    pub src_title: Option<String>,
    pub src_artist: Option<String>,
    pub dst_id: String,
    pub dst_title: Option<String>,
    pub dst_artist: Option<String>,
}

struct Candidate {
    id: String,
    title: Option<String>,
    artist: Option<String>,
    length: Option<i64>,
    norm_title: String,
}

pub fn find_auto_matches(db: &MasterDb, filter: AutoFilter) -> Result<Vec<AutoMatch>> {
    let destinations = load_destinations(db, filter.include_cued)?;
    let sources = load_sources(db)?;

    // Group sources by normalized title for an O(D) scan; we still do the
    // duration + artist check linearly within each bucket.
    use std::collections::HashMap;
    let mut by_title: HashMap<&str, Vec<&Candidate>> = HashMap::new();
    for s in &sources {
        by_title.entry(&s.norm_title).or_default().push(s);
    }

    let mut matches = Vec::new();
    for dst in &destinations {
        let bucket = match by_title.get(dst.norm_title.as_str()) {
            Some(b) => b,
            None => continue,
        };
        let viable: Vec<&&Candidate> = bucket
            .iter()
            .filter(|s| s.id != dst.id)
            .filter(|s| match (s.length, dst.length) {
                (Some(sl), Some(dl)) => (sl - dl).abs() <= filter.duration_tol_secs,
                _ => false,
            })
            .filter(|s| artist_matches(s.artist.as_deref(), dst.artist.as_deref()))
            .collect();

        // Multiple matches → skip (never pick one automatically).
        if viable.len() != 1 {
            continue;
        }
        let src = viable[0];
        matches.push(AutoMatch {
            src_id: src.id.clone(),
            src_title: src.title.clone(),
            src_artist: src.artist.clone(),
            dst_id: dst.id.clone(),
            dst_title: dst.title.clone(),
            dst_artist: dst.artist.clone(),
        });

        if let Some(lim) = filter.limit
            && matches.len() >= lim as usize
        {
            break;
        }
    }
    Ok(matches)
}

fn load_destinations(db: &MasterDb, include_cued: bool) -> Result<Vec<Candidate>> {
    let sql = "
        SELECT c.ID, c.Title, a.Name AS Artist, c.Length
        FROM djmdContent c
        LEFT JOIN djmdArtist a ON a.ID = c.ArtistID
        WHERE (c.Analysed & 128) = 0
          AND c.FileType IN (0, 1, 4, 5, 11)
          AND (
            ?1 = 1
            OR (SELECT COUNT(*) FROM djmdCue
                WHERE ContentID = c.ID
                  AND (rb_local_deleted = 0 OR rb_local_deleted IS NULL)) = 0
          )";
    load_candidates(db, sql, &[&(include_cued as i64) as &dyn rusqlite::ToSql])
}

fn load_sources(db: &MasterDb) -> Result<Vec<Candidate>> {
    let sql = "
        SELECT c.ID, c.Title, a.Name AS Artist, c.Length
        FROM djmdContent c
        LEFT JOIN djmdArtist a ON a.ID = c.ArtistID
        WHERE (c.Analysed & 128) != 0
           OR EXISTS (
                SELECT 1 FROM djmdCue
                WHERE ContentID = c.ID
                  AND (rb_local_deleted = 0 OR rb_local_deleted IS NULL)
              )";
    load_candidates(db, sql, &[])
}

fn load_candidates(
    db: &MasterDb,
    sql: &str,
    params: &[&dyn rusqlite::ToSql],
) -> Result<Vec<Candidate>> {
    let mut stmt = db.conn.prepare(sql)?;
    let rows = stmt.query_map(params, |r| {
        let title: Option<String> = r.get("Title")?;
        let norm = title.as_deref().map(normalize_title).unwrap_or_default();
        Ok(Candidate {
            id: r.get("ID")?,
            title,
            artist: r.get("Artist")?,
            length: r.get("Length")?,
            norm_title: norm,
        })
    })?;
    Ok(rows
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .filter(|c| !c.norm_title.is_empty())
        .collect())
}

pub(crate) fn artist_matches(a: Option<&str>, b: Option<&str>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => {
            let a = a.to_lowercase();
            let b = b.to_lowercase();
            a.contains(&b) || b.contains(&a)
        }
        _ => false,
    }
}

/// Lowercase, strip parenthesised text (including the parens), collapse runs
/// of whitespace to a single space, trim.
pub fn normalize_title(s: &str) -> String {
    let lower = s.to_lowercase();
    let mut out = String::with_capacity(lower.len());
    let mut depth = 0i32;
    for ch in lower.chars() {
        match ch {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' if depth > 0 => {
                depth -= 1;
                out.push(' ');
            }
            _ if depth == 0 => out.push(ch),
            _ => {}
        }
    }
    // Collapse whitespace.
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ----------------------------------------------------------------------------
// Pure helpers
// ----------------------------------------------------------------------------

/// `b1fed0f0-df56-...` → `/PIONEER/USBANLZ/b1f/ed0f0-df56-.../ANLZ0000.DAT`
pub fn derive_anlz_path(uuid: &str) -> String {
    let bare = uuid.replace('-', "");
    let prefix = &uuid[..3];
    let rest = &uuid[3..];
    debug_assert_eq!(bare.len(), 32, "uuid not 32 hex chars: {uuid:?}");
    format!("/PIONEER/USBANLZ/{prefix}/{rest}/ANLZ0000.DAT")
}

pub(crate) fn random_numeric_id() -> String {
    // djmdCue.ID is a uint32 stored as a decimal string. Use a v4 UUID's first
    // 4 bytes for randomness — adequate; collision rate vs the existing 1816
    // cue IDs is on the order of 10^-6 per insertion.
    let bytes = Uuid::new_v4().as_bytes()[..4].try_into().unwrap();
    let n = u32::from_be_bytes(bytes);
    n.to_string()
}

fn now_iso_string() -> String {
    chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.3f+00:00")
        .to_string()
}

// Suppress unused import warning when not building the bin
#[allow(dead_code)]
fn _path_marker(_: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usn_allocator_assigns_sequential_values_and_returns_max() {
        let mut a = UsnAllocator::new(100);
        assert_eq!(a.next(), 101);
        assert_eq!(a.next(), 102);
        assert_eq!(a.next(), 103);
        assert_eq!(a.current(), 103);
    }

    #[test]
    fn derive_anlz_path_handles_known_uuid() {
        let p = derive_anlz_path("b1fed0f0-df56-497f-b631-8125a26f069d");
        assert_eq!(
            p,
            "/PIONEER/USBANLZ/b1f/ed0f0-df56-497f-b631-8125a26f069d/ANLZ0000.DAT"
        );
    }

    #[test]
    fn random_numeric_id_is_decimal_in_uint32_range() {
        for _ in 0..16 {
            let id = random_numeric_id();
            assert!(id.bytes().all(|b| b.is_ascii_digit()), "{id}");
            let n: u64 = id.parse().unwrap();
            assert!(n <= u32::MAX as u64);
        }
    }

    #[test]
    fn now_iso_string_format() {
        let s = now_iso_string();
        // 2026-06-04T18:23:45.123+00:00 — 29 chars
        assert_eq!(s.len(), 29, "{s}");
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[10..11], "T");
        assert!(s.ends_with("+00:00"));
    }

    #[test]
    fn normalize_title_strips_parens_and_lowercases() {
        assert_eq!(normalize_title("Hello World"), "hello world");
        assert_eq!(normalize_title("Track (Extended Mix)"), "track");
        assert_eq!(normalize_title("  Mix  (feat. X)  [Remix]  "), "mix");
        assert_eq!(normalize_title("UPPER CASE"), "upper case");
        assert_eq!(normalize_title(""), "");
        assert_eq!(normalize_title("Track (Part 1) (Part 2)"), "track");
        // Unicode case-folding.
        assert_eq!(normalize_title("СТРАНА"), "страна");
    }

    #[test]
    fn artist_matches_substring_either_way() {
        assert!(artist_matches(Some("GRRL"), Some("GRRL")));
        assert!(artist_matches(Some("GRRL"), Some("grrl"))); // case-insens.
        assert!(artist_matches(Some("GRRL & Friend"), Some("GRRL")));
        assert!(artist_matches(Some("GRRL"), Some("GRRL & Friend")));
        assert!(!artist_matches(Some("GRRL"), Some("Other")));
        assert!(!artist_matches(None, Some("GRRL")));
        assert!(!artist_matches(Some("GRRL"), None));
    }
}
