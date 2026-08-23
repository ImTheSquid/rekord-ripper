//! Audio fingerprint matching, used to decide whether an analysis transfer is
//! safe.
//!
//! # Two axes, not one
//!
//! "Is this the same recording?" is **not sufficient** to justify a transfer.
//! Cues are copied as absolute time offsets (`analysis::build_plan` clones
//! `InMsec`/`InFrame`/`InMpegAbs` verbatim) and the beat grid is copied as opaque
//! ANLZ binary files. Nothing in this crate can read or rewrite ANLZ contents.
//!
//! So if two files are the same recording but differ by a trimmed intro, every
//! copied cue and the whole grid land in the wrong place — and there is no
//! partial fix, because shifting the cue millisecond values would leave the ANLZ
//! grid unshifted, leaving the database and the ANLZ disagreeing about where each
//! cue is while rekordbox reads both. Refusing is strictly better.
//!
//! `match_fingerprints` answers both questions at once: `score` says *same
//! recording*, and `offset1`/`offset2` say *time-aligned*. The verdict fails
//! closed on either.
//!
//! # Resolution, honestly
//!
//! One chromaprint item at the `test2` preset is **123.9 ms** (a 4096-sample
//! frame with ⅔ overlap at 11025 Hz). Even at zero item offset the true shift is
//! only bounded to about **±62 ms**. At 145 BPM a beat is 414 ms, so that is ~15%
//! of a beat — audible drift on a CDJ. This gate proves *no gross shift*; it does
//! not certify sample alignment, and the accept message says so.
//!
//! Measured, not assumed: a 50 ms shift reports 0 ms and passes; 100 ms is the
//! smallest offset reliably caught. Anything finer would need sample-domain
//! correlation, which cannot be a hard gate anyway — lossy codecs add real
//! encoder delay (MP3 ~13–26 ms, Opus 6.5 ms pre-skip), so a small residual lag
//! between a stream and a lossless file of the same master is expected.
//!
//! One further limit, found while calibrating: a shift that is an exact multiple
//! of the audio's own period is undetectable, because the audio genuinely
//! repeats. Irrelevant for music, but it makes periodic test signals useless for
//! validating this axis.
//!
//! # The thresholds are guesses
//!
//! Every constant below is uncalibrated and chosen to fail closed. `rekord-ripper
//! fp <a> <b>` prints the raw numbers so they can be calibrated on real pairs
//! before anything unattended relies on them.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use rusty_chromaprint::{Configuration, Fingerprinter, Segment, match_fingerprints};

use crate::proc;

/// Seconds of audio fingerprinted from the start of each file.
///
/// `fpcalc`'s default. Both sides must use the same window, measured from t=0:
/// coverage is a fraction of the shorter scan, and the alignment reading is only
/// meaningful if neither side was offset before we started.
pub const DEFAULT_WINDOW_SECS: u32 = 120;

/// Max best-segment score. 0..32, lower is more similar.
///
/// Measured on synthetic pink-noise fixtures: same master across codecs scored
/// 0.00–0.30, same master time-shifted 0.10–0.68, a 2% speed change 6.44, and a
/// genuinely different recording 9.18. So the observed gap is roughly 0.7 → 9.2
/// and 8.0 sits inside it, near the upper end.
///
/// Left deliberately loose rather than tightened to ~4.0: noise-versus-noise is
/// an easy discrimination, and real pairs (a remix against its original, a live
/// take against the studio cut) will land in between. Tighten this from your own
/// library with `rekord-ripper fp`, not from the synthetic numbers above.
pub const SCORE_MAX: f64 = 8.0;

/// Min fraction of the shorter scan covered by the single best segment.
///
/// Measured: same recording 0.95–0.98, different recording 0.49. 0.80 sits in
/// that gap with margin on both sides.
pub const COVERAGE_MIN: f32 = 0.80;

/// Max alignment offset in chromaprint items.
///
/// Zero is the only defensible value — one item is already ~124 ms. Measured
/// against deliberately shifted copies: a 250 ms trim reported +248 ms, 500 ms
/// reported +495 ms, 3000 ms reported +2971 ms, and front-padding produced the
/// same magnitudes negated. So the sign is directional (trim positive, pad
/// negative) and magnitudes are accurate to within one item.
///
/// The floor is real and worth knowing: a **50 ms** shift reports 0 ms and is
/// accepted, because it is below the ±62 ms half-item resolution. ~100 ms is the
/// smallest offset this reliably catches.
pub const SHIFT_ITEMS_MAX: i64 = 0;

