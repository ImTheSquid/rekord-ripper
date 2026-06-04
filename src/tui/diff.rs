use crate::analysis::CopyOpts;
use crate::format::{file_type_name, format_bpm, format_length};

use super::data::TrackRow;

/// Compact two-or-three-line summary of the (src, dst) pair for the preview
/// pane. Returns a vec of lines. No ratatui types here — keep this testable.
pub fn render_pair(src: Option<&TrackRow>, dst: Option<&TrackRow>, _opts: CopyOpts) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(s) = src {
        lines.push(format_row_line("src", s));
    } else {
        lines.push("src   (none selected)".into());
    }
    if let Some(d) = dst {
        lines.push(format_row_line("dst", d));
    } else {
        lines.push("dst   (none selected)".into());
    }
    if let (Some(s), Some(d)) = (src, dst) {
        lines.push(format_diff_line(s, d));
    }
    lines
}

fn format_row_line(label: &str, r: &TrackRow) -> String {
    let lock = if r.locked { "yes" } else { "no " };
    format!(
        "{label}   BPM {bpm:<6}  Len {len:<10}  Cues {cues:<3}  Lock {lock}  Type {ty}",
        bpm = format_bpm(r.bpm),
        len = format_length(r.length),
        cues = r.cue_count,
        lock = lock,
        ty = file_type_name(r.file_type),
    )
}

fn format_diff_line(src: &TrackRow, dst: &TrackRow) -> String {
    let bpm_delta = match (src.bpm, dst.bpm) {
        (Some(s), Some(d)) if s != d => format!("BPM {:.2} → {:.2}", d as f64 / 100.0, s as f64 / 100.0),
        (Some(s), None) => format!("BPM - → {:.2}", s as f64 / 100.0),
        _ => "BPM unchanged".into(),
    };
    let cue_delta = format!("cues {} → {}", dst.cue_count, src.cue_count);
    let len_delta = match (src.length, dst.length) {
        (Some(s), Some(d)) if (s - d).abs() <= 1 => "len ≈".into(),
        (Some(s), Some(d)) => format!("len Δ {}s [skipped]", (s - d).abs()),
        _ => "len -".into(),
    };
    format!("Δ     {bpm_delta}   {cue_delta}   {len_delta}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(bpm: Option<i64>, length: Option<i64>, cue_count: i64, locked: bool) -> TrackRow {
        use crate::tui::data::TrackRow;
        TrackRow {
            id: "id".into(),
            title: "T".into(),
            artist: "A".into(),
            bpm,
            length,
            cue_count,
            analysed: if locked { 233 } else { 105 },
            file_type: Some(5),
            norm_title: "t".into(),
            locked,
            is_unlocked_cueless_audio: !locked && cue_count == 0,
            search_blob: "t a".into(),
        }
    }

    #[test]
    fn diff_line_skips_length_when_outside_tolerance() {
        let src = fixture(Some(14600), Some(221), 4, false);
        let dst = fixture(Some(15000), Some(225), 0, false);
        let lines = render_pair(Some(&src), Some(&dst), CopyOpts::default());
        let diff = lines.iter().find(|l| l.starts_with("Δ")).unwrap();
        assert!(diff.contains("len Δ 4s [skipped]"), "{diff}");
        assert!(diff.contains("BPM 150.00 → 146.00"), "{diff}");
        assert!(diff.contains("cues 0 → 4"), "{diff}");
    }

    #[test]
    fn diff_line_marks_length_unchanged_when_within_tolerance() {
        let src = fixture(Some(14600), Some(221), 4, false);
        let dst = fixture(Some(15000), Some(222), 0, false);
        let lines = render_pair(Some(&src), Some(&dst), CopyOpts::default());
        let diff = lines.iter().find(|l| l.starts_with("Δ")).unwrap();
        assert!(diff.contains("len ≈"), "{diff}");
    }

    #[test]
    fn handles_missing_sides() {
        let src = fixture(Some(14600), Some(221), 4, false);
        let none_src = render_pair(None, Some(&src), CopyOpts::default());
        assert!(none_src[0].contains("(none selected)"));
        let none_dst = render_pair(Some(&src), None, CopyOpts::default());
        assert!(none_dst[1].contains("(none selected)"));
        let none_both = render_pair(None, None, CopyOpts::default());
        assert_eq!(none_both.len(), 2);
    }
}
