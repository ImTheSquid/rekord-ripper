//! Reading an audio file's properties with ffprobe.
//!
//! Rekordbox stores sample rate, bit depth and length on the track row, so a
//! hand-created row needs them from somewhere. ffprobe is already a dependency of
//! the fingerprint path, so this adds no new requirement.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};

use crate::acquire::types::AudioFormat;
use crate::proc;

/// ffprobe is fast, but a corrupt file can make it work hard.
const PROBE_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq)]
pub struct AudioInfo {
    pub duration_secs: f64,
    pub sample_rate: Option<i64>,
    /// Bits per sample. Absent for lossy formats, which is why it is optional
    /// rather than defaulted to 16.
    pub bit_depth: Option<i64>,
    pub channels: Option<i64>,
    /// Container bitrate in bits per second.
    pub bit_rate: Option<i64>,
    pub codec: Option<String>,
    pub file_size: u64,
    /// Embedded metadata. Rekordbox reads these when you drag a file in, so a
    /// hand-created row that ignored them would be worse than the manual path.
    pub tags: Tags,
}

/// The tags worth putting on a track row.
///
/// Tag keys vary by container — FLAC/Vorbis uses uppercase, MP4 lowercase — so
/// lookups are case-insensitive.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Tags {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub genre: Option<String>,
    pub comment: Option<String>,
    pub year: Option<i64>,
    pub track_no: Option<i64>,
    pub disc_no: Option<i64>,
}

impl AudioInfo {
    /// Whole seconds, as rekordbox stores `Length`.
    pub fn length_secs(&self) -> i64 {
        self.duration_secs.round() as i64
    }

    /// `djmdContent.FileType`, from the codec where possible and the extension
    /// otherwise.
    pub fn rekordbox_file_type(&self, path: &Path) -> Option<i64> {
        // Codec first: an extension can lie, a decoded stream cannot.
        let from_codec = match self.codec.as_deref() {
            Some("flac") => Some(5),
            Some("pcm_s16be" | "pcm_s24be" | "pcm_s16le" | "pcm_s24le") => {
                // AIFF is big-endian PCM, WAV little-endian; the container
                // decides, so fall through to the extension for these.
                None
            }
            Some("mp3") => Some(0),
            Some("aac" | "alac") => Some(1),
            _ => None,
        };
        if from_codec.is_some() {
            return from_codec;
        }
        path.extension()
            .and_then(|e| e.to_str())
            .and_then(|e| e.parse::<AudioFormat>().ok())
            .and_then(|f| f.rekordbox_file_type())
    }
}

/// Probe `path` with a single ffprobe call.
pub fn probe(path: &Path) -> Result<AudioInfo> {
    if !path.exists() {
        bail!("no such audio file: {}", path.display());
    }
    let file_size = std::fs::metadata(path)?.len();

    let mut cmd = proc::capture("ffprobe");
    cmd.args([
        "-v",
        "error",
        "-select_streams",
        "a:0",
        "-show_entries",
        "stream=codec_name,sample_rate,channels,bits_per_raw_sample,bit_rate",
        "-show_entries",
        "format=duration,bit_rate",
        "-show_entries",
        "format_tags",
        "-of",
        "default=noprint_wrappers=1",
    ])
    .arg(path);

    let out = proc::run_with_deadline(cmd, Instant::now() + PROBE_TIMEOUT)?;
    if !out.status.success() {
        bail!(
            "ffprobe failed on {}: {}",
            path.display(),
            proc::stderr_tail(&out.stderr)
        );
    }

    let text = String::from_utf8_lossy(&out.stdout);
    parse_probe(&text, file_size)
        .ok_or_else(|| anyhow!("ffprobe gave no usable stream info for {}", path.display()))
}

/// Parse ffprobe's `key=value` output. Split out so it can be tested without a
/// subprocess.
fn parse_probe(text: &str, file_size: u64) -> Option<AudioInfo> {
    let mut duration = None;
    let mut sample_rate = None;
    let mut bit_depth = None;
    let mut channels = None;
    let mut stream_bit_rate = None;
    let mut format_bit_rate = None;
    let mut codec = None;
    let mut raw_tags: Vec<(String, String)> = Vec::new();

    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let v = v.trim();
        // ffprobe writes a literal "N/A" for absent values.
        if v.is_empty() || v == "N/A" {
            continue;
        }
        // Tags arrive as `TAG:NAME=value`, with case varying by container.
        if let Some(name) = k.trim().strip_prefix("TAG:") {
            raw_tags.push((name.to_ascii_lowercase(), v.to_string()));
            continue;
        }
        match k.trim() {
            "duration" => duration = v.parse::<f64>().ok(),
            "sample_rate" => sample_rate = v.parse::<i64>().ok(),
            "bits_per_raw_sample" => bit_depth = v.parse::<i64>().ok(),
            "channels" => channels = v.parse::<i64>().ok(),
            "codec_name" => codec = Some(v.to_string()),
            // The stream line comes first, so the first bit_rate is the stream's
            // and any later one belongs to the format.
            "bit_rate" => {
                if stream_bit_rate.is_none() {
                    stream_bit_rate = v.parse::<i64>().ok();
                } else {
                    format_bit_rate = v.parse::<i64>().ok();
                }
            }
            _ => {}
        }
    }

    Some(AudioInfo {
        duration_secs: duration?,
        sample_rate,
        bit_depth,
        channels,
        bit_rate: stream_bit_rate.or(format_bit_rate),
        codec,
        file_size,
        tags: build_tags(&raw_tags),
    })
}

