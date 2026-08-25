//! Turning an acquired file into an analysis transfer, gated on a fingerprint.
//!
//! This is where the acquisition side meets the existing
//! `build_plan → render → safety_preflight → apply_plan` pipeline. Those four are
//! not modified: the gate runs *before* `build_plan`, so a pair that is not
//! provably the same recording at the same alignment never becomes a `Plan` at
//! all.

use std::path::Path;

use anyhow::{Result, anyhow};
use owo_colors::OwoColorize;

use crate::analysis::{self, CopyOpts, TrackHeader};
use crate::config::Config;
use crate::db::MasterDb;
use crate::fingerprint::{self as fp, SpeedEvidence, Thresholds, Verdict};
use crate::pending::{Entry, PendingStore, State, find_imported_row};

/// Thresholds from config.
pub fn thresholds(cfg: &Config) -> Thresholds {
    Thresholds {
        score_max: cfg.fingerprint.score_max,
        coverage_min: cfg.fingerprint.coverage_min,
        shift_items_max: cfg.fingerprint.shift_items_max,
        speed_ratio_tol: cfg.fingerprint.speed_ratio_tol,
    }
}

/// Where a source track's audio can be read from.
#[derive(Debug, Clone)]
pub enum AudioSource {
    Local(std::path::PathBuf),
    /// A rekordbox streaming row. `FolderPath` holds a service URI such as
    /// `soundcloud:tracks:123`, and there is no local audio — so the reference
    /// side of the comparison has to be fetched just to be fingerprinted.
    Streaming {
        uri: String,
        url: String,
    },
    Unavailable {
        reason: String,
    },
}

/// Decide how to get at a track's audio.
pub fn resolve_audio_source(track: &TrackHeader) -> AudioSource {
    if let Some(p) = track.local_path() {
        let path = std::path::PathBuf::from(p);
        if path.exists() {
            return AudioSource::Local(path);
        }
        // A path that is not on this machine: another device's row, or a file
        // that moved. Either way there is nothing to fingerprint.
        return AudioSource::Unavailable {
            reason: format!("{p} does not exist on this machine"),
        };
    }
    if let Some(uri) = track.streaming_uri() {
        if let Some(id) = crate::acquire::soundcloud::track_id(uri) {
            return AudioSource::Streaming {
                uri: uri.to_string(),
                url: crate::acquire::soundcloud::api_url(&id),
            };
        }
        // apple-music and the other DRM services cannot be fetched at all.
        return AudioSource::Unavailable {
            reason: format!("{uri} is a streaming source this tool cannot fetch"),
        };
    }
    AudioSource::Unavailable {
        reason: "no audio path on this track".into(),
    }
}

/// The outcome of checking one pairing.
pub struct GateOutcome {
    pub verdict: Verdict,
    /// The precise durations used, when both were available.
    pub durations: Option<(f64, f64)>,
}

/// Fingerprint both sides and judge them.
///
/// Cheap checks first: whole-second `Length`, then normalised title and artist,
/// then precise durations, and only then any decoding. A streaming source is
/// fetched to a scratch file that is removed before this returns.
pub fn gate(
    src: &TrackHeader,
    dst_path: &Path,
    dst_length: Option<i64>,
    dst_bpm: Option<i64>,
    cfg: &Config,
) -> Result<GateOutcome> {
    let t = thresholds(cfg);
    let window = cfg.fingerprint.window_secs;

    // A duration difference beyond tolerance means these are not the same cut,
    // and it costs nothing to notice.
    if let (Some(a), Some(b)) = (src.length, dst_length)
        && (a - b).abs() > cfg.fingerprint.duration_tol_secs
    {
        return Ok(GateOutcome {
            verdict: Verdict::Reject {
                reason: fp::RejectReason::DurationMismatch {
                    a,
                    b,
                    tol: cfg.fingerprint.duration_tol_secs,
                },
                score: f64::NAN,
                coverage: 0.0,
                shift_ms: (a - b) * 1000,
            },
            durations: None,
        });
    }

    let source = resolve_audio_source(src);
    let mut scratch = None;
    let src_path = match &source {
        AudioSource::Local(p) => p.clone(),
        AudioSource::Streaming { url, .. } => {
            if !cfg.fingerprint.stream_fetch {
                return Err(anyhow!(
                    "source {} is a streaming track and stream_fetch is off, \
                     so it cannot be verified",
                    src.id
                ));
            }
            let dir = fp::ScratchDir::new()?;
            let path = fetch_for_fingerprint(url, dir.path(), cfg)?;
            scratch = Some(dir);
            path
        }
        AudioSource::Unavailable { reason } => {
            // Fail closed. An unverifiable source must never be transferred from.
            return Err(anyhow!("cannot verify source {}: {reason}", src.id));
        }
    };

    // Precise durations catch a sub-1% speed change that whole-second Length
    // cannot see. Missing values simply skip that axis.
    let durations = match (
        fp::probe_duration_secs(&src_path),
        fp::probe_duration_secs(dst_path),
    ) {
        (Ok(a), Ok(b)) => Some((a, b)),
        _ => None,
    };

    let bpms = match (src.bpm, dst_bpm) {
        // BPM is stored ×100.
        (Some(a), Some(b)) if a > 0 && b > 0 => Some((a as f64 / 100.0, b as f64 / 100.0)),
        _ => None,
    };

    let fa = fp::fingerprint_file(&src_path, window)?;
    let fb = fp::fingerprint_file(dst_path, window)?;
    let verdict = fp::compare(&fa, &fb, SpeedEvidence { durations, bpms }, &t)?;

    // Drop the scratch rip now that the items are in memory.
    drop(scratch);

    Ok(GateOutcome { verdict, durations })
}

