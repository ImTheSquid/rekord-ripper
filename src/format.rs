//! Small pure formatting helpers shared between `dump`, `analysis` rendering,
//! and the TUI row formatter. Keep this module dependency-free.

pub(crate) fn file_type_name(ft: Option<i64>) -> &'static str {
    // Mapping observed in master.db dumps; see also pyrekordbox tables.py.
    match ft {
        Some(0) => "MP3",
        Some(1) => "M4A",
        Some(4) => "WAV",
        Some(5) => "FLAC",
        Some(11) => "AIFF",
        Some(19) => "streaming",
        Some(_) => "unknown",
        None => "-",
    }
}

pub(crate) fn format_bpm(bpm: Option<i64>) -> String {
    // BPM is stored as integer * 100 (e.g. 12800 = 128.00).
    match bpm {
        Some(v) => format!("{:.2}", v as f64 / 100.0),
        None => "-".into(),
    }
}

pub(crate) fn format_length(secs: Option<i64>) -> String {
    match secs {
        Some(s) => format!("{}:{:02} ({s}s)", s / 60, s % 60),
        None => "-".into(),
    }
}

pub(crate) fn format_msec(ms: i64) -> String {
    let total_secs = ms / 1000;
    let frac = ms % 1000;
    format!("{}:{:02}.{:03}", total_secs / 60, total_secs % 60, frac)
}

pub(crate) fn kind_label(kind: Option<i64>) -> &'static str {
    // djmdCue.Kind: 0 = memory cue; 1..=16 = hot cue slots A..P.
    match kind {
        Some(0) => "memory",
        Some(k) if (1..=16).contains(&k) => match k {
            1 => "hot A",
            2 => "hot B",
            3 => "hot C",
            4 => "hot D",
            5 => "hot E",
            6 => "hot F",
            7 => "hot G",
            8 => "hot H",
            9 => "hot I",
            10 => "hot J",
            11 => "hot K",
            12 => "hot L",
            13 => "hot M",
            14 => "hot N",
            15 => "hot O",
            16 => "hot P",
            _ => unreachable!(),
        },
        _ => "?",
    }
}