/// Max |ratio - 1| for duration and BPM before calling it a speed change.
///
/// This check is load-bearing, not a backstop: the 2% speed change measured a
/// score of 6.44, which would have **passed** `SCORE_MAX`. Chromaprint is not
/// tempo-invariant but it degrades gently enough that score alone misses it.
pub const SPEED_RATIO_TOL: f64 = 0.005;

/// Read size when draining ffmpeg's stdout.
const CHUNK_BYTES: usize = 16 * 1024;

/// The fingerprint of one audio stream.
#[derive(Debug, Clone)]
pub struct Fingerprint {
    pub items: Vec<u32>,
    /// Audio actually consumed, which may be less than the requested window.
    pub scanned_secs: f32,
}

impl Fingerprint {
    pub fn is_usable(&self) -> bool {
        // match_fingerprints needs enough items to align at all; a couple of
        // seconds of audio cannot support a verdict.
        self.items.len() >= 32
    }
}

/// The standard configuration. `preset_test2` is the fpcalc/AcoustID default.
///
/// The *same* configuration must be used for both fingerprints and for the
/// comparison, or the result is meaningless — hence one function, no parameter.
pub fn config() -> Configuration {
    Configuration::preset_test2()
}

/// Decode `path` with ffmpeg and fingerprint the first `window_secs`.
///
/// ffmpeg is asked for exactly what chromaprint wants — mono signed 16-bit at the
/// configuration's own sample rate — so no resampling assumptions are involved.
/// Output is streamed into `consume` in chunks; the decoded audio is never fully
/// buffered.
pub fn fingerprint_file(path: &Path, window_secs: u32) -> Result<Fingerprint> {
    if !path.exists() {
        bail!("no such audio file: {}", path.display());
    }
    let cfg = config();
    let rate = cfg.sample_rate();

    let mut cmd = proc::capture("ffmpeg");
    cmd.args([
        // Without -nostdin ffmpeg will happily eat the TUI's stdin.
        "-nostdin", "-v", "error", "-i",
    ])
    .arg(path)
    .args(["-map", "0:a:0"])
    // One extra second so the window is genuinely full rather than a hair short.
    .args(["-t", &(window_secs + 1).to_string()])
    .args(["-f", "s16le", "-acodec", "pcm_s16le", "-ac", "1"])
    .args(["-ar", &rate.to_string(), "-"])
    .stdin(Stdio::null());

    let mut child = cmd.spawn().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => {
            anyhow!("ffmpeg not found — install it, or fingerprinting cannot work")
        }
        _ => anyhow!("could not run ffmpeg: {e}"),
    })?;

    let mut fp = Fingerprinter::new(&cfg);
    fp.start(rate, 1)
        .map_err(|e| anyhow!("chromaprint rejected {rate}Hz mono: {e:?}"))?;

    let want_samples = (window_secs as u64) * (rate as u64);
    let mut consumed: u64 = 0;
    let mut buf = vec![0u8; CHUNK_BYTES];
    let mut samples: Vec<i16> = Vec::with_capacity(CHUNK_BYTES / 2);
    // A read can split an i16 across chunk boundaries; carry the odd byte over.
    let mut carry: Option<u8> = None;
    let mut hit_target = false;

    {
        let mut out = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("ffmpeg produced no stdout pipe"))?;

        loop {
            let n = out.read(&mut buf).context("reading ffmpeg output")?;
            if n == 0 {
                break;
            }
            samples.clear();
            let mut bytes = &buf[..n];
            if let Some(hi) = carry.take()
                && let Some((first, rest)) = bytes.split_first()
            {
                samples.push(i16::from_le_bytes([hi, *first]));
                bytes = rest;
            }
            let mut chunks = bytes.chunks_exact(2);
            for c in &mut chunks {
                samples.push(i16::from_le_bytes([c[0], c[1]]));
            }
            if let [last] = chunks.remainder() {
                carry = Some(*last);
            }

            consumed += samples.len() as u64;
            fp.consume(&samples);

            if consumed >= want_samples {
                hit_target = true;
                break;
            }
        }
    }

    if hit_target {
        // We have what we need; stop the decoder. ffmpeg will see EPIPE, which is
        // expected here and must not be read as a failure.
        let _ = child.kill();
        let _ = child.wait();
    } else {
        let status = child.wait()?;
        if !status.success() {
            let mut err = Vec::new();
            if let Some(mut s) = child.stderr.take() {
                let _ = s.read_to_end(&mut err);
            }
            bail!(
                "ffmpeg could not decode {}: {}",
                path.display(),
                proc::stderr_tail(&err)
            );
        }
    }

    fp.finish();
    let items = fp.fingerprint().to_vec();
    if items.is_empty() {
        bail!("no audio decoded from {}", path.display());
    }
    Ok(Fingerprint {
        items,
        scanned_secs: consumed as f32 / rate as f32,
    })
}