/// Rip a streaming source to `dir`, purely to fingerprint it.
fn fetch_for_fingerprint(url: &str, dir: &Path, cfg: &Config) -> Result<std::path::PathBuf> {
    use std::time::{Duration, Instant};

    let template = dir.join("src.%(ext)s");
    let mut cmd = crate::proc::capture(&cfg.soundcloud.yt_dlp_path);
    cmd.args([
        "--no-playlist",
        "--no-progress",
        "--no-warnings",
        "-f",
        "bestaudio/best",
        "-o",
    ])
    .arg(&template)
    .arg(url);

    let out = crate::proc::run_with_deadline(cmd, Instant::now() + Duration::from_secs(300))?;
    if !out.status.success() {
        let tail = crate::proc::stderr_tail(&out.stderr);
        // Go+ tracks fail here and always will; say so plainly.
        if tail.to_ascii_lowercase().contains("drm") {
            return Err(anyhow!(
                "{url} is DRM protected, so it cannot be fingerprinted: {tail}"
            ));
        }
        return Err(anyhow!("could not fetch {url} for comparison: {tail}"));
    }

    std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| {
            p.extension()
                .and_then(|x| x.to_str())
                .map(|x| !matches!(x, "part" | "ytdl" | "json"))
                .unwrap_or(false)
        })
        .ok_or_else(|| anyhow!("yt-dlp wrote no audio for {url}"))
}

/// Result of processing one pending entry.
pub enum Processed {
    /// Still waiting for the file to appear in rekordbox.
    NotImported,
    /// The gate refused; the reason is recorded and it will not be retried.
    Rejected(String),
    /// A plan is ready. Held rather than applied so the caller can honour
    /// `--apply` and the running-rekordbox refuse.
    ///
    /// The plan is boxed because it is a kilobyte next to the 24 bytes of the
    /// other variants, and every `Processed` — most of them `NotImported` —
    /// would otherwise be sized for it.
    Ready {
        dst_content_id: String,
        plan: Box<analysis::Plan>,
        verdict: Verdict,
    },
}

/// Check one pending entry: has it been imported, and if so does it pass?
pub fn process(
    db: &MasterDb,
    store: &PendingStore,
    entry: &Entry,
    cfg: &Config,
) -> Result<Processed> {
    let Some(dst_id) = find_imported_row(db, &entry.acquired_path)? else {
        return Ok(Processed::NotImported);
    };

    let src = analysis::load_track(db, &entry.src_content_id)?;
    let dst = analysis::load_track(db, &dst_id)?;

    if src.uuid != entry.src_uuid {
        return Ok(Processed::Rejected(format!(
            "source track {} is no longer the track this was queued for",
            entry.src_content_id
        )));
    }

    let outcome = gate(&src, &entry.acquired_path, dst.length, dst.bpm, cfg)?;
    if !outcome.verdict.is_accept() {
        let why = outcome.verdict.summary();
        store.set_rejected(entry.id, &why)?;
        return Ok(Processed::Rejected(why));
    }

    let plan = analysis::build_plan(
        db,
        &entry.src_content_id,
        &dst_id,
        &CopyOpts {
            replace: entry.replace,
            lock: entry.lock,
        },
    )?;
    store.set_matched(entry.id, &dst_id, &outcome.verdict.summary())?;

    Ok(Processed::Ready {
        dst_content_id: dst_id,
        plan: Box::new(plan),
        verdict: outcome.verdict,
    })
}

