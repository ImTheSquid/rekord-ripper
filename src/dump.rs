use std::collections::HashMap;

use anyhow::{Result, anyhow};
use owo_colors::OwoColorize;
use rusqlite::{Row, params};

use crate::db::MasterDb;
use crate::format::{file_type_name, format_bpm, format_length, format_msec, kind_label};
use crate::query::{Fields, Query, text_blob};

struct Track {
    id: String,
    title: Option<String>,
    artist: Option<String>,
    folder_path: Option<String>,
    file_type: Option<i64>,
    bpm: Option<i64>,
    length: Option<i64>,
    analysed: Option<i64>,
    analysis_updated: Option<String>,
    cue_updated: Option<String>,
    analysis_data_path: Option<String>,
    uuid: Option<String>,
    content_link: Option<i64>,
    service_id: Option<i64>,
    cue_count: i64,
}

impl Track {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Track {
            id: row.get("ID")?,
            title: row.get("Title")?,
            artist: row.get("ArtistName")?,
            folder_path: row.get("FolderPath")?,
            file_type: row.get("FileType")?,
            bpm: row.get("BPM")?,
            length: row.get("Length")?,
            analysed: row.get("Analysed")?,
            analysis_updated: row.get("AnalysisUpdated")?,
            cue_updated: row.get("CueUpdated")?,
            analysis_data_path: row.get("AnalysisDataPath")?,
            uuid: row.get("UUID")?,
            content_link: row.get("ContentLink")?,
            service_id: row.get("ServiceID")?,
            cue_count: row.get("cue_count")?,
        })
    }

    fn tags(&self) -> String {
        let path = self.folder_path.as_deref();
        let origin = crate::format::origin(self.file_type, path, self.service_id);
        crate::format::track_tags(crate::format::TrackFacts {
            origin,
            file_type: self.file_type,
            cue_count: self.cue_count,
            locked: self.analysed.unwrap_or(0) & 0x80 != 0,
            present: crate::presence::check(origin, path),
        })
    }
}

struct Cue {
    in_msec: Option<i64>,
    out_msec: Option<i64>,
    kind: Option<i64>,
    color: Option<i64>,
    comment: Option<String>,
    beat_loop_size: Option<i64>,
}

struct MixerParam {
    gain_high: Option<i64>,
    gain_low: Option<i64>,
    peak_high: Option<i64>,
    peak_low: Option<i64>,
}

pub fn run(db: &MasterDb, query: Option<&str>, limit: Option<u32>) -> Result<()> {
    let tracks = find_tracks(db, query, limit)?;
    if tracks.is_empty() {
        match query {
            Some(q) => println!("No tracks matched {q:?}."),
            None => println!("master.db has no tracks."),
        }
        return Ok(());
    }

    for (i, t) in tracks.iter().enumerate() {
        if i > 0 {
            println!();
        }
        print_track(db, t)?;
    }
    Ok(())
}

const SELECT_TRACK: &str = "
    SELECT c.ID, c.Title, c.FolderPath, c.FileType, c.ServiceID, c.BPM, c.Length,
           c.Analysed, c.AnalysisUpdated, c.CueUpdated, c.AnalysisDataPath, c.UUID,
           c.ContentLink, a.Name AS ArtistName,
           (SELECT COUNT(*) FROM djmdCue
             WHERE ContentID = c.ID
               AND (rb_local_deleted = 0 OR rb_local_deleted IS NULL)) AS cue_count
    FROM djmdContent c
    LEFT JOIN djmdArtist a ON a.ID = c.ArtistID
    WHERE (c.rb_local_deleted = 0 OR c.rb_local_deleted IS NULL)";