/// Precise duration via ffprobe.
///
/// `djmdContent.Length` is whole seconds, which cannot see the sub-1% difference
/// a speed change produces on a short track, so the speed check needs this.
pub fn probe_duration_secs(path: &Path) -> Result<f64> {
    let mut cmd = proc::capture("ffprobe");
    cmd.args([
        "-v",
        "error",
        "-show_entries",
        "format=duration",
        "-of",
        "csv=p=0",
    ])
    .arg(path);
    let out = proc::run_with_deadline(cmd, Instant::now() + Duration::from_secs(30))?;
    if !out.status.success() {
        bail!(
            "ffprobe failed on {}: {}",
            path.display(),
            proc::stderr_tail(&out.stderr)
        );
    }
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<f64>()
        .map_err(|_| anyhow!("ffprobe gave no duration for {}", path.display()))
}

/// Why a pair was rejected. Each variant gets its own message because the
/// remedies are completely different.
#[derive(Debug, Clone, PartialEq)]
pub enum RejectReason {
    /// No common audio at all.
    NoCommonSegment,
    /// Same-ish, but not similar enough to be the same recording.
    Score { best: f64, max: f64 },
    /// Matched, but over too little of the track — a different edit, or a mix.
    Coverage { got: f32, min: f32 },
    /// Same recording, but not time-aligned. The dangerous case.
    TimeShift { shift_ms: i64, tol_items: i64 },
    /// Pitched or sped up.
    Speed {
        ratio: f64,
        tol: f64,
        from: &'static str,
    },
    /// Too little audio to judge.
    TooShort { items: usize },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Accept {
        score: f64,
        coverage: f32,
        shift_ms: i64,
    },
    Reject {
        reason: RejectReason,
        score: f64,
        coverage: f32,
        shift_ms: i64,
    },
}

impl Verdict {
    pub fn is_accept(&self) -> bool {
        matches!(self, Self::Accept { .. })
    }

    pub fn shift_ms(&self) -> i64 {
        match self {
            Self::Accept { shift_ms, .. } | Self::Reject { shift_ms, .. } => *shift_ms,
        }
    }

    /// One line, with the numbers, for the CLI and the TUI status line.
    pub fn summary(&self) -> String {
        match self {
            Self::Accept {
                score,
                coverage,
                shift_ms,
            } => format!(
                "same recording, aligned (score {score:.2}, coverage {coverage:.2}, \
                 shift {shift_ms}ms ±62ms)"
            ),
            Self::Reject {
                reason,
                score,
                coverage,
                shift_ms,
            } => match reason {
                RejectReason::NoCommonSegment => {
                    "no audio in common — these are different recordings".into()
                }
                RejectReason::Score { best, max } => format!(
                    "different recording (score {best:.2}, needs ≤ {max:.2}; coverage {coverage:.2})"
                ),
                RejectReason::Coverage { got, min } => format!(
                    "only {got:.2} of the track matches (needs ≥ {min:.2}) — \
                     a different edit, or one is a longer mix"
                ),
                RejectReason::TimeShift { shift_ms, .. } => format!(
                    "same recording (score {score:.2}, coverage {coverage:.2}) but time-shifted \
                     by {shift_ms}ms — copied cues and the ANLZ beat grid would land \
                     {:.2}s off, and this tool cannot shift ANLZ contents",
                    *shift_ms as f64 / 1000.0
                ),
                RejectReason::Speed { ratio, tol, from } => format!(
                    "{from} ratio {ratio:.4} exceeds {tol:.4} — likely a pitched or sped-up \
                     version, whose beat grid would be wrong anyway (shift {shift_ms}ms)"
                ),
                RejectReason::TooShort { items } => {
                    format!("only {items} fingerprint items — too little audio to judge")
                }
            },
        }
    }
}

