//! What will go wrong on a player: damaged audio, formats a level cannot play,
//! and FLACs a player can only seek in by scanning.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::compat::{self, Decision, Level, SourceFormat};
use crate::flac::{self, Frames, SeekPlan};
use crate::{audio, proc};

/// A full decode; an hour-long WAV over USB is the slow case.
const DECODE_TIMEOUT: Duration = Duration::from_secs(600);

const AUDIO_EXTENSIONS: [&str; 8] = ["flac", "mp3", "m4a", "aac", "mp4", "wav", "aif", "aiff"];

pub struct Target {
    pub label: String,
    pub path: PathBuf,
    /// The rekordbox row, when the file came from the library.
    pub track_id: Option<String>,
}

#[derive(Default)]
pub struct Finding {
    /// Where the audio is damaged, in words.
    pub damage: Option<String>,
    /// What the file is, when the level cannot play it.
    pub unplayable: Option<SourceFormat>,
    /// FLAC only.
    pub seek: Option<SeekPlan>,
    /// Why part of the check could not run.
    pub error: Option<String>,
}

/// Every audio file under `roots`, skipping dotfiles such as macOS's `._`
/// resource forks.
pub fn files_under(roots: &[PathBuf]) -> Result<Vec<Target>> {
    let mut out = Vec::new();
    for root in roots {
        let mut stack = vec![root.clone()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir)? {
                let entry = entry?;
                let path = entry.path();
                if entry.file_name().to_string_lossy().starts_with('.') {
                    continue;
                }
                if entry.file_type()?.is_dir() {
                    stack.push(path);
                } else if is_audio(&path) {
                    let label = path
                        .strip_prefix(root)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .into_owned();
                    out.push(Target {
                        label,
                        path,
                        track_id: None,
                    });
                }
            }
        }
    }
    out.sort_by(|a, b| a.label.cmp(&b.label));
    Ok(out)
}

fn is_audio(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| AUDIO_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

fn is_flac(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("flac"))
}

/// Check one file. FLAC frames are walked and their CRCs checked, which also
/// yields its seek points; anything else is decoded in full by ffmpeg.
pub fn inspect(path: &Path, level: Option<&Level>) -> Finding {
    let mut f = Finding::default();
    if let Some(level) = level {
        match audio::probe(path).and_then(|i| compat::classify(&i, path)) {
            Ok(src) if compat::plan_track(&src, level, false) != Decision::Keep => {
                f.unplayable = Some(src);
            }
            Ok(_) => {}
            Err(e) => f.error = Some(format!("{e:#}")),
        }
    }
    let walked = is_flac(path).then(|| flac::inspect(path));
    match walked {
        Some(Ok(ins)) => {
            if let Frames::Damaged { at_sample } = ins.frames {
                f.damage = Some(format!(
                    "damaged at {}",
                    clock(at_sample / u64::from(ins.info.sample_rate.max(1)))
                ));
            }
            f.seek = Some(ins.seek);
        }
        // Not a bare FLAC stream (an ID3 tag in front, say), so ffmpeg decides.
        Some(Err(e)) => {
            f.error.get_or_insert(format!("{e:#}"));
            f.damage = decode_errors(path).unwrap_or_else(|e| Some(format!("{e:#}")));
        }
        None => f.damage = decode_errors(path).unwrap_or_else(|e| Some(format!("{e:#}"))),
    }
    f
}

/// [`inspect`] every target on `jobs` threads, in order.
pub fn inspect_all(
    targets: &[Target],
    level: Option<&Level>,
    jobs: usize,
    mut progress: impl FnMut(usize, usize),
) -> Vec<Finding> {
    compat::par_map(targets, jobs, |t| inspect(&t.path, level), &mut progress)
}

