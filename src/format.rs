//! Small pure formatting helpers shared between `dump`, `analysis` rendering,
//! and the TUI row formatter. Keep this module dependency-free.

pub fn file_type_name(ft: Option<i64>) -> &'static str {
    // Counted over the rows rekordbox itself wrote in a real master.db: 1 is
    // mp3 (522), 4 m4a (70), 5 flac (725), 11 wav (355). 12 is aiff, read back
    // off a probe file imported for the purpose. 0 appears on nothing rekordbox
    // created, and a row carrying it reads as "Unknown Format" and will not play.
    match ft {
        Some(0) => "unplayable (FileType 0)",
        Some(1) => "MP3",
        Some(4 | 6) => "M4A",
        Some(5) => "FLAC",
        Some(11) => "WAV",
        Some(12) => "AIFF",
        Some(19) => "SoundCloud",
        Some(21) => "Beatport",
        Some(25) => "Spotify",
        Some(26) => "Apple Music",
        Some(_) => "unknown",
        None => "-",
    }
}

/// File types that are a streaming link rather than a file.
const STREAMING_FILE_TYPES: &[i64] = &[19, 21, 25, 26];

/// `djmdContent.ServiceID` for a row that belongs to rekordbox Cloud Library
/// Sync rather than to this machine.
const CLOUD_SERVICE_ID: i64 = 2;

/// Everything the filter language needs to know about one row's audio.
pub(crate) struct TrackFacts {
    pub origin: Origin,
    pub file_type: Option<i64>,
    pub cue_count: i64,
    pub locked: bool,
    /// `None` when presence could not be checked — see `crate::presence`.
    pub present: Option<bool>,
}

/// Where a row's audio actually lives.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Origin {
    /// A file on this machine.
    Local,
    /// A real file, but one Cloud Library Sync owns — its `FolderPath` is a
    /// `/contents_…` cloud path, so it may or may not be downloaded here.
    Cloud,
    /// A service link with no file behind it at all.
    Stream,
}

impl Origin {
    fn tag(&self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Cloud => "cloud",
            Self::Stream => "stream",
        }
    }
}

/// The keyword tags a query's `is:` / `has:` / `type:` terms match against.
///
/// Space-delimited *and* space-padded, so a term matches a whole tag: `is:wav`
/// must never be satisfied by some future `wavpack`.
pub(crate) fn track_tags(f: TrackFacts) -> String {
    let mut tags: Vec<&str> = Vec::new();
    tags.push(f.origin.tag());
    match f.present {
        Some(true) => tags.push("present"),
        Some(false) => tags.push("missing"),
        None => {}
    }
    // Streaming rows get no format tag on purpose: `is:lossy` should mean "you
    // have a lossy file", which is the question worth asking before shopping.
    match f.file_type {
        Some(0) => tags.extend(["mp3", "lossy"]),
        Some(1) => tags.extend(["m4a", "lossy"]),
        Some(4) => tags.extend(["wav", "lossless"]),
        Some(5) => tags.extend(["flac", "lossless"]),
        Some(11) => tags.extend(["aiff", "lossless"]),
        _ => {}
    }
    if f.cue_count > 0 {
        tags.push("cues");
    }
    if f.locked {
        tags.push("locked");
    }
    format!(" {} ", tags.join(" "))
}