/// Independent evidence of a speed change, from whatever is known.
#[derive(Debug, Clone, Copy, Default)]
pub struct SpeedEvidence {
    /// Precise durations in seconds, ideally from ffprobe.
    pub durations: Option<(f64, f64)>,
    /// BPM values. `djmdContent.BPM` is stored ×100; pass real BPM here.
    pub bpms: Option<(f64, f64)>,
}

impl SpeedEvidence {
    /// The first ratio that exceeds `tol`, if any.
    fn violation(&self, tol: f64) -> Option<(f64, &'static str)> {
        for (pair, name) in [(self.durations, "duration"), (self.bpms, "bpm")] {
            if let Some((a, b)) = pair
                && a > 0.0
                && b > 0.0
            {
                let ratio = a / b;
                if (ratio - 1.0).abs() > tol {
                    return Some((ratio, name));
                }
            }
        }
        None
    }
}

/// Thresholds, so they can be overridden from config without touching the rule.
#[derive(Debug, Clone, Copy)]
pub struct Thresholds {
    pub score_max: f64,
    pub coverage_min: f32,
    pub shift_items_max: i64,
    pub speed_ratio_tol: f64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            score_max: SCORE_MAX,
            coverage_min: COVERAGE_MIN,
            shift_items_max: SHIFT_ITEMS_MAX,
            speed_ratio_tol: SPEED_RATIO_TOL,
        }
    }
}

/// Compare two fingerprints and decide whether a transfer is justified.
pub fn compare(
    a: &Fingerprint,
    b: &Fingerprint,
    speed: SpeedEvidence,
    t: &Thresholds,
) -> Result<Verdict> {
    if !a.is_usable() || !b.is_usable() {
        return Ok(Verdict::Reject {
            reason: RejectReason::TooShort {
                items: a.items.len().min(b.items.len()),
            },
            score: 0.0,
            coverage: 0.0,
            shift_ms: 0,
        });
    }

    let cfg = config();
    let segments = match_fingerprints(&a.items, &b.items, &cfg)
        .map_err(|e| anyhow!("fingerprint comparison failed: {e}"))?;

    Ok(decide(&segments, a, b, speed, t, &cfg))
}

/// The decision rule. Split from the I/O so it can be tested exhaustively over
/// synthetic segments.
fn decide(
    segments: &[Segment],
    a: &Fingerprint,
    b: &Fingerprint,
    speed: SpeedEvidence,
    t: &Thresholds,
    cfg: &Configuration,
) -> Verdict {
    let Some(best) = segments.iter().max_by_key(|s| s.items_count) else {
        return Verdict::Reject {
            reason: RejectReason::NoCommonSegment,
            score: 0.0,
            coverage: 0.0,
            shift_ms: 0,
        };
    };

    let item_secs = cfg.item_duration_in_seconds();
    let overlap = a.scanned_secs.min(b.scanned_secs).max(f32::EPSILON);
    // Only the single best segment counts. Summing fragmented segments is exactly
    // how a time-warped or re-edited file would slip through.
    let coverage = (best.items_count as f32 * item_secs) / overlap;
    let shift_items = best.offset1 as i64 - best.offset2 as i64;
    let shift_ms = (shift_items as f64 * item_secs as f64 * 1000.0).round() as i64;
    let score = best.score;

    let reject = |reason| Verdict::Reject {
        reason,
        score,
        coverage,
        shift_ms,
    };

    // Speed first: it is independent of chromaprint and catches the case where a
    // stretched file still manages a plausible score.
    if let Some((ratio, from)) = speed.violation(t.speed_ratio_tol) {
        return reject(RejectReason::Speed {
            ratio,
            tol: t.speed_ratio_tol,
            from,
        });
    }
    if score > t.score_max {
        return reject(RejectReason::Score {
            best: score,
            max: t.score_max,
        });
    }
    if coverage < t.coverage_min {
        return reject(RejectReason::Coverage {
            got: coverage,
            min: t.coverage_min,
        });
    }
    // The axis that "same recording" alone would miss.
    if shift_items.abs() > t.shift_items_max {
        return reject(RejectReason::TimeShift {
            shift_ms,
            tol_items: t.shift_items_max,
        });
    }

    Verdict::Accept {
        score,
        coverage,
        shift_ms,
    }
}