/// Print a plan together with its gate verdict.
///
/// `build_plan` already warns when `Length` differs by more than a second and
/// then leaves the destination unchanged — the same hazard the shift axis catches
/// properly, but at whole-second resolution and buried inside the render. It is
/// hoisted here so it is actually seen.
pub fn report(plan: &analysis::Plan, verdict: &Verdict) -> String {
    let mut s = String::new();
    s.push_str(&format!("{} {}\n", "fp ok".green(), verdict.summary()));
    s.push_str(&plan.render());
    if !plan.warnings.is_empty() {
        s.push('\n');
        for w in &plan.warnings {
            s.push_str(&format!("{} {w}\n", "warning:".yellow()));
        }
    }
    s
}

/// Mark an entry applied.
pub fn mark_applied(store: &PendingStore, entry: &Entry) -> Result<()> {
    store.set_state(entry.id, State::Applied)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(folder: Option<&str>) -> TrackHeader {
        TrackHeader {
            id: "1".into(),
            uuid: "u1".into(),
            title: Some("T".into()),
            artist: Some("A".into()),
            bpm: Some(12800),
            length: Some(200),
            analysed: Some(105),
            analysis_data_path: None,
            file_type: Some(0),
            cue_count: 1,
            folder_path: folder.map(str::to_string),
            org_folder_path: None,
        }
    }

    #[test]
    fn a_streaming_row_resolves_to_a_fetchable_url() {
        // This is what makes FileType 19 sources verifiable at all: rekordbox
        // stores soundcloud:tracks:<id>, and yt-dlp accepts the api url form.
        let t = header(Some("soundcloud:tracks:1803453465"));
        match resolve_audio_source(&t) {
            AudioSource::Streaming { url, uri } => {
                assert_eq!(uri, "soundcloud:tracks:1803453465");
                assert_eq!(url, "https://api.soundcloud.com/tracks/1803453465");
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn a_drm_streaming_row_is_unavailable_rather_than_attempted() {
        let t = header(Some("apple-music:tracks:12345"));
        match resolve_audio_source(&t) {
            AudioSource::Unavailable { reason } => {
                assert!(reason.contains("cannot fetch"), "got {reason}")
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn a_missing_local_file_is_unavailable_not_silently_skipped() {
        // Another device's row, or a file that moved. Must fail closed.
        let t = header(Some("/definitely/not/here.mp3"));
        match resolve_audio_source(&t) {
            AudioSource::Unavailable { reason } => {
                assert!(reason.contains("does not exist"), "got {reason}")
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn a_windows_path_from_another_device_is_not_mistaken_for_a_stream() {
        let t = header(Some("C:/Users/Someone/Music/x.mp3"));
        assert!(matches!(
            resolve_audio_source(&t),
            AudioSource::Unavailable { .. }
        ));
    }

    #[test]
    fn a_real_local_file_resolves_to_local() {
        let dir = std::env::temp_dir().join(format!("rr-src-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("song.mp3");
        std::fs::write(&f, b"x").unwrap();
        let t = header(Some(f.to_str().unwrap()));
        assert!(matches!(resolve_audio_source(&t), AudioSource::Local(_)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_row_with_no_path_at_all_is_unavailable() {
        assert!(matches!(
            resolve_audio_source(&header(None)),
            AudioSource::Unavailable { .. }
        ));
    }

    #[test]
    fn org_folder_path_is_used_when_folder_path_was_rewritten_by_cloud_sync() {
        let dir = std::env::temp_dir().join(format!("rr-org-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("song.flac");
        std::fs::write(&f, b"x").unwrap();

        let mut t = header(Some("/contents_2768718261/artist/album/song.flac"));
        t.org_folder_path = Some(f.to_str().unwrap().to_string());
        // local_path takes the first path-shaped candidate, so the rewritten
        // FolderPath wins and is then found missing.
        match resolve_audio_source(&t) {
            AudioSource::Unavailable { reason } => {
                assert!(reason.contains("contents_"), "got {reason}")
            }
            other => panic!("got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn thresholds_come_from_config() {
        let mut cfg = Config::default();
        cfg.fingerprint.score_max = 3.5;
        cfg.fingerprint.shift_items_max = 2;
        let t = thresholds(&cfg);
        assert_eq!(t.score_max, 3.5);
        assert_eq!(t.shift_items_max, 2);
    }
}