fn find_tracks(db: &MasterDb, query: Option<&str>, limit: Option<u32>) -> Result<Vec<Track>> {
    match query {
        None => {
            // Default to no limit when listing everything.
            let lim: i64 = limit.map(|l| l as i64).unwrap_or(i64::MAX);
            let sql = format!("{SELECT_TRACK} ORDER BY c.Title LIMIT ?1");
            let mut stmt = db.conn.prepare(&sql)?;
            let rows = stmt.query_map(params![lim], Track::from_row)?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| anyhow!("query failed: {e}"))
        }
        // An all-digit query is an exact ContentID. That one stays exact: it is
        // how the other subcommands hand you a track, and a fuzzy match on it
        // would be a worse answer, not a kinder one.
        Some(q) if !q.is_empty() && q.bytes().all(|b| b.is_ascii_digit()) => {
            let sql = format!("{SELECT_TRACK} AND c.ID = ?1");
            let mut stmt = db.conn.prepare(&sql)?;
            let rows = stmt.query_map(params![q], Track::from_row)?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| anyhow!("query failed: {e}"))
        }
        Some(q) => filter_tracks(db, q, limit.unwrap_or(10) as usize),
    }
}

/// Run the shared filter language over the library.
///
/// Filtering here rather than in SQL is what keeps `dump` and the TUI honestly
/// identical — one parser, one matcher, one answer. The library is a few
/// thousand rows, so the scan costs less than the ANLZ read that follows it.
fn filter_tracks(db: &MasterDb, query: &str, limit: usize) -> Result<Vec<Track>> {
    let parsed = Query::parse(query);
    let playlists = if parsed.touches_playlists() {
        crate::playlists::blobs_by_track(db).unwrap_or_default()
    } else {
        HashMap::new()
    };

    let sql = format!("{SELECT_TRACK} ORDER BY c.Title");
    let mut stmt = db.conn.prepare(&sql)?;
    let rows = stmt.query_map([], Track::from_row)?;

    let mut out = Vec::new();
    for row in rows {
        if out.len() >= limit {
            break;
        }
        let t = row.map_err(|e| anyhow!("query failed: {e}"))?;
        let text = text_blob(
            t.title.as_deref().unwrap_or_default(),
            t.artist.as_deref().unwrap_or_default(),
        );
        let tags = t.tags();
        let fields = Fields {
            text: &text,
            playlists: playlists.get(&t.id).map(String::as_str).unwrap_or_default(),
            tags: &tags,
            bpm: t.bpm,
            length: t.length,
        };
        if parsed.matches(fields) {
            out.push(t);
        }
    }
    Ok(out)
}