/// A per-segment readout, for calibrating the thresholds.
pub fn debug_report(a: &Fingerprint, b: &Fingerprint) -> Result<String> {
    let cfg = config();
    let item_secs = cfg.item_duration_in_seconds();
    let mut s = String::new();
    s.push_str(&format!(
        "config     preset_test2  item = {:.4}s  sample_rate {}  mono\n",
        item_secs,
        cfg.sample_rate()
    ));
    s.push_str(&format!(
        "A          {} items, scanned {:.1}s\nB          {} items, scanned {:.1}s\n\n",
        a.items.len(),
        a.scanned_secs,
        b.items.len(),
        b.scanned_secs
    ));

    if !a.is_usable() || !b.is_usable() {
        s.push_str("too little audio to compare\n");
        return Ok(s);
    }

    let segments = match_fingerprints(&a.items, &b.items, &cfg)
        .map_err(|e| anyhow!("fingerprint comparison failed: {e}"))?;

    s.push_str("seg  offset1  offset2  items  duration_s   score  shift_items  shift_ms\n");
    for (i, sg) in segments.iter().enumerate() {
        let shift_items = sg.offset1 as i64 - sg.offset2 as i64;
        s.push_str(&format!(
            "{i:>3}  {:>7}  {:>7}  {:>5}  {:>10.2}  {:>6.2}  {:>11}  {:>8}\n",
            sg.offset1,
            sg.offset2,
            sg.items_count,
            sg.duration(&cfg),
            sg.score,
            shift_items,
            (shift_items as f64 * item_secs as f64 * 1000.0).round() as i64
        ));
    }
    if segments.is_empty() {
        s.push_str("     (no common segments)\n");
    }
    Ok(s)
}

/// A scratch directory that removes itself.
///
/// Needed because a source track may be a rekordbox streaming row with no local
/// audio, so the *reference* side of a comparison sometimes has to be downloaded
/// just to be fingerprinted.
///
/// `Drop` does not run on abort or SIGKILL, so callers also sweep stale scratch
/// directories at startup — that is the real cleanup guarantee, not this.
pub struct ScratchDir {
    path: PathBuf,
    armed: bool,
}