fn build_tags(raw: &[(String, String)]) -> Tags {
    let get = |names: &[&str]| -> Option<String> {
        names.iter().find_map(|want| {
            raw.iter()
                .find(|(k, _)| k == want)
                .map(|(_, v)| v.trim().to_string())
                .filter(|v| !v.is_empty())
        })
    };
    // "1/12" is a common track-number form, so take the part before the slash.
    let number = |names: &[&str]| -> Option<i64> {
        get(names).and_then(|v| {
            v.split(['/', '-'])
                .next()
                .and_then(|n| n.trim().parse::<i64>().ok())
        })
    };

    Tags {
        title: get(&["title"]),
        artist: get(&["artist"]),
        album: get(&["album"]),
        album_artist: get(&["album_artist", "albumartist"]),
        genre: get(&["genre"]),
        comment: get(&["comment", "description"]),
        year: number(&["date", "year", "originaldate"]),
        track_no: number(&["track", "tracknumber"]),
        disc_no: number(&["disc", "discnumber"]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FLAC: &str = "\
codec_name=flac
sample_rate=44100
channels=2
bits_per_raw_sample=16
bit_rate=N/A
duration=310.588231
bit_rate=1064321
";

    const MP3: &str = "\
codec_name=mp3
sample_rate=44100
channels=2
bits_per_raw_sample=N/A
bit_rate=128000
duration=265.102
bit_rate=128000
";

    #[test]
    fn parses_a_lossless_stream() {
        let i = parse_probe(FLAC, 41_330_706).unwrap();
        assert_eq!(i.codec.as_deref(), Some("flac"));
        assert_eq!(i.sample_rate, Some(44100));
        assert_eq!(i.bit_depth, Some(16));
        assert_eq!(i.channels, Some(2));
        assert_eq!(i.length_secs(), 311);
        assert_eq!(i.file_size, 41_330_706);
        // The stream had no bitrate, so the format's is used.
        assert_eq!(i.bit_rate, Some(1_064_321));
    }

    #[test]
    fn a_lossy_stream_has_no_bit_depth() {
        // Defaulting this to 16 would write a wrong value onto the track row.
        let i = parse_probe(MP3, 4_242_000).unwrap();
        assert_eq!(i.bit_depth, None);
        assert_eq!(i.bit_rate, Some(128_000));
        assert_eq!(i.length_secs(), 265);
    }

    #[test]
    fn na_and_blank_values_are_treated_as_absent() {
        let i = parse_probe("duration=100\nsample_rate=N/A\nchannels=\n", 1).unwrap();
        assert_eq!(i.sample_rate, None);
        assert_eq!(i.channels, None);
    }

    #[test]
    fn no_duration_means_not_usable_audio() {
        // This is what an HTML page saved as .flac looks like to ffprobe.
        assert!(parse_probe("codec_name=flac\nsample_rate=0\n", 1).is_none());
        assert!(parse_probe("", 1).is_none());
    }

    #[test]
    fn file_type_comes_from_the_codec_not_the_extension() {
        let i = parse_probe(FLAC, 1).unwrap();
        // Even mislabelled, a flac stream is FileType 5.
        assert_eq!(
            i.rekordbox_file_type(Path::new("/x/mislabelled.mp3")),
            Some(5)
        );
    }

    #[test]
    fn pcm_falls_back_to_the_extension_because_the_container_decides() {
        // AIFF and WAV are both PCM; only the container distinguishes them.
        let pcm = parse_probe("codec_name=pcm_s16be\nduration=10\n", 1).unwrap();
        assert_eq!(pcm.rekordbox_file_type(Path::new("/x/a.aiff")), Some(11));
        assert_eq!(pcm.rekordbox_file_type(Path::new("/x/a.wav")), Some(4));
    }

    #[test]
    fn an_unreadable_codec_and_extension_yield_no_file_type() {
        let i = parse_probe("codec_name=vorbis\nduration=10\n", 1).unwrap();
        // Rekordbox cannot open ogg, so there is no FileType to assign.
        assert_eq!(i.rekordbox_file_type(Path::new("/x/a.ogg")), None);
    }

    #[test]
    fn probing_a_missing_file_errors_rather_than_panicking() {
        assert!(probe(Path::new("/nonexistent/x.flac")).is_err());
    }
}