/// A streaming entry keeps a service URI (`soundcloud:tracks:123`) where a real
/// track keeps a path — `/Users/…` here, `C:/…` from a synced Windows machine,
/// `/contents_…` in the cloud. The separator test catches services this list has
/// never seen; the list catches Beatport, whose URI is path-shaped.
///
/// Stream is decided before cloud, so a service link never reads as a file just
/// because the row is synced.
pub(crate) fn origin(
    file_type: Option<i64>,
    folder_path: Option<&str>,
    service_id: Option<i64>,
) -> Origin {
    let has_path = folder_path.is_some_and(|p| p.contains('/') || p.contains('\\'));
    if file_type.is_some_and(|ft| STREAMING_FILE_TYPES.contains(&ft)) || !has_path {
        Origin::Stream
    } else if service_id == Some(CLOUD_SERVICE_ID) {
        Origin::Cloud
    } else {
        Origin::Local
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

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(ft: Option<i64>, path: Option<&str>, sid: Option<i64>) -> TrackFacts {
        TrackFacts {
            origin: origin(ft, path, sid),
            file_type: ft,
            cue_count: 0,
            locked: false,
            present: None,
        }
    }

    fn tags(ft: i64, path: &str, sid: i64) -> String {
        track_tags(facts(Some(ft), Some(path), Some(sid)))
    }

    #[test]
    fn a_service_uri_is_a_stream_however_it_is_spelled() {
        // Shapes taken from a real master.db.
        assert!(tags(19, "soundcloud:tracks:1011732511", 0).contains(" stream "));
        assert!(tags(26, "apple-music:tracks:1083723407", 0).contains(" stream "));
        assert!(tags(25, "spotify:track:0BLAbNPYQspTAF7mKmS4Kb", 0).contains(" stream "));
        // Beatport's link is path-shaped, so the file-type list is what catches
        // it; an empty path is nothing to play either way.
        assert!(tags(21, "/bs/catalog/tracks/6238554/", 0).contains(" stream "));
        assert!(track_tags(facts(None, None, None)).contains(" stream "));
    }

    #[test]
    fn a_path_is_local_whichever_machine_wrote_it() {
        assert!(tags(5, "/Users/x/Music/a.flac", 0).contains(" local "));
        assert!(tags(1, "C:/users/x/Music/a.mp3", 0).contains(" local "));
        assert!(tags(11, "D:/Contents/a.aiff", 0).contains(" local "));
    }

    #[test]
    fn cloud_sync_is_its_own_origin_not_a_local_file() {
        let cloud = tags(5, "/contents_2768718261/artist/album/01.flac", 2);
        assert!(cloud.contains(" cloud "));
        assert!(!cloud.contains(" local "));
        assert!(!cloud.contains(" stream "));
        // It is still a real FLAC, so shopping filters can see the format.
        assert!(cloud.contains(" flac ") && cloud.contains(" lossless "));
        // A cloud row synced from a Windows machine is still cloud.
        assert!(tags(1, "C:/Users/x/Music/contents_4204620759/a.m4a", 2).contains(" cloud "));
    }

    #[test]
    fn a_stream_carries_no_format_keyword() {
        let stream = tags(19, "soundcloud:tracks:1", 0);
        assert!(!stream.contains(" lossy "));
        assert!(!stream.contains(" lossless "));
    }

    #[test]
    fn cues_and_lock_are_tagged_only_when_present() {
        let bare = track_tags(facts(Some(5), Some("/a/b.flac"), Some(0)));
        assert!(!bare.contains(" cues ") && !bare.contains(" locked "));
        let full = track_tags(TrackFacts {
            cue_count: 4,
            locked: true,
            ..facts(Some(5), Some("/a/b.flac"), Some(0))
        });
        assert!(full.contains(" cues ") && full.contains(" locked "));
    }

    #[test]
    fn presence_is_tagged_only_when_it_could_be_checked() {
        let unchecked = facts(Some(5), Some("/a/b.flac"), Some(0));
        let tags = track_tags(unchecked);
        assert!(!tags.contains(" present ") && !tags.contains(" missing "));

        let here = track_tags(TrackFacts {
            present: Some(true),
            ..facts(Some(5), Some("/a/b.flac"), Some(0))
        });
        assert!(here.contains(" present ") && !here.contains(" missing "));

        let gone = track_tags(TrackFacts {
            present: Some(false),
            ..facts(Some(5), Some("/a/b.flac"), Some(0))
        });
        assert!(gone.contains(" missing ") && !gone.contains(" present "));
        // Presence is a separate axis from origin, so both still read.
        assert!(gone.contains(" local ") && gone.contains(" flac "));
    }
}