impl ScratchDir {
    pub fn new() -> Result<Self> {
        let path = crate::paths::scratch_root()?.join(format!(
            "fp-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&path)
            .with_context(|| format!("creating scratch dir {}", path.display()))?;
        Ok(Self { path, armed: true })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Keep the contents, for debugging a bad verdict.
    pub fn keep(&mut self) {
        self.armed = false;
    }

    /// Remove scratch directories left behind by a killed process.
    pub fn sweep_stale(max_age: Duration) -> Result<usize> {
        let root = crate::paths::scratch_root()?;
        if !root.exists() {
            return Ok(0);
        }
        let mut removed = 0;
        for entry in std::fs::read_dir(&root)? {
            let entry = entry?;
            if !entry.file_name().to_string_lossy().starts_with("fp-") {
                continue;
            }
            let stale = entry
                .metadata()
                .and_then(|m| m.modified())
                .map(|m| m.elapsed().map(|e| e > max_age).unwrap_or(false))
                .unwrap_or(false);
            if stale && std::fs::remove_dir_all(entry.path()).is_ok() {
                removed += 1;
            }
        }
        Ok(removed)
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        if self.armed {
            // Errors are ignored on purpose: Drop cannot report, and a leftover
            // directory is swept next run.
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fp(items: usize, scanned: f32) -> Fingerprint {
        Fingerprint {
            items: vec![0; items],
            scanned_secs: scanned,
        }
    }

    /// Build a segment without needing real audio.
    fn seg(offset1: usize, offset2: usize, items_count: usize, score: f64) -> Segment {
        Segment {
            offset1,
            offset2,
            items_count,
            score,
        }
    }

    fn item_secs() -> f32 {
        config().item_duration_in_seconds()
    }

    /// Items needed to cover `secs` of audio.
    fn items_for(secs: f32) -> usize {
        (secs / item_secs()).round() as usize
    }

    fn judge(segments: &[Segment], scanned: f32, speed: SpeedEvidence) -> Verdict {
        let a = fp(items_for(scanned), scanned);
        let b = fp(items_for(scanned), scanned);
        decide(segments, &a, &b, speed, &Thresholds::default(), &config())
    }

    #[test]
    fn the_documented_item_duration_is_what_the_library_actually_reports() {
        // The ±62ms resolution claim in the module docs depends on this.
        let d = item_secs();
        assert!(
            (d - 0.1239).abs() < 0.0005,
            "item duration changed: {d} — the shift tolerance reasoning needs revisiting"
        );
        assert_eq!(config().sample_rate(), 11025);
    }

    #[test]
    fn an_aligned_full_length_match_is_accepted() {
        let v = judge(
            &[seg(0, 0, items_for(118.0), 2.41)],
            120.0,
            SpeedEvidence::default(),
        );
        assert!(v.is_accept(), "got {v:?}");
        assert_eq!(v.shift_ms(), 0);
        assert!(v.summary().contains("aligned"), "{}", v.summary());
    }

    #[test]
    fn the_accept_message_admits_its_own_resolution_limit() {
        // Claiming exact alignment would be overstating a 124ms-per-item measure.
        let v = judge(
            &[seg(0, 0, items_for(118.0), 1.0)],
            120.0,
            SpeedEvidence::default(),
        );
        assert!(v.summary().contains("±62ms"), "{}", v.summary());
    }

    #[test]
    fn a_time_shifted_match_is_rejected_even_though_the_recording_matches() {
        // The whole reason this gate has two axes: cues and the ANLZ grid would
        // land uniformly wrong, and there is no way to compensate.
        let shift = 26; // items, ~3.2s
        let v = judge(
            &[seg(shift, 0, items_for(118.0), 1.20)],
            120.0,
            SpeedEvidence::default(),
        );
        match &v {
            Verdict::Reject {
                reason: RejectReason::TimeShift { shift_ms, .. },
                score,
                ..
            } => {
                assert!(
                    *shift_ms > 3000 && *shift_ms < 3400,
                    "shift was {shift_ms}ms"
                );
                assert!(*score < 2.0, "the recording itself matched fine");
            }
            other => panic!("a shifted pair must be rejected, got {other:?}"),
        }
        // The message must name the shift so the user can judge it themselves.
        assert!(v.summary().contains("time-shifted"), "{}", v.summary());
        assert!(v.summary().contains("ANLZ"), "{}", v.summary());
    }

    #[test]
    fn shift_direction_is_preserved_in_the_reported_value() {
        let neg = judge(
            &[seg(0, 26, items_for(118.0), 1.0)],
            120.0,
            SpeedEvidence::default(),
        );
        let pos = judge(
            &[seg(26, 0, items_for(118.0), 1.0)],
            120.0,
            SpeedEvidence::default(),
        );
        assert!(neg.shift_ms() < 0, "got {}", neg.shift_ms());
        assert!(pos.shift_ms() > 0, "got {}", pos.shift_ms());
        assert_eq!(neg.shift_ms(), -pos.shift_ms());
    }

    #[test]
    fn a_one_item_shift_is_rejected_because_one_item_is_already_124ms() {
        let v = judge(
            &[seg(1, 0, items_for(118.0), 1.0)],
            120.0,
            SpeedEvidence::default(),
        );
        assert!(!v.is_accept(), "a single item is ~124ms of drift");
    }

    #[test]
    fn the_sub_item_blind_spot_is_documented_by_this_test_not_hidden() {
        // Measured against real shifted audio: a 50ms offset resolves to zero
        // items and is accepted. This is the honest floor of the gate, not a bug,
        // and the accept message carries the ±62ms caveat because of it.
        let v = judge(
            &[seg(0, 0, items_for(118.0), 1.15)],
            120.0,
            SpeedEvidence::default(),
        );
        assert!(v.is_accept());
        assert!(
            v.summary().contains("±62ms"),
            "an accept must state its resolution: {}",
            v.summary()
        );
    }

    #[test]
    fn a_speed_change_is_caught_even_at_a_score_that_would_pass() {
        // Measured: a 2% tempo change scored 6.44, under SCORE_MAX of 8.0. Score
        // alone would have accepted it, which is why the duration check exists.
        assert!(6.44 < SCORE_MAX, "the premise of this test");
        let v = judge(
            &[seg(0, 0, items_for(118.0), 6.44)],
            120.0,
            SpeedEvidence {
                durations: Some((150.0, 147.06)),
                bpms: None,
            },
        );
        assert!(matches!(
            v,
            Verdict::Reject {
                reason: RejectReason::Speed { .. },
                ..
            }
        ));
    }

    #[test]
    fn a_different_recording_is_rejected_on_score() {
        let v = judge(
            &[seg(0, 0, items_for(118.0), 19.0)],
            120.0,
            SpeedEvidence::default(),
        );
        assert!(matches!(
            v,
            Verdict::Reject {
                reason: RejectReason::Score { .. },
                ..
            }
        ));
    }

    #[test]
    fn a_partial_match_is_rejected_on_coverage() {
        // A long DJ-mix upload, or a different edit.
        let v = judge(
            &[seg(0, 0, items_for(30.0), 1.0)],
            120.0,
            SpeedEvidence::default(),
        );
        match v {
            Verdict::Reject {
                reason: RejectReason::Coverage { got, .. },
                ..
            } => assert!(got < 0.3, "coverage was {got}"),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn coverage_uses_only_the_best_segment_not_the_sum() {
        // Summing fragments is how a re-edit sneaks through, so many small
        // matching pieces must not add up to an accept.
        let n = items_for(20.0);
        let fragments: Vec<Segment> = (0..6).map(|i| seg(i * n, i * n, n, 1.0)).collect();
        let v = judge(&fragments, 120.0, SpeedEvidence::default());
        assert!(
            !v.is_accept(),
            "six 20s fragments must not sum to a full-length match: {v:?}"
        );
    }

    #[test]
    fn no_common_audio_is_rejected() {
        let v = judge(&[], 120.0, SpeedEvidence::default());
        assert!(matches!(
            v,
            Verdict::Reject {
                reason: RejectReason::NoCommonSegment,
                ..
            }
        ));
        assert!(v.summary().contains("different recordings"));
    }

    #[test]
    fn a_sped_up_reupload_is_rejected_on_duration_even_with_a_good_score() {
        // Chromaprint is not tempo-invariant, but this is the independent check
        // that does not depend on it degrading the way we expect.
        let v = judge(
            &[seg(0, 0, items_for(118.0), 1.0)],
            120.0,
            SpeedEvidence {
                durations: Some((124.0, 121.5)), // ~2% faster
                bpms: None,
            },
        );
        match v {
            Verdict::Reject {
                reason: RejectReason::Speed { ratio, from, .. },
                ..
            } => {
                assert_eq!(from, "duration");
                assert!(ratio > 1.01, "ratio was {ratio}");
            }
            other => panic!("a 2% speed change must be rejected, got {other:?}"),
        }
    }

    #[test]
    fn bpm_disagreement_alone_is_enough_to_reject() {
        let v = judge(
            &[seg(0, 0, items_for(118.0), 1.0)],
            120.0,
            SpeedEvidence {
                durations: None,
                bpms: Some((128.0, 130.0)),
            },
        );
        assert!(matches!(
            v,
            Verdict::Reject {
                reason: RejectReason::Speed { from: "bpm", .. },
                ..
            }
        ));
    }

    #[test]
    fn tiny_speed_differences_within_tolerance_are_accepted() {
        // Encoder and container rounding, not a real speed change.
        let v = judge(
            &[seg(0, 0, items_for(118.0), 1.0)],
            120.0,
            SpeedEvidence {
                durations: Some((124.383, 124.352)),
                bpms: Some((163.16, 163.16)),
            },
        );
        assert!(v.is_accept(), "got {v:?}");
    }

    #[test]
    fn absent_speed_evidence_does_not_reject() {
        // A streaming source may have no precise duration; that is not a failure.
        assert!(
            judge(
                &[seg(0, 0, items_for(118.0), 1.0)],
                120.0,
                SpeedEvidence {
                    durations: None,
                    bpms: None
                }
            )
            .is_accept()
        );
    }

    #[test]
    fn zero_or_missing_values_are_not_treated_as_a_ratio() {
        // 0/0 must not become NaN and slip past the comparison.
        assert!(
            judge(
                &[seg(0, 0, items_for(118.0), 1.0)],
                120.0,
                SpeedEvidence {
                    durations: Some((0.0, 124.0)),
                    bpms: Some((0.0, 0.0))
                }
            )
            .is_accept()
        );
    }

    #[test]
    fn too_little_audio_is_rejected_rather_than_guessed() {
        let v = compare(
            &fp(4, 0.5),
            &fp(4, 0.5),
            SpeedEvidence::default(),
            &Thresholds::default(),
        )
        .unwrap();
        assert!(matches!(
            v,
            Verdict::Reject {
                reason: RejectReason::TooShort { .. },
                ..
            }
        ));
    }

    #[test]
    fn coverage_is_measured_against_the_shorter_scan() {
        // Otherwise a short file compared against a long one would look partial.
        let short = fp(items_for(60.0), 60.0);
        let long = fp(items_for(120.0), 120.0);
        let v = decide(
            &[seg(0, 0, items_for(59.0), 1.0)],
            &short,
            &long,
            SpeedEvidence::default(),
            &Thresholds::default(),
            &config(),
        );
        assert!(
            v.is_accept(),
            "a fully-matched shorter file should pass: {v:?}"
        );
    }

    #[test]
    fn thresholds_are_overridable_without_changing_the_rule() {
        let segments = [seg(0, 0, items_for(118.0), 12.0)];
        let a = fp(items_for(120.0), 120.0);
        let strict = decide(
            &segments,
            &a,
            &a,
            SpeedEvidence::default(),
            &Thresholds::default(),
            &config(),
        );
        assert!(!strict.is_accept());

        let loose = decide(
            &segments,
            &a,
            &a,
            SpeedEvidence::default(),
            &Thresholds {
                score_max: 15.0,
                ..Default::default()
            },
            &config(),
        );
        assert!(loose.is_accept(), "raising score_max should admit it");
    }

    #[test]
    fn debug_report_prints_every_segment_for_calibration() {
        let a = fp(items_for(120.0), 120.0);
        let r = debug_report(&a, &a).unwrap();
        assert!(r.contains("preset_test2"));
        assert!(r.contains("item = 0.12"), "{r}");
        assert!(r.contains("shift_ms"), "{r}");
    }

    #[test]
    fn debug_report_says_so_when_there_is_not_enough_audio() {
        let r = debug_report(&fp(2, 0.2), &fp(2, 0.2)).unwrap();
        assert!(r.contains("too little audio"), "{r}");
    }

    #[test]
    fn a_missing_file_is_an_error_not_a_panic() {
        let err = fingerprint_file(Path::new("/nonexistent/x.flac"), 10).unwrap_err();
        assert!(err.to_string().contains("no such audio file"), "{err}");
    }

    #[test]
    fn scratch_dirs_delete_themselves_and_can_be_kept() {
        let path = {
            let s = ScratchDir::new().unwrap();
            let p = s.path().to_path_buf();
            assert!(p.exists());
            std::fs::write(p.join("x.bin"), b"data").unwrap();
            p
        };
        assert!(!path.exists(), "dropping a scratch dir must remove it");

        let kept = {
            let mut s = ScratchDir::new().unwrap();
            s.keep();
            s.path().to_path_buf()
        };
        assert!(kept.exists(), "keep() should defuse cleanup");
        std::fs::remove_dir_all(&kept).unwrap();
    }

    #[test]
    fn sweeping_leaves_fresh_scratch_dirs_alone() {
        let s = ScratchDir::new().unwrap();
        let path = s.path().to_path_buf();
        // A day-old cutoff means a directory created just now is not stale.
        ScratchDir::sweep_stale(Duration::from_secs(86_400)).unwrap();
        assert!(path.exists(), "a scratch dir in use must survive a sweep");
    }
}