fn print_track(db: &MasterDb, t: &Track) -> Result<()> {
    let header = format!("Track {}", t.id);
    println!("{}", header.bold().cyan());
    println!(
        "  {} {}",
        "Title           ".dimmed(),
        t.title.as_deref().unwrap_or("-")
    );
    println!(
        "  {} {}",
        "Artist          ".dimmed(),
        t.artist.as_deref().unwrap_or("-")
    );
    println!(
        "  {} {}",
        "FolderPath      ".dimmed(),
        t.folder_path.as_deref().unwrap_or("-")
    );
    println!(
        "  {} {} ({})",
        "FileType        ".dimmed(),
        t.file_type.map(|v| v.to_string()).unwrap_or("-".into()),
        file_type_name(t.file_type)
    );
    println!("  {} {}", "BPM             ".dimmed(), format_bpm(t.bpm));
    println!(
        "  {} {}",
        "Length          ".dimmed(),
        format_length(t.length)
    );
    println!(
        "  {} {}",
        "Analysed        ".dimmed(),
        t.analysed.map(|v| v.to_string()).unwrap_or("-".into())
    );
    println!(
        "  {} {}",
        "AnalysisUpdated ".dimmed(),
        t.analysis_updated.as_deref().unwrap_or("-")
    );
    println!(
        "  {} {}",
        "CueUpdated      ".dimmed(),
        t.cue_updated.as_deref().unwrap_or("-")
    );
    println!(
        "  {} {}",
        "ContentLink     ".dimmed(),
        t.content_link.map(|v| v.to_string()).unwrap_or("-".into())
    );
    println!(
        "  {} {}",
        "UUID            ".dimmed(),
        t.uuid.as_deref().unwrap_or("-")
    );

    match &t.analysis_data_path {
        Some(rel) => {
            let abs = db.resolve_analysis_path(rel);
            let exists_marker = if abs.exists() {
                let size = std::fs::metadata(&abs).map(|m| m.len()).unwrap_or(0);
                format!("[exists, {size} bytes]").green().to_string()
            } else {
                "[MISSING]".red().to_string()
            };
            println!("  {} {rel}", "AnalysisDataPath".dimmed());
            println!(
                "  {} {} {exists_marker}",
                "  -> resolved   ".dimmed(),
                abs.display()
            );
        }
        None => println!("  {} -", "AnalysisDataPath".dimmed()),
    }

    let cues = fetch_cues(db, &t.id)?;
    println!(
        "  {} {}",
        "Cues            ".dimmed(),
        format!("({})", cues.len()).bold()
    );
    for c in &cues {
        let pos = c.in_msec.map(format_msec).unwrap_or_else(|| "-".into());
        let kind = c.kind.map(|k| k.to_string()).unwrap_or("-".into());
        let kind_label = kind_label(c.kind);
        let mut extras = Vec::new();
        if let Some(o) = c.out_msec
            && o >= 0
        {
            extras.push(format!("out={}", format_msec(o)));
        }
        if let Some(bls) = c.beat_loop_size
            && bls > 0
        {
            extras.push(format!("loop={bls}b"));
        }
        if let Some(color) = c.color
            && color >= 0
        {
            extras.push(format!("color={color}"));
        }
        if let Some(cmt) = c.comment.as_deref().filter(|s| !s.is_empty()) {
            extras.push(format!("\"{cmt}\""));
        }
        let extras_str = if extras.is_empty() {
            String::new()
        } else {
            format!("  {}", extras.join(" "))
        };
        println!("    {pos:>10}  kind={kind} ({kind_label}){extras_str}");
    }

    if let Some(mp) = fetch_mixer_param(db, &t.id)? {
        println!(
            "  {} GainHigh={} GainLow={} PeakHigh={} PeakLow={}",
            "MixerParam      ".dimmed(),
            mp.gain_high.map(|v| v.to_string()).unwrap_or("-".into()),
            mp.gain_low.map(|v| v.to_string()).unwrap_or("-".into()),
            mp.peak_high.map(|v| v.to_string()).unwrap_or("-".into()),
            mp.peak_low.map(|v| v.to_string()).unwrap_or("-".into()),
        );
    } else {
        println!("  {} -", "MixerParam      ".dimmed());
    }

    let censor_count: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM djmdActiveCensor WHERE ContentID = ?1",
        params![t.id],
        |r| r.get(0),
    )?;
    println!("  {} {censor_count}", "ActiveCensors   ".dimmed());

    Ok(())
}

fn fetch_cues(db: &MasterDb, content_id: &str) -> Result<Vec<Cue>> {
    let mut stmt = db.conn.prepare(
        "SELECT InMsec, OutMsec, Kind, Color, Comment, BeatLoopSize
         FROM djmdCue
         WHERE ContentID = ?1
         ORDER BY InMsec",
    )?;
    let rows = stmt.query_map(params![content_id], |row| {
        Ok(Cue {
            in_msec: row.get(0)?,
            out_msec: row.get(1)?,
            kind: row.get(2)?,
            color: row.get(3)?,
            comment: row.get(4)?,
            beat_loop_size: row.get(5)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| anyhow!("cue query failed: {e}"))
}

fn fetch_mixer_param(db: &MasterDb, content_id: &str) -> Result<Option<MixerParam>> {
    let mut stmt = db.conn.prepare(
        "SELECT GainHigh, GainLow, PeakHigh, PeakLow
         FROM djmdMixerParam
         WHERE ContentID = ?1 LIMIT 1",
    )?;
    let mut rows = stmt.query_map(params![content_id], |row| {
        Ok(MixerParam {
            gain_high: row.get(0)?,
            gain_low: row.get(1)?,
            peak_high: row.get(2)?,
            peak_low: row.get(3)?,
        })
    })?;
    Ok(rows.next().transpose()?)
}