fn clock(secs: u64) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// ffmpeg's complaints decoding the whole stream, summarised; None when clean.
fn decode_errors(path: &Path) -> Result<Option<String>> {
    let mut cmd = proc::capture("ffmpeg");
    cmd.args(["-v", "error", "-nostdin", "-i"])
        .arg(path)
        .args(["-map", "0:a:0", "-f", "null", "-"]);
    let out = proc::run_with_deadline(cmd, Instant::now() + DECODE_TIMEOUT)?;
    let text = String::from_utf8_lossy(&out.stderr);
    let lines: Vec<&str> = text
        .lines()
        .map(strip_context)
        .filter(|l| !l.is_empty())
        .collect();
    // ffmpeg follows each decoder message with one line naming the packet.
    let is_summary =
        |l: &str| l.contains("Error submitting packet") || l.contains("Decoding error");
    let failures = lines.iter().filter(|l| is_summary(l)).count();
    let first = lines.iter().find(|l| !is_summary(l)).or(lines.first());
    Ok(match (first, out.status.success()) {
        (None, true) => None,
        (None, false) => Some(format!("ffmpeg could not decode it ({})", out.status)),
        (Some(msg), _) => Some(format!(
            "{} decode error{}: {msg}",
            failures.max(1),
            if failures > 1 { "s" } else { "" }
        )),
    })
}

/// A log line without its `[component @ 0x…]` prefixes.
fn strip_context(mut l: &str) -> &str {
    while l.starts_with('[') {
        match l.find("] ") {
            Some(i) => l = &l[i + 2..],
            None => break,
        }
    }
    l.trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rr-check-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn make(dir: &Path, name: &str, args: &[&str]) -> Option<PathBuf> {
        let out = dir.join(name);
        let mut cmd = proc::capture("ffmpeg");
        cmd.args([
            "-v",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "sine=duration=12",
        ])
        .args(args)
        .arg(&out);
        cmd.output().ok()?.status.success().then_some(out)
    }

    #[test]
    fn log_prefixes_are_stripped_however_deeply_nested() {
        assert_eq!(
            strip_context("[mp3float @ 0x1] Header missing"),
            "Header missing"
        );
        assert_eq!(
            strip_context("[aist#0:0/mp3 @ 0x2] [dec:mp3float @ 0x3] Error submitting packet"),
            "Error submitting packet"
        );
        assert_eq!(strip_context("plain"), "plain");
    }

    #[test]
    fn dotfiles_are_skipped_and_audio_is_found_in_subfolders() {
        let dir = tmp("walk");
        std::fs::create_dir_all(dir.join("a/b")).unwrap();
        for f in ["a/b/one.flac", "a/._one.flac", "a/two.MP3", "a/cover.jpg"] {
            std::fs::write(dir.join(f), b"").unwrap();
        }
        let found: Vec<String> = files_under(std::slice::from_ref(&dir))
            .unwrap()
            .into_iter()
            .map(|t| t.label)
            .collect();
        assert_eq!(found, ["a/b/one.flac", "a/two.MP3"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn damage_is_reported_for_flac_and_for_ffmpeg_decoded_formats() {
        let dir = tmp("damage");
        let Some(flac_path) = make(&dir, "t.flac", &[]) else {
            return; // no ffmpeg here
        };
        let Some(mp3) = make(&dir, "t.mp3", &["-b:a", "128k"]) else {
            return;
        };
        let clean = inspect(&flac_path, None);
        assert!(clean.damage.is_none(), "{:?}", clean.damage);
        assert!(matches!(clean.seek, Some(SeekPlan::Add(..))));
        assert!(inspect(&mp3, None).damage.is_none());

        for p in [&flac_path, &mp3] {
            let mut bytes = std::fs::read(p).unwrap();
            let mid = bytes.len() / 2;
            bytes[mid..mid + 64].fill(0x5A);
            std::fs::write(p, bytes).unwrap();
        }
        let f = inspect(&flac_path, None);
        assert!(
            f.damage
                .as_deref()
                .is_some_and(|d| d.starts_with("damaged at 0:0")),
            "{:?}",
            f.damage
        );
        assert_eq!(
            f.seek,
            Some(SeekPlan::Present),
            "damaged frames are never seek targets"
        );
        assert!(inspect(&mp3, None).damage.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_float_wav_is_unplayable_on_nxs2() {
        let dir = tmp("level");
        let Some(wav) = make(&dir, "f.wav", &["-c:a", "pcm_f32le"]) else {
            return;
        };
        let level = compat::resolve_level(&Default::default(), Some("nxs2")).unwrap();
        let f = inspect(&wav, Some(&level));
        assert!(f.unplayable.is_some_and(|s| s.float));
        assert!(f.seek.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
