//! Compatibility levels: convert what a target player cannot play, and point the
//! existing rekordbox row at the result.
//!
//! The row keeps its ID, so cues, beat grid, playlists and history stay put; only
//! the file-derived columns change. The original file is never touched, which is
//! what makes [`undo`] possible.
//!
//! # Why the old analysis stays valid
//!
//! Cues are absolute times and the ANLZ grid is opaque binary (see
//! `crate::fingerprint`), so they only hold on a file with the same timeline.
//! Every conversion is checked against its source in the sample domain before the
//! row moves. The fingerprint gate cannot do this job: its ±62 ms floor is wider
//! than an MP3 encoder delay.

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::acquire::types::AudioFormat;
use crate::audio::{self, AudioInfo};
use crate::db::{MasterDb, now_db_string};
use crate::format::{self, Origin};
use crate::proc;

/// A whole-file encode. FLAC to AIFF runs at hundreds of times real time, so this
/// only trips on a hung ffmpeg.
const ENCODE_TIMEOUT: Duration = Duration::from_secs(600);
const DECODE_TIMEOUT: Duration = Duration::from_secs(120);

/// Alignment is measured at this rate: 0.09 ms per sample, far finer than the
/// tolerance, and cheap to correlate.
const ALIGN_RATE: u32 = 11025;
const ALIGN_WINDOW_SECS: f64 = 10.0;
/// Past most intros, so the window holds music rather than silence.
const ALIGN_START_SECS: f64 = 30.0;
/// Widest shift searched for. An MP3 encoder delay is about 26 ms.
const ALIGN_SEARCH_MS: f64 = 100.0;
/// Largest shift accepted. A cue 1 ms off is inaudible.
const ALIGN_TOLERANCE_MS: f64 = 1.0;
/// Below this the two windows are not the same audio, wherever the peak is.
const MIN_CORRELATION: f64 = 0.9;
/// A window this quiet has nothing to align on.
const SILENCE_RMS: f64 = 1e-4;
/// Catches a truncated encode. One MP3 frame is 26 ms.
const DURATION_TOLERANCE_SECS: f64 = 0.05;

/// Sample rates a 320 kbps MPEG-1 Layer III encode can use.
const MP3_RATES: [u32; 3] = [32000, 44100, 48000];
/// Every manual checked lists MP3 and AAC at 44.1/48 kHz only, even on players
/// that take 96 kHz PCM.
const LOSSY_MAX_RATE: u32 = 48000;
const PCM_DEPTHS: [u8; 3] = [16, 24, 32];

/// A codec a player decodes. Coarser than [`AudioFormat`]: a level says which
/// codecs a player can play, not how well they were encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Codec {
    Mp3,
    Aac,
    Wav,
    Aiff,
    Flac,
    Alac,
}

impl Codec {
    pub fn is_lossless(self) -> bool {
        matches!(self, Self::Wav | Self::Aiff | Self::Flac | Self::Alac)
    }

    fn extension(self) -> &'static str {
        match self {
            Self::Mp3 => "mp3",
            Self::Aac | Self::Alac => "m4a",
            Self::Wav => "wav",
            Self::Aiff => "aiff",
            Self::Flac => "flac",
        }
    }
}

impl fmt::Display for Codec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Mp3 => "MP3",
            Self::Aac => "AAC",
            Self::Wav => "WAV",
            Self::Aiff => "AIFF",
            Self::Flac => "FLAC",
            Self::Alac => "ALAC",
        })
    }
}

impl FromStr for Codec {
    type Err = anyhow::Error;

    /// The same spellings as `format_preference`. A bitrate suffix is ignored,
    /// and `m4a` means AAC.
    fn from_str(s: &str) -> Result<Self> {
        Ok(match s.parse::<AudioFormat>()? {
            AudioFormat::Mp3(_) | AudioFormat::Mp3V0 => Self::Mp3,
            AudioFormat::Aac(_) => Self::Aac,
            AudioFormat::Wav => Self::Wav,
            AudioFormat::Aiff => Self::Aiff,
            AudioFormat::Flac => Self::Flac,
            AudioFormat::Alac => Self::Alac,
            AudioFormat::Ogg | AudioFormat::Opus => {
                bail!("rekordbox cannot read '{s}', so no player level can include it")
            }
        })
    }
}

/// What a class of player can play.
#[derive(Debug, Clone, PartialEq)]
pub struct Level {
    pub name: String,
    /// Which players it stands for, for `compat --levels`.
    pub about: String,
    pub codecs: Vec<Codec>,
    /// Allowed rates, not a maximum: older players refuse 32 kHz as well as 96.
    pub sample_rates: Vec<u32>,
    /// Allowed PCM bit depths. Never applies to lossy audio.
    pub bit_depths: Vec<u8>,
}

/// A level as written under `[compat.levels.<name>]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LevelSpec {
    pub codecs: Vec<String>,
    pub sample_rates: Vec<u32>,
    pub bit_depths: Vec<u8>,
}

/// The `[compat]` section of `config.toml`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CompatConfig {
    /// The level `compat` uses when `--level` is not given.
    pub default_level: Option<String>,
    /// Your own levels, by name. A name shared with a built-in replaces it.
    pub levels: BTreeMap<String, LevelSpec>,
}

impl Level {
    fn from_spec(name: &str, spec: &LevelSpec) -> Result<Self> {
        // The name goes into a filename when two targets collide.
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            bail!("a level name may only use letters, digits, '-', '_' and '.'");
        }
        if spec.codecs.is_empty() || spec.sample_rates.is_empty() || spec.bit_depths.is_empty() {
            bail!("codecs, sample_rates and bit_depths must each list at least one value");
        }
        let codecs = spec
            .codecs
            .iter()
            .map(|c| c.parse::<Codec>())
            .collect::<Result<Vec<_>>>()?;
        if let Some(r) = spec
            .sample_rates
            .iter()
            .find(|r| !(8000..=384_000).contains(*r))
        {
            bail!("sample rate {r} is not an audio rate in Hz");
        }
        if let Some(d) = spec.bit_depths.iter().find(|d| !PCM_DEPTHS.contains(d)) {
            bail!("bit depth {d} is not one of 16, 24 or 32");
        }
        Ok(Self {
            name: name.to_string(),
            about: "defined in config.toml".into(),
            codecs,
            sample_rates: spec.sample_rates.clone(),
            bit_depths: spec.bit_depths.clone(),
        })
    }

    /// True when a player at this level plays `src` as it is.
    pub fn plays(&self, src: &SourceFormat) -> bool {
        self.codecs.contains(&src.codec)
            && self.sample_rates.contains(&src.sample_rate)
            && match src.bit_depth {
                None => src.sample_rate <= LOSSY_MAX_RATE,
                Some(d) => !src.float && self.bit_depths.contains(&d),
            }
    }
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let codecs: Vec<String> = self.codecs.iter().map(|c| c.to_string()).collect();
        let rates: Vec<String> = self.sample_rates.iter().map(|r| khz(*r)).collect();
        let depths: Vec<String> = self.bit_depths.iter().map(|d| d.to_string()).collect();
        write!(
            f,
            "{} · {} · {}-bit",
            codecs.join(" "),
            rates.join(" "),
            depths.join("/")
        )
    }
}

/// Each from the "playable file formats" table in the players' operating
/// instructions (downloads.support.alphatheta.com); the README links them.
fn builtin_levels() -> Vec<Level> {
    let level = |name: &str, about: &str, codecs: &[Codec], rates: &[u32]| Level {
        name: name.into(),
        about: about.into(),
        codecs: codecs.to_vec(),
        sample_rates: rates.to_vec(),
        bit_depths: vec![16, 24],
    };
    use Codec::*;
    vec![
        level(
            "legacy",
            "CDJ-2000, CDJ-2000NXS, CDJ-900(NXS), CDJ-850, CDJ-350, XDJ-1000, XDJ-700, XDJ-RX, XDJ-RX2",
            &[Mp3, Aac, Wav, Aiff],
            &[44100, 48000],
        ),
        level(
            "flac48",
            "XDJ-1000MK2, XDJ-RX3, XDJ-XZ (FLAC only at 44.1/48k; the RX3 and XZ have no ALAC)",
            &[Mp3, Aac, Wav, Aiff, Flac],
            &[44100, 48000],
        ),
        level(
            "nxs2",
            "CDJ-2000NXS2, CDJ-3000, OPUS-QUAD",
            &[Mp3, Aac, Wav, Aiff, Flac, Alac],
            &[44100, 48000, 88200, 96000],
        ),
    ]
}

/// Built-in levels, then `[compat.levels]`. A configured level replaces a
/// built-in of the same name.
pub fn levels(cfg: &CompatConfig) -> Result<Vec<Level>> {
    let mut out = builtin_levels();
    for (name, spec) in &cfg.levels {
        let level =
            Level::from_spec(name, spec).with_context(|| format!("[compat.levels.{name}]"))?;
        match out.iter_mut().find(|l| l.name == *name) {
            Some(slot) => {
                eprintln!("note: [compat.levels.{name}] replaces the built-in level of that name");
                *slot = level;
            }
            None => out.push(level),
        }
    }
    Ok(out)
}

/// The level named by `--level`, or `default_level` when there is none.
pub fn resolve_level(cfg: &CompatConfig, name: Option<&str>) -> Result<Level> {
    let name = name.or(cfg.default_level.as_deref()).ok_or_else(|| {
        anyhow!(
            "which level? Pass --level NAME, or set default_level under [compat]. \
             `compat --levels` lists them."
        )
    })?;
    levels(cfg)?
        .into_iter()
        .find(|l| l.name == name)
        .ok_or_else(|| anyhow!("no level named {name:?}; `compat --levels` lists them"))
}

/// What a file actually holds, as far as a player cares.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SourceFormat {
    pub codec: Codec,
    pub sample_rate: u32,
    /// Bits per sample for lossless audio; `None` for lossy, where it means nothing.
    pub bit_depth: Option<u8>,
    /// Floating-point PCM, which no level accepts.
    pub float: bool,
}

impl fmt::Display for SourceFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.codec, khz(self.sample_rate))?;
        if let Some(d) = self.bit_depth {
            write!(f, "/{d}{}", if self.float { "f" } else { "" })?;
        }
        Ok(())
    }
}

/// `44.1k`, `48k`, `22.05k`.
fn khz(rate: u32) -> String {
    let s = format!("{:.2}", rate as f64 / 1000.0);
    format!("{}k", s.trim_end_matches('0').trim_end_matches('.'))
}

/// Read a probed file as a [`SourceFormat`].
pub fn classify(info: &AudioInfo, path: &Path) -> Result<SourceFormat> {
    let codec_name = info
        .codec
        .as_deref()
        .ok_or_else(|| anyhow!("no audio stream"))?;
    let sample_rate = info
        .sample_rate
        .and_then(|r| u32::try_from(r).ok())
        .filter(|r| *r > 0)
        .ok_or_else(|| anyhow!("no sample rate"))?;
    let stored_depth = || {
        info.bit_depth
            .and_then(|d| u8::try_from(d).ok())
            .filter(|d| *d > 0)
            .ok_or_else(|| anyhow!("{codec_name} with no bit depth"))
    };
    let (codec, bit_depth, float) = match codec_name {
        "mp3" => (Codec::Mp3, None, false),
        "aac" => (Codec::Aac, None, false),
        "flac" => (Codec::Flac, Some(stored_depth()?), false),
        "alac" => (Codec::Alac, Some(stored_depth()?), false),
        pcm if pcm.starts_with("pcm_") => {
            // ffprobe reports no raw bit depth for PCM, but the codec name has it.
            let (depth, float) = pcm_depth(pcm).ok_or_else(|| anyhow!("unsupported {pcm}"))?;
            // The container decides, as in `AudioInfo::rekordbox_file_type`.
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase();
            let codec = match ext.as_str() {
                "aif" | "aiff" | "aifc" => Codec::Aiff,
                "wav" | "wave" => Codec::Wav,
                other => bail!("PCM audio in a .{other} file"),
            };
            (codec, Some(depth), float)
        }
        other => bail!("codec {other}, which rekordbox cannot play"),
    };
    Ok(SourceFormat {
        codec,
        sample_rate,
        bit_depth,
        float,
    })
}

/// `pcm_s24le` → 24-bit integer, `pcm_f32be` → 32-bit float.
fn pcm_depth(codec: &str) -> Option<(u8, bool)> {
    let rest = codec.strip_prefix("pcm_")?;
    let float = match rest.chars().next()? {
        's' | 'u' => false,
        'f' => true,
        _ => return None,
    };
    let digits: String = rest[1..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    Some((digits.parse().ok()?, float))
}

/// What a conversion produces.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Target {
    /// AIFF, WAV or MP3: the formats this module encodes.
    pub codec: Codec,
    pub sample_rate: u32,
    /// `None` for MP3.
    pub bit_depth: Option<u8>,
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.bit_depth {
            Some(d) => write!(f, "{} {}/{d}", self.codec, khz(self.sample_rate)),
            None => write!(f, "{} {} 320k", self.codec, khz(self.sample_rate)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Decision {
    Keep,
    Convert(Target),
    Skip(SkipReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SkipReason {
    /// Lossy, and re-encoding was not allowed.
    Lossy,
    /// The level has nothing this source can become.
    NoTarget,
    Unreadable,
    /// A Cloud Library Sync row, and `--include-cloud` was not given.
    Cloud,
    /// A cloud row, but rekordbox's settings name no sync folder.
    NoSyncFolder,
    /// A cloud file Dropbox holds only as a placeholder.
    OnlineOnly,
    Missing,
    /// A pending analysis transfer is waiting on this file.
    Queued,
    /// Both candidate filenames are taken.
    TargetTaken,
}

impl SkipReason {
    pub fn describe(self) -> &'static str {
        match self {
            Self::Lossy => {
                "lossy: re-encoding loses quality. Pass --allow-lossy, or `shop` for a lossless copy"
            }
            Self::NoTarget => "the level has no format this could be converted into",
            Self::Unreadable => "unreadable",
            Self::Cloud => "Cloud Library Sync rows; pass --include-cloud to convert them too",
            Self::NoSyncFolder => {
                "Cloud Library Sync rows, but rekordbox's settings name no sync folder"
            }
            Self::OnlineOnly => {
                "only a Dropbox placeholder is on this machine; make it available offline first"
            }
            Self::Missing => "the file is not on this machine",
            Self::Queued => "a queued analysis transfer is waiting on this file",
            Self::TargetTaken => "both filenames for the converted copy are taken",
        }
    }
}

/// Decide what `src` needs to play at `level`.
///
/// Lossless audio becomes AIFF, which carries tags and art where WAV does not.
/// Lossy audio is only re-encoded when `allow_lossy` says so, and then as 320k
/// MP3: a second lossy generation is audibly worse, so it is never the default.
pub fn plan_track(src: &SourceFormat, level: &Level, allow_lossy: bool) -> Decision {
    if level.plays(src) {
        return Decision::Keep;
    }
    if src.codec.is_lossless() {
        let Some(codec) = [Codec::Aiff, Codec::Wav]
            .into_iter()
            .find(|c| level.codecs.contains(c))
        else {
            return Decision::Skip(SkipReason::NoTarget);
        };
        // `classify` gives every lossless source a depth.
        let depth = pick_depth(src.bit_depth.unwrap_or(16), src.float, &level.bit_depths);
        return Decision::Convert(Target {
            codec,
            sample_rate: pick_rate(src.sample_rate, &level.sample_rates),
            bit_depth: Some(depth),
        });
    }
    if !allow_lossy {
        return Decision::Skip(SkipReason::Lossy);
    }
    let rates: Vec<u32> = level
        .sample_rates
        .iter()
        .copied()
        .filter(|r| MP3_RATES.contains(r))
        .collect();
    if !level.codecs.contains(&Codec::Mp3) || rates.is_empty() {
        return Decision::Skip(SkipReason::NoTarget);
    }
    Decision::Convert(Target {
        codec: Codec::Mp3,
        sample_rate: pick_rate(src.sample_rate, &rates),
        bit_depth: None,
    })
}

/// The source rate if allowed. Otherwise the highest allowed rate below it in the
/// same family (88.2k → 44.1k, 96k → 48k), then the highest below it in either
/// family, then the lowest above it.
fn pick_rate(src: u32, allowed: &[u32]) -> u32 {
    if allowed.contains(&src) {
        return src;
    }
    let cd_family = |r: u32| r.is_multiple_of(11025);
    let below = |same_family: bool| {
        allowed
            .iter()
            .copied()
            .filter(|&r| r < src && (!same_family || cd_family(r) == cd_family(src)))
            .max()
    };
    below(true)
        .or_else(|| below(false))
        .or_else(|| allowed.iter().copied().filter(|&r| r > src).min())
        .expect("a level allows at least one rate")
}

/// The source depth if allowed, else the deepest allowed one at or below it, else
/// the shallowest above it. Float always converts: integer PCM of the same width
/// holds everything a mastered track uses.
fn pick_depth(src: u8, float: bool, allowed: &[u8]) -> u8 {
    if !float && allowed.contains(&src) {
        return src;
    }
    allowed
        .iter()
        .copied()
        .filter(|&d| d <= src)
        .max()
        .or_else(|| allowed.iter().copied().filter(|&d| d > src).min())
        .expect("a level allows at least one depth")
}

/// One track to convert.
#[derive(Debug, Clone)]
pub struct Planned {
    pub content_id: String,
    /// `Artist — Title`, for output.
    pub label: String,
    /// `FolderPath` exactly as stored, so the repoint can check it has not moved.
    pub folder_path: String,
    /// Where the source is on this machine. The same as `folder_path` unless the
    /// row is a cloud row.
    pub source_path: PathBuf,
    pub source: SourceFormat,
    pub source_duration: f64,
    pub channels: u32,
    pub target: Target,
    pub target_path: PathBuf,
    /// The `FolderPath` the row gets once it points at `target_path`.
    pub target_folder_path: String,
    /// A Cloud Library Sync row, whose converted file lands in the sync folder.
    pub cloud: bool,
}

impl Planned {
    /// Size of the converted file, for the dry-run's disk estimate.
    pub fn estimated_bytes(&self) -> u64 {
        let bytes_per_sec = match self.target.bit_depth {
            Some(d) => self.target.sample_rate as f64 * self.channels as f64 * f64::from(d) / 8.0,
            None => 320_000.0 / 8.0,
        };
        (bytes_per_sec * self.source_duration) as u64
    }
}

pub struct Skipped {
    pub label: String,
    pub reason: SkipReason,
    pub detail: Option<String>,
}

pub struct Scan {
    pub planned: Vec<Planned>,
    pub skipped: Vec<Skipped>,
    /// Local files that already play at the level.
    pub fits: usize,
    /// Streaming rows, which have no file to convert.
    pub streams: usize,
}

/// A library row whose audio is a file on this machine.
pub struct LibraryFile {
    pub id: String,
    pub label: String,
    pub folder_path: String,
    /// The path a cloud row had before upload, which pending entries still use.
    pub org_path: Option<String>,
    pub on_disk: PathBuf,
    pub cloud: bool,
}

pub struct LibraryFiles {
    pub files: Vec<LibraryFile>,
    pub skipped: Vec<Skipped>,
    /// Streaming rows, which have no file.
    pub streams: usize,
}

/// Every row (or those in `only`) whose file is readable here. Cloud rows
/// resolve through `cloud_root` when `include_cloud` is set.
pub fn library_files(
    db: &MasterDb,
    only: Option<&HashSet<String>>,
    include_cloud: bool,
    cloud_root: Option<&Path>,
) -> Result<LibraryFiles> {
    let mut stmt = db.conn.prepare(
        "SELECT c.ID, c.Title, a.Name, c.FolderPath, c.FileType, c.ServiceID, c.OrgFolderPath
         FROM djmdContent c
         LEFT JOIN djmdArtist a ON a.ID = c.ArtistID
         WHERE c.rb_local_deleted = 0 OR c.rb_local_deleted IS NULL
         ORDER BY c.Title COLLATE NOCASE",
    )?;
    type Raw = (
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<i64>,
        Option<i64>,
        Option<String>,
    );
    let raw: Vec<Raw> = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;

    let cloud_root = cloud_root.filter(|_| include_cloud);
    let mut out = LibraryFiles {
        files: Vec::new(),
        skipped: Vec::new(),
        streams: 0,
    };
    for (id, title, artist, path, file_type, service_id, org_path) in raw {
        if only.is_some_and(|o| !o.contains(&id)) {
            continue;
        }
        let label = format!(
            "{} — {}",
            artist.as_deref().unwrap_or("?"),
            title.as_deref().unwrap_or("?")
        );
        let skip = |reason| Skipped {
            label: label.clone(),
            reason,
            detail: None,
        };
        let origin = format::origin(file_type, path.as_deref(), service_id);
        let Some(folder_path) = path.filter(|_| origin != Origin::Stream) else {
            out.streams += 1;
            continue;
        };
        let cloud = origin == Origin::Cloud;
        let on_disk = match (cloud, cloud_root) {
            (false, _) => PathBuf::from(&folder_path),
            (true, Some(root)) => root.join(folder_path.trim_start_matches('/')),
            (true, None) if include_cloud => {
                out.skipped.push(skip(SkipReason::NoSyncFolder));
                continue;
            }
            (true, None) => {
                out.skipped.push(skip(SkipReason::Cloud));
                continue;
            }
        };
        if !on_disk.exists() {
            out.skipped.push(skip(SkipReason::Missing));
        } else if crate::presence::online_only(&on_disk) {
            out.skipped.push(skip(SkipReason::OnlineOnly));
        } else {
            out.files.push(LibraryFile {
                id,
                label,
                folder_path,
                org_path,
                on_disk,
                cloud,
            });
        }
    }
    Ok(out)
}

pub struct ScanOpts<'a> {
    /// Restrict to these track IDs.
    pub only: Option<&'a HashSet<String>>,
    pub allow_lossy: bool,
    /// Convert Cloud Library Sync rows too, inside the sync folder.
    pub include_cloud: bool,
    /// [`crate::paths::cloud_sync_root`], when `include_cloud` is set.
    pub cloud_root: Option<&'a Path>,
    /// Files with a pending analysis transfer; converting one would leave that
    /// transfer pointing at a row that has moved on.
    pub queued: &'a HashSet<String>,
    pub jobs: usize,
}

/// Probe every local track (or those in `only`) and decide what each needs.
pub fn scan(
    db: &MasterDb,
    level: &Level,
    opts: &ScanOpts,
    mut progress: impl FnMut(usize, usize),
) -> Result<Scan> {
    let lib = library_files(db, opts.only, opts.include_cloud, opts.cloud_root)?;
    let mut out = Scan {
        planned: Vec::new(),
        skipped: lib.skipped,
        fits: 0,
        streams: lib.streams,
    };
    let mut to_probe: Vec<LibraryFile> = Vec::new();
    for row in lib.files {
        // A cloud row's FolderPath was rewritten on upload; a pending entry
        // still knows it by the path it was downloaded to.
        let is_queued = opts.queued.contains(&row.folder_path)
            || row
                .org_path
                .as_ref()
                .is_some_and(|p| opts.queued.contains(p));
        if is_queued {
            out.skipped.push(Skipped {
                label: row.label,
                reason: SkipReason::Queued,
                detail: None,
            });
        } else {
            to_probe.push(row);
        }
    }

    let probed = par_map(
        &to_probe,
        opts.jobs,
        |row| audio::probe(&row.on_disk),
        &mut progress,
    );

    // Two sources in one folder can want the same converted name.
    let mut claimed: HashSet<String> = HashSet::new();
    for (row, info) in to_probe.into_iter().zip(probed) {
        let path = row.on_disk.clone();
        let unreadable = |e: anyhow::Error| Skipped {
            label: row.label.clone(),
            reason: SkipReason::Unreadable,
            detail: Some(format!("{e:#}")),
        };
        let info = match info {
            Ok(i) => i,
            Err(e) => {
                out.skipped.push(unreadable(e));
                continue;
            }
        };
        let source = match classify(&info, &path) {
            Ok(s) => s,
            Err(e) => {
                out.skipped.push(unreadable(e));
                continue;
            }
        };
        let target = match plan_track(&source, level, opts.allow_lossy) {
            Decision::Keep => {
                out.fits += 1;
                continue;
            }
            Decision::Skip(reason) => {
                out.skipped.push(Skipped {
                    label: row.label,
                    reason,
                    detail: Some(source.to_string()),
                });
                continue;
            }
            Decision::Convert(t) => t,
        };
        let Some((target_path, target_folder_path)) = choose_target_path(
            db,
            &path,
            &row.folder_path,
            target.codec,
            &level.name,
            &mut claimed,
        )?
        else {
            out.skipped.push(Skipped {
                label: row.label,
                reason: SkipReason::TargetTaken,
                detail: None,
            });
            continue;
        };
        out.planned.push(Planned {
            content_id: row.id,
            label: row.label,
            folder_path: row.folder_path,
            source_path: path,
            source,
            source_duration: info.duration_secs,
            channels: info
                .channels
                .and_then(|c| u32::try_from(c).ok())
                .unwrap_or(2),
            target,
            target_path,
            target_folder_path,
            cloud: row.cloud,
        });
    }
    Ok(out)
}

/// `<stem>.<ext>` beside the source, or `<stem> [<level>].<ext>` when that is the
/// source itself or already taken. Never a file that exists or a path a row
/// already references.
///
/// Returns the path on disk and the `FolderPath` that names it, which differ for
/// a cloud row: `folder_path` is then relative to the sync folder.
fn choose_target_path(
    db: &MasterDb,
    source: &Path,
    folder_path: &str,
    codec: Codec,
    level: &str,
    claimed: &mut HashSet<String>,
) -> Result<Option<(PathBuf, String)>> {
    let dir = source.parent().unwrap_or(Path::new("."));
    let stem = source
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .ok_or_else(|| anyhow!("{} has no filename", source.display()))?;
    let row_dir = folder_path.rsplit_once('/').map_or("", |(d, _)| d);
    let ext = codec.extension();
    for name in [format!("{stem}.{ext}"), format!("{stem} [{level}].{ext}")] {
        let p = dir.join(&name);
        let row_path = format!("{row_dir}/{name}");
        // Lowercased: the default macOS filesystem ignores case.
        let key = p.to_string_lossy().to_lowercase();
        if p.exists()
            || claimed.contains(&key)
            || crate::import::existing_row_for_path(db, Path::new(&row_path))?.is_some()
        {
            continue;
        }
        claimed.insert(key);
        return Ok(Some((p, row_path)));
    }
    Ok(None)
}

/// Map `items` on `jobs` threads, keeping their order.
pub(crate) fn par_map<T: Sync, R: Send>(
    items: &[T],
    jobs: usize,
    f: impl Fn(&T) -> R + Sync,
    progress: &mut impl FnMut(usize, usize),
) -> Vec<R> {
    let next = AtomicUsize::new(0);
    let mut out: Vec<Option<R>> = items.iter().map(|_| None).collect();
    std::thread::scope(|s| {
        let (tx, rx) = mpsc::channel();
        for _ in 0..jobs.clamp(1, items.len().max(1)) {
            let tx = tx.clone();
            let (next, f) = (&next, &f);
            s.spawn(move || {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= items.len() || tx.send((i, f(&items[i]))).is_err() {
                        break;
                    }
                }
            });
        }
        drop(tx);
        for (done, (i, r)) in rx.into_iter().enumerate() {
            out[i] = Some(r);
            progress(done + 1, items.len());
        }
    });
    out.into_iter()
        .map(|r| r.expect("every item is mapped"))
        .collect()
}

/// Half the cores: ffmpeg is the bottleneck and the machine stays usable.
pub fn default_jobs() -> usize {
    std::thread::available_parallelism()
        .map(|n| (n.get() / 2).max(1))
        .unwrap_or(1)
}

/// Check the tools a conversion needs before planning anything.
pub fn preflight(allow_lossy: bool) -> Result<()> {
    for tool in ["ffmpeg", "ffprobe"] {
        if !proc::tool_available(tool, "-version") {
            bail!("{tool} not found; `compat` needs it to read and convert audio");
        }
    }
    if allow_lossy {
        let mut cmd = proc::capture("ffmpeg");
        cmd.args(["-v", "error", "-hide_banner", "-h", "encoder=libmp3lame"]);
        let out = proc::run_with_deadline(cmd, Instant::now() + Duration::from_secs(30))?;
        if !String::from_utf8_lossy(&out.stdout).contains("libmp3lame") {
            bail!("this ffmpeg was built without libmp3lame, so --allow-lossy cannot encode MP3");
        }
    }
    Ok(())
}

/// The measured offset between a conversion and its source.
#[derive(Debug, Clone, Copy)]
pub struct Alignment {
    pub lag_ms: f64,
    pub correlation: f64,
}

pub struct Converted {
    pub info: AudioInfo,
    pub format: SourceFormat,
    pub alignment: Alignment,
}

/// Encode, verify, and move the result to `plan.target_path`.
///
/// Nothing lands under the real name unless it decodes to the planned format, the
/// same length, and the same timeline as the source.
pub fn convert(plan: &Planned) -> Result<Converted> {
    let staged = staged_sibling(&plan.target_path)?;
    let verified = encode(plan, &staged).and_then(|()| verify(plan, &staged));
    let converted = match verified {
        Ok(c) => c,
        Err(e) => {
            let _ = std::fs::remove_file(&staged);
            return Err(e);
        }
    };
    if plan.target_path.exists() {
        let _ = std::fs::remove_file(&staged);
        bail!(
            "{} appeared while converting; leaving it alone",
            plan.target_path.display()
        );
    }
    std::fs::rename(&staged, &plan.target_path).inspect_err(|_| {
        let _ = std::fs::remove_file(&staged);
    })?;
    Ok(converted)
}

/// A hidden sibling that keeps the extension ffmpeg picks a muxer from.
fn staged_sibling(target: &Path) -> Result<PathBuf> {
    let stem = target
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow!("{} has no filename", target.display()))?;
    let ext = target
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default();
    // Capped so the prefix still fits when the stem is at the filesystem limit.
    let stem: String = stem.chars().take(120).collect();
    Ok(target
        .parent()
        .unwrap_or(Path::new("."))
        .join(format!(".rr-conv-{stem}.{ext}")))
}

fn encode(plan: &Planned, out: &Path) -> Result<()> {
    let (src, target) = (&plan.source, &plan.target);
    let mut cmd = proc::capture("ffmpeg");
    cmd.args(["-v", "error", "-nostdin", "-y", "-i"])
        .arg(&plan.source_path)
        .args(["-map", "0:a:0", "-map_metadata", "0"]);
    // WAV's muxer refuses pictures; see `artwork::container_holds_art`.
    if target.codec != Codec::Wav {
        cmd.args(["-map", "0:v:0?"]);
    }

    let mut resample: Vec<String> = Vec::new();
    if target.sample_rate != src.sample_rate {
        resample.push(format!("osr={}", target.sample_rate));
    }
    match target.bit_depth {
        Some(16) => {
            resample.push("osf=s16".into());
            if src.float || src.bit_depth.is_some_and(|d| d > 16) {
                resample.push("dither_method=triangular_hp".into());
            }
        }
        // ffmpeg's 24-bit PCM encoders take 32-bit samples.
        Some(_) => resample.push("osf=s32".into()),
        None => {}
    }
    if !resample.is_empty() {
        cmd.arg("-af")
            .arg(format!("aresample={}", resample.join(":")));
    }

    // Pictures as `artwork::embed` writes them, so players show the cover.
    let picture = [
        "-c:v",
        "copy",
        "-metadata:s:v",
        "title=Album cover",
        "-metadata:s:v",
        "comment=Cover (front)",
    ];
    match (target.codec, target.bit_depth) {
        (Codec::Aiff, Some(d)) => {
            cmd.args(["-c:a", &format!("pcm_s{d}be"), "-write_id3v2", "1"])
                .args(picture);
        }
        (Codec::Wav, Some(d)) => {
            cmd.args(["-c:a", &format!("pcm_s{d}le")]);
        }
        (Codec::Mp3, None) => {
            cmd.args(["-c:a", "libmp3lame", "-b:a", "320k", "-id3v2_version", "3"])
                .args(picture);
        }
        _ => bail!("cannot encode {target}"),
    }
    cmd.arg(out);

    let res = proc::run_with_deadline(cmd, Instant::now() + ENCODE_TIMEOUT)?;
    if !res.status.success() || std::fs::metadata(out).map(|m| m.len()).unwrap_or(0) == 0 {
        bail!(
            "ffmpeg failed on {}: {}",
            plan.source_path.display(),
            proc::stderr_tail(&res.stderr)
        );
    }
    Ok(())
}

fn verify(plan: &Planned, out: &Path) -> Result<Converted> {
    let info = audio::probe(out)?;
    let format = classify(&info, out)?;
    let target = &plan.target;
    if format.codec != target.codec
        || format.sample_rate != target.sample_rate
        || (target.bit_depth.is_some() && format.bit_depth != target.bit_depth)
    {
        bail!("came out as {format}, not {target}");
    }
    let drift = (info.duration_secs - plan.source_duration).abs();
    if drift > DURATION_TOLERANCE_SECS {
        bail!(
            "came out {:.0} ms {} than the source",
            drift * 1000.0,
            if info.duration_secs < plan.source_duration {
                "shorter"
            } else {
                "longer"
            }
        );
    }
    let alignment = check_alignment(&plan.source_path, out, plan.source_duration)?;
    Ok(Converted {
        info,
        format,
        alignment,
    })
}

/// Measure the offset between `converted` and `source` and fail unless it is
/// within [`ALIGN_TOLERANCE_MS`].
pub fn check_alignment(source: &Path, converted: &Path, duration: f64) -> Result<Alignment> {
    let window = ALIGN_WINDOW_SECS.min(duration);
    // Thirty seconds in, then the middle in case that was a breakdown.
    let starts: Vec<f64> = if duration >= ALIGN_START_SECS + window {
        vec![ALIGN_START_SECS, (duration - window) / 2.0]
    } else {
        vec![0.0]
    };
    let max_lag = (ALIGN_SEARCH_MS / 1000.0 * ALIGN_RATE as f64) as usize;

    for start in starts {
        let a = decode_window(source, start, window)?;
        let b = decode_window(converted, start, window)?;
        let rms =
            (a.iter().map(|&x| f64::from(x).powi(2)).sum::<f64>() / a.len().max(1) as f64).sqrt();
        if rms < SILENCE_RMS {
            continue;
        }
        let Some((lag, correlation)) = estimate_lag(&a, &b, max_lag) else {
            continue;
        };
        let lag_ms = lag as f64 * 1000.0 / ALIGN_RATE as f64;
        if correlation < MIN_CORRELATION {
            bail!("the converted audio does not match the source (correlation {correlation:.2})");
        }
        if lag_ms.abs() > ALIGN_TOLERANCE_MS {
            bail!("the converted audio is shifted {lag_ms:+.2} ms from the source");
        }
        return Ok(Alignment {
            lag_ms,
            correlation,
        });
    }
    bail!("the source is silent or too short where checked, so alignment cannot be verified")
}

/// Mono samples at [`ALIGN_RATE`] from `start` for `secs`.
///
/// Seeks as an output option, which decodes from the top and trims to the exact
/// sample instead of landing on whatever frame an input seek finds.
fn decode_window(path: &Path, start: f64, secs: f64) -> Result<Vec<f32>> {
    let mut cmd = proc::capture("ffmpeg");
    cmd.args(["-nostdin", "-v", "error", "-i"])
        .arg(path)
        .args(["-map", "0:a:0", "-ss", &format!("{start:.3}")])
        .args(["-t", &format!("{secs:.3}")])
        .args(["-f", "s16le", "-acodec", "pcm_s16le", "-ac", "1"])
        .args(["-ar", &ALIGN_RATE.to_string(), "-"]);
    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow!("could not run ffmpeg: {e}"))?;
    // Read on another thread: the output outgrows the pipe buffer, so waiting
    // first would deadlock.
    let mut stdout = child.stdout.take().expect("capture pipes stdout");
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        stdout.read_to_end(&mut buf).map(|_| buf)
    });
    let status = proc::wait_until(&mut child, Instant::now() + DECODE_TIMEOUT)?;
    let bytes = reader
        .join()
        .map_err(|_| anyhow!("decoder reader panicked"))??;
    match status {
        None => bail!("decoding {} timed out", path.display()),
        Some(s) if !s.success() => {
            let mut err = Vec::new();
            if let Some(mut e) = child.stderr.take() {
                let _ = e.read_to_end(&mut err);
            }
            bail!("decoding {}: {}", path.display(), proc::stderr_tail(&err))
        }
        Some(_) => {}
    }
    Ok(bytes
        .chunks_exact(2)
        .map(|b| f32::from(i16::from_le_bytes([b[0], b[1]])) / 32768.0)
        .collect())
}

/// The shift of `other` against `reference` with the highest normalised
/// correlation, within `max_lag` samples either way. Positive means `other` is
/// late. `None` when there is too little audio or nothing but silence.
pub(crate) fn estimate_lag(
    reference: &[f32],
    other: &[f32],
    max_lag: usize,
) -> Option<(isize, f64)> {
    let n = reference.len().min(other.len());
    if n <= 2 * max_lag {
        return None;
    }
    // Only the middle of the reference, so every lag compares the same samples.
    let core = &reference[max_lag..n - max_lag];
    let core_energy: f64 = core.iter().map(|&x| f64::from(x).powi(2)).sum();
    if core_energy == 0.0 {
        return None;
    }
    let mut energy = Vec::with_capacity(n + 1);
    energy.push(0.0f64);
    for &y in &other[..n] {
        energy.push(energy.last().copied().unwrap_or(0.0) + f64::from(y).powi(2));
    }

    let mut best: Option<(isize, f64)> = None;
    for lag in -(max_lag as isize)..=(max_lag as isize) {
        let lo = (max_lag as isize + lag) as usize;
        let hi = lo + core.len();
        let other_energy = energy[hi] - energy[lo];
        if other_energy == 0.0 {
            continue;
        }
        let dot: f64 = core
            .iter()
            .zip(&other[lo..hi])
            .map(|(&a, &b)| f64::from(a) * f64::from(b))
            .sum();
        let corr = dot / (core_energy * other_energy).sqrt();
        if best.is_none_or(|(_, c)| corr > c) {
            best = Some((lag, corr));
        }
    }
    best
}

/// Convert every plan on `jobs` threads, handing each result to `done` on the
/// calling thread as it lands.
///
/// An error from `done` stops the run: nothing new starts, and files that finish
/// afterwards are deleted, since no row will ever point at them.
pub fn convert_all(
    plans: &[Planned],
    jobs: usize,
    mut done: impl FnMut(&Planned, Result<Converted>) -> Result<()>,
) -> Result<()> {
    let next = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let mut failure: Option<anyhow::Error> = None;
    std::thread::scope(|s| {
        let (tx, rx) = mpsc::channel();
        for _ in 0..jobs.clamp(1, plans.len().max(1)) {
            let tx = tx.clone();
            let (next, stop) = (&next, &stop);
            s.spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= plans.len() || tx.send((i, convert(&plans[i]))).is_err() {
                        break;
                    }
                }
            });
        }
        drop(tx);
        for (i, result) in rx {
            let plan = &plans[i];
            if failure.is_some() {
                if result.is_ok() {
                    let _ = std::fs::remove_file(&plan.target_path);
                }
                continue;
            }
            if let Err(e) = done(plan, result) {
                stop.store(true, Ordering::Relaxed);
                failure = Some(e);
            }
        }
    });
    match failure {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// The `djmdContent` columns that describe the file a row points at.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileColumns {
    pub folder_path: String,
    pub file_name: Option<String>,
    pub file_type: Option<i64>,
    pub file_size: Option<i64>,
    pub sample_rate: Option<i64>,
    pub bit_depth: Option<i64>,
    pub bit_rate: Option<i64>,
    /// Sync's ID for the file the row points at. Cleared on a repoint, since it
    /// names the old file; 347 cloud rows already go without one.
    pub rb_file_id: Option<String>,
}

impl FileColumns {
    /// The values `import` would write for `on_disk`, under the row path
    /// `folder_path`, with the bit depth `verify` read off the codec, since
    /// ffprobe reports none for PCM.
    fn for_file(
        folder_path: &str,
        on_disk: &Path,
        info: &AudioInfo,
        format: &SourceFormat,
    ) -> Result<Self> {
        Ok(Self {
            folder_path: folder_path.to_string(),
            file_name: on_disk
                .file_name()
                .map(|n| n.to_string_lossy().into_owned()),
            file_type: Some(
                info.rekordbox_file_type(on_disk)
                    .ok_or_else(|| anyhow!("no rekordbox FileType for {}", on_disk.display()))?,
            ),
            file_size: Some(info.file_size as i64),
            sample_rate: info.sample_rate,
            bit_depth: format.bit_depth.map(i64::from),
            bit_rate: info.bit_rate.map(|b| b / 1000),
            rb_file_id: None,
        })
    }
}

fn read_columns(db: &MasterDb, content_id: &str) -> Result<FileColumns> {
    db.conn
        .query_row(
            "SELECT FolderPath, FileNameL, FileType, FileSize, SampleRate, BitDepth, BitRate,
                    rb_file_id
             FROM djmdContent WHERE ID = ?1",
            params![content_id],
            |r| {
                Ok(FileColumns {
                    folder_path: r.get(0)?,
                    file_name: r.get(1)?,
                    file_type: r.get(2)?,
                    file_size: r.get(3)?,
                    sample_rate: r.get(4)?,
                    bit_depth: r.get(5)?,
                    bit_rate: r.get(6)?,
                    rb_file_id: r.get(7)?,
                })
            },
        )
        .with_context(|| format!("reading track {content_id}"))
}

/// Set a row's file columns, provided it still points at `expect_path`. One USN,
/// so Cloud Library Sync sees it as an edit.
fn write_columns(
    db: &MasterDb,
    content_id: &str,
    cols: &FileColumns,
    expect_path: &str,
) -> Result<()> {
    let usn = db.read_local_usn()? + 1;
    let tx = db.conn.unchecked_transaction()?;
    let n = tx.execute(
        "UPDATE djmdContent
         SET FolderPath = ?3, FileNameL = ?4, FileType = ?5, FileSize = ?6,
             SampleRate = ?7, BitDepth = ?8, BitRate = ?9, rb_file_id = ?10,
             rb_local_synced = 0, rb_local_usn = ?11, updated_at = ?12
         WHERE ID = ?1 AND FolderPath = ?2",
        params![
            content_id,
            expect_path,
            cols.folder_path,
            cols.file_name,
            cols.file_type,
            cols.file_size,
            cols.sample_rate,
            cols.bit_depth,
            cols.bit_rate,
            cols.rb_file_id,
            usn,
            now_db_string(),
        ],
    )?;
    if n != 1 {
        bail!("track {content_id} no longer points at {expect_path}; leaving it alone");
    }
    db.write_local_usn(usn)?;
    tx.commit()?;
    Ok(())
}

/// What a repoint changed, written beside the backup so [`undo`] can reverse it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConvertNote {
    pub content_id: String,
    pub level: String,
    pub old: FileColumns,
    pub new: FileColumns,
    /// Where the original is on this machine; for a cloud row `old.folder_path`
    /// is relative to the sync folder.
    pub original_path: String,
    /// The original's size at conversion time. The row's `FileSize` can be stale,
    /// so it is no proof the original is unchanged.
    pub original_size: u64,
    /// Where the converted file is on this machine.
    pub converted_path: String,
    pub converted_at: String,
    pub backup: String,
}

impl ConvertNote {
    fn path_beside(backup: &Path, content_id: &str) -> PathBuf {
        let mut name = backup.file_name().unwrap_or_default().to_os_string();
        name.push(format!(".{content_id}.converted.json"));
        backup.with_file_name(name)
    }
}

/// Point `plan`'s row at its converted file. Writes the undo note first, so a
/// committed repoint always has one.
pub fn repoint(
    db: &MasterDb,
    plan: &Planned,
    converted: &Converted,
    level: &str,
    backup: &Path,
) -> Result<PathBuf> {
    let old = read_columns(db, &plan.content_id)?;
    let new = FileColumns::for_file(
        &plan.target_folder_path,
        &plan.target_path,
        &converted.info,
        &converted.format,
    )?;
    let note = ConvertNote {
        content_id: plan.content_id.clone(),
        level: level.to_string(),
        old,
        new,
        original_path: plan.source_path.to_string_lossy().into_owned(),
        original_size: std::fs::metadata(&plan.source_path)?.len(),
        converted_path: plan.target_path.to_string_lossy().into_owned(),
        converted_at: now_db_string(),
        backup: backup.to_string_lossy().into_owned(),
    };
    let note_path = ConvertNote::path_beside(backup, &plan.content_id);
    std::fs::write(&note_path, serde_json::to_vec_pretty(&note)?)
        .with_context(|| format!("writing {}", note_path.display()))?;
    write_columns(db, &plan.content_id, &note.new, &plan.folder_path)?;
    Ok(note_path)
}

/// The most recent conversion note for a track.
pub fn find_note(backup_dir: &Path, content_id: &str) -> Result<(PathBuf, ConvertNote)> {
    let suffix = format!(".{content_id}.converted.json");
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(backup_dir)
        .with_context(|| format!("reading {}", backup_dir.display()))?
    {
        let entry = entry?;
        if !entry.file_name().to_string_lossy().ends_with(&suffix) {
            continue;
        }
        let modified = entry.metadata()?.modified()?;
        if newest.as_ref().is_none_or(|(t, _)| modified > *t) {
            newest = Some((modified, entry.path()));
        }
    }
    let (_, path) = newest.ok_or_else(|| anyhow!("no conversion of track {content_id} to undo"))?;
    let note = serde_json::from_slice(&std::fs::read(&path)?)
        .with_context(|| format!("reading {}", path.display()))?;
    Ok((path, note))
}

/// Point the row back at its original file.
///
/// Refuses when the row has moved on since the conversion, or when the original
/// is gone or has changed, since restoring the old columns would then describe a
/// file that is not there.
pub fn undo(db: &MasterDb, note: &ConvertNote) -> Result<()> {
    let current = read_columns(db, &note.content_id)?;
    if current.folder_path != note.new.folder_path {
        bail!(
            "track {} points at {}, not the converted file; already undone, or changed since",
            note.content_id,
            current.folder_path
        );
    }
    let original = Path::new(&note.original_path);
    let size = std::fs::metadata(original)
        .with_context(|| format!("the original {} is gone", original.display()))?
        .len();
    if size != note.original_size {
        bail!(
            "the original {} has changed since it was converted ({} bytes, was {})",
            original.display(),
            size,
            note.original_size
        );
    }
    write_columns(db, &note.content_id, &note.old, &note.new.folder_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(codec: Codec, rate: u32, depth: Option<u8>) -> SourceFormat {
        SourceFormat {
            codec,
            sample_rate: rate,
            bit_depth: depth,
            float: false,
        }
    }

    fn level(name: &str) -> Level {
        builtin_levels()
            .into_iter()
            .find(|l| l.name == name)
            .unwrap()
    }

    /// No built-in level is 16-bit only; every player checked takes 24.
    fn sixteen_bit() -> Level {
        Level {
            bit_depths: vec![16],
            ..level("legacy")
        }
    }

    fn convert_to(decision: Decision) -> Target {
        match decision {
            Decision::Convert(t) => t,
            other => panic!("expected a conversion, got {other:?}"),
        }
    }

    fn aiff(rate: u32, depth: u8) -> Target {
        Target {
            codec: Codec::Aiff,
            sample_rate: rate,
            bit_depth: Some(depth),
        }
    }

    #[test]
    fn what_a_level_plays_is_kept() {
        let legacy = level("legacy");
        for keep in [
            src(Codec::Aiff, 44100, Some(16)),
            src(Codec::Wav, 48000, Some(24)),
            src(Codec::Mp3, 44100, None),
            src(Codec::Aac, 48000, None),
        ] {
            assert_eq!(plan_track(&keep, &legacy, false), Decision::Keep, "{keep}");
        }
        let nxs2 = level("nxs2");
        assert_eq!(
            plan_track(&src(Codec::Flac, 96000, Some(24)), &nxs2, false),
            Decision::Keep
        );
    }

    /// What `src` converts to at `level`.
    fn target(src: SourceFormat, level: &Level, allow_lossy: bool) -> Target {
        convert_to(plan_track(&src, level, allow_lossy))
    }

    #[test]
    fn lossless_the_player_cannot_read_becomes_aiff_at_the_same_quality() {
        let legacy = level("legacy");
        let flac = src(Codec::Flac, 44100, Some(16));
        assert_eq!(target(flac, &legacy, false), aiff(44100, 16));
        let alac = src(Codec::Alac, 48000, Some(24));
        assert_eq!(target(alac, &legacy, false), aiff(48000, 24));
    }

    #[test]
    fn a_high_rate_drops_to_its_own_family() {
        let legacy = level("legacy");
        let flac = src(Codec::Flac, 96000, Some(24));
        assert_eq!(target(flac, &legacy, false), aiff(48000, 24));
        let aiff_88 = src(Codec::Aiff, 88200, Some(24));
        assert_eq!(target(aiff_88, &legacy, false), aiff(44100, 24));
        assert_eq!(pick_rate(176_400, &[44100, 48000]), 44100);
        assert_eq!(pick_rate(192_000, &[44100, 48000]), 48000);
        // Nothing below in its family: the highest below in the other one.
        assert_eq!(pick_rate(96000, &[44100]), 44100);
        // Nothing below at all: the lowest above.
        assert_eq!(pick_rate(32000, &[44100, 48000]), 44100);
    }

    #[test]
    fn float_and_32_bit_pcm_come_down_to_the_deepest_allowed() {
        let legacy = level("legacy");
        let float = SourceFormat {
            float: true,
            ..src(Codec::Wav, 44100, Some(32))
        };
        assert_eq!(target(float, &legacy, false), aiff(44100, 24));
        let int32 = src(Codec::Wav, 44100, Some(32));
        assert_eq!(target(int32, &legacy, false), aiff(44100, 24));
        // 16-bit only: down to 16.
        let deep = src(Codec::Aiff, 44100, Some(24));
        assert_eq!(target(deep, &sixteen_bit(), false), aiff(44100, 16));
        // Below every allowed depth: up to the shallowest.
        assert_eq!(pick_depth(8, false, &[16, 24]), 16);
    }

    #[test]
    fn lossy_that_does_not_fit_is_skipped_unless_allowed() {
        let mut aac_free = level("legacy");
        aac_free.codecs.retain(|c| *c != Codec::Aac);
        let aac = src(Codec::Aac, 44100, None);
        assert_eq!(
            plan_track(&aac, &aac_free, false),
            Decision::Skip(SkipReason::Lossy)
        );
        assert_eq!(
            target(aac, &aac_free, true),
            Target {
                codec: Codec::Mp3,
                sample_rate: 44100,
                bit_depth: None
            }
        );
        // A 22.05k MP3 plays nowhere old; re-encoding lifts it to 44.1k.
        let low = src(Codec::Mp3, 22050, None);
        let legacy = level("legacy");
        assert_eq!(
            plan_track(&low, &legacy, false),
            Decision::Skip(SkipReason::Lossy)
        );
        assert_eq!(target(low, &legacy, true).sample_rate, 44100);
    }

    #[test]
    fn lossy_above_48k_does_not_play_even_where_pcm_at_96k_does() {
        let hi_aac = src(Codec::Aac, 96000, None);
        let nxs2 = level("nxs2");
        assert_eq!(
            plan_track(&hi_aac, &nxs2, false),
            Decision::Skip(SkipReason::Lossy)
        );
        assert_eq!(target(hi_aac, &nxs2, true).sample_rate, 48000);
    }

    #[test]
    fn flac48_takes_flac_but_not_alac_or_high_rates() {
        let flac48 = level("flac48");
        assert_eq!(
            plan_track(&src(Codec::Flac, 44100, Some(24)), &flac48, false),
            Decision::Keep
        );
        let alac = src(Codec::Alac, 44100, Some(16));
        assert_eq!(target(alac, &flac48, false), aiff(44100, 16));
        let hires = src(Codec::Flac, 96000, Some(24));
        assert_eq!(target(hires, &flac48, false), aiff(48000, 24));
    }

    #[test]
    fn a_level_with_nothing_to_convert_into_says_so() {
        let mut flac_only = level("nxs2");
        flac_only.codecs = vec![Codec::Flac];
        assert_eq!(
            plan_track(&src(Codec::Wav, 44100, Some(16)), &flac_only, false),
            Decision::Skip(SkipReason::NoTarget)
        );
        assert_eq!(
            plan_track(&src(Codec::Aac, 44100, None), &flac_only, true),
            Decision::Skip(SkipReason::NoTarget)
        );
    }

    fn info(codec: &str, rate: i64, depth: Option<i64>) -> AudioInfo {
        AudioInfo {
            duration_secs: 10.0,
            sample_rate: Some(rate),
            bit_depth: depth,
            channels: Some(2),
            bit_rate: None,
            codec: Some(codec.into()),
            file_size: 1,
            tags: Default::default(),
        }
    }

    #[test]
    fn classification_reads_pcm_depth_off_the_codec_and_the_container_off_the_name() {
        let wav = classify(&info("pcm_s24le", 48000, None), Path::new("/a/b.wav")).unwrap();
        assert_eq!(wav, src(Codec::Wav, 48000, Some(24)));
        let aiff = classify(&info("pcm_s16be", 44100, None), Path::new("/a/b.aif")).unwrap();
        assert_eq!(aiff, src(Codec::Aiff, 44100, Some(16)));
        let float = classify(&info("pcm_f32le", 44100, None), Path::new("/a/b.wav")).unwrap();
        assert!(float.float && float.bit_depth == Some(32));
        // ALAC and AAC share a container; only the codec tells them apart.
        let alac = classify(&info("alac", 44100, Some(24)), Path::new("/a/b.m4a")).unwrap();
        assert_eq!(alac.codec, Codec::Alac);
        let aac = classify(&info("aac", 44100, None), Path::new("/a/b.m4a")).unwrap();
        assert_eq!(aac, src(Codec::Aac, 44100, None));
    }

    #[test]
    fn classification_refuses_what_it_cannot_place() {
        assert!(classify(&info("vorbis", 44100, None), Path::new("/a/b.ogg")).is_err());
        assert!(classify(&info("flac", 44100, None), Path::new("/a/b.flac")).is_err());
        assert!(classify(&info("pcm_s16le", 44100, None), Path::new("/a/b.raw")).is_err());
        assert!(classify(&info("pcm_mulaw", 8000, None), Path::new("/a/b.wav")).is_err());
    }

    fn spec(codecs: &[&str], rates: &[u32], depths: &[u8]) -> LevelSpec {
        LevelSpec {
            codecs: codecs.iter().map(|s| s.to_string()).collect(),
            sample_rates: rates.to_vec(),
            bit_depths: depths.to_vec(),
        }
    }

    #[test]
    fn a_configured_level_parses_and_can_replace_a_builtin() {
        let mut cfg = CompatConfig::default();
        cfg.levels.insert(
            "my-xdj".into(),
            spec(&["mp3", "m4a", "aiff"], &[44100], &[16]),
        );
        cfg.levels
            .insert("legacy".into(), spec(&["aiff"], &[44100], &[16]));
        let all = levels(&cfg).unwrap();
        let mine = all.iter().find(|l| l.name == "my-xdj").unwrap();
        assert_eq!(mine.codecs, vec![Codec::Mp3, Codec::Aac, Codec::Aiff]);
        let legacy = all.iter().find(|l| l.name == "legacy").unwrap();
        assert_eq!(legacy.codecs, vec![Codec::Aiff]);
        assert_eq!(all.iter().filter(|l| l.name == "legacy").count(), 1);
    }

    #[test]
    fn a_bad_level_is_an_error_not_a_silent_default() {
        // A typo here would convert the wrong tracks, so nothing is guessed.
        for (name, bad) in [
            ("typo", spec(&["flca"], &[44100], &[16])),
            ("ogg", spec(&["ogg"], &[44100], &[16])),
            ("depth", spec(&["aiff"], &[44100], &[20])),
            ("rate", spec(&["aiff"], &[44], &[16])),
            ("empty", spec(&[], &[44100], &[16])),
            ("bad/name", spec(&["aiff"], &[44100], &[16])),
        ] {
            let mut cfg = CompatConfig::default();
            cfg.levels.insert(name.into(), bad);
            assert!(levels(&cfg).is_err(), "{name} should be refused");
        }
    }

    #[test]
    fn an_unknown_level_key_is_refused() {
        let parsed: Result<CompatConfig, _> =
            toml::from_str("[levels.x]\ncodecs=[\"aiff\"]\nsample_rate=[44100]\nbit_depths=[16]\n");
        assert!(parsed.is_err());
    }

    #[test]
    fn resolving_needs_a_name_from_somewhere() {
        let mut cfg = CompatConfig::default();
        assert!(resolve_level(&cfg, None).is_err());
        assert!(resolve_level(&cfg, Some("nope")).is_err());
        cfg.default_level = Some("nxs2".into());
        assert_eq!(resolve_level(&cfg, None).unwrap().name, "nxs2");
        assert_eq!(resolve_level(&cfg, Some("legacy")).unwrap().name, "legacy");
    }

    #[test]
    fn rates_print_the_way_people_say_them() {
        assert_eq!(khz(44100), "44.1k");
        assert_eq!(khz(48000), "48k");
        assert_eq!(khz(22050), "22.05k");
        assert_eq!(src(Codec::Flac, 96000, Some(24)).to_string(), "FLAC 96k/24");
    }

    /// Deterministic noise: sharp autocorrelation, so only the true lag peaks.
    fn noise(n: usize, seed: u64) -> Vec<f32> {
        let mut x = seed;
        (0..n)
            .map(|_| {
                x = x
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                ((x >> 33) as f32 / (1u64 << 31) as f32) - 0.5
            })
            .collect()
    }

    #[test]
    fn the_lag_estimator_finds_a_shift_to_the_sample() {
        let base = noise(4000, 7);
        for shift in [0isize, 5, -5, 50, -50] {
            // other[i] = base[i - shift]
            let other: Vec<f32> = (0..base.len() as isize)
                .map(|i| {
                    let j = i - shift;
                    if (0..base.len() as isize).contains(&j) {
                        base[j as usize]
                    } else {
                        0.0
                    }
                })
                .collect();
            let (lag, corr) = estimate_lag(&base, &other, 200).unwrap();
            assert_eq!(lag, shift);
            assert!(corr > 0.99, "{corr}");
        }
    }

    #[test]
    fn unrelated_audio_does_not_correlate() {
        let (_, corr) = estimate_lag(&noise(4000, 1), &noise(4000, 2), 200).unwrap();
        assert!(corr < MIN_CORRELATION, "{corr}");
    }

    #[test]
    fn silence_and_short_windows_give_no_answer() {
        assert!(estimate_lag(&[0.0; 1000], &noise(1000, 3), 100).is_none());
        assert!(estimate_lag(&noise(100, 3), &noise(100, 3), 100).is_none());
    }

    /// The columns `scan`, `repoint` and `undo` read and write, plus the USN row.
    fn db() -> MasterDb {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE djmdContent (ID TEXT, Title TEXT, ArtistID TEXT, FolderPath TEXT,
                OrgFolderPath TEXT, FileNameL TEXT, FileType INTEGER, FileSize INTEGER,
                SampleRate INTEGER, BitDepth INTEGER, BitRate INTEGER, ServiceID INTEGER,
                rb_file_id TEXT,
                rb_local_deleted INTEGER, rb_local_synced INTEGER, rb_local_usn INTEGER,
                updated_at TEXT);
             CREATE TABLE djmdArtist (ID TEXT, Name TEXT);
             CREATE TABLE agentRegistry (registry_id TEXT, int_1 INTEGER, updated_at TEXT);
             INSERT INTO agentRegistry VALUES ('localUpdateCount', 100, '');",
        )
        .unwrap();
        MasterDb {
            conn,
            app_dir: PathBuf::from("."),
        }
    }

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rr-compat-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_taken_name_falls_back_to_the_level_suffix_and_then_gives_up() {
        let db = db();
        let dir = scratch();
        let mut claimed = HashSet::new();
        // An AIFF that needs resampling cannot overwrite itself.
        let source = dir.join("a.aiff");
        std::fs::write(&source, b"x").unwrap();
        let row = source.to_string_lossy().into_owned();
        let mut pick = |src: &Path, row: &str| {
            choose_target_path(&db, src, row, Codec::Aiff, "legacy", &mut claimed)
                .unwrap()
                .map(|(p, _)| p)
        };
        assert_eq!(pick(&source, &row), Some(dir.join("a [legacy].aiff")));
        // A second source in the same folder cannot claim the same name.
        assert_eq!(pick(&source, &row), None);

        // A path a row already references is as taken as a file on disk.
        let flac = dir.join("b.flac");
        db.conn
            .execute(
                "INSERT INTO djmdContent (ID, FolderPath) VALUES ('9', ?1)",
                params![dir.join("b.aiff").to_string_lossy()],
            )
            .unwrap();
        let flac_row = flac.to_string_lossy().into_owned();
        assert_eq!(pick(&flac, &flac_row), Some(dir.join("b [legacy].aiff")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_cloud_target_is_named_relative_to_the_sync_folder() {
        let db = db();
        let root = scratch();
        let mut claimed = HashSet::new();
        let source = root.join("contents_1/artist/a.flac");
        // A row already holding the plain name, as rekordbox knows it.
        db.conn
            .execute(
                "INSERT INTO djmdContent (ID, FolderPath) VALUES ('9', '/contents_1/artist/a.aiff')",
                [],
            )
            .unwrap();
        let picked = choose_target_path(
            &db,
            &source,
            "/contents_1/artist/a.flac",
            Codec::Aiff,
            "legacy",
            &mut claimed,
        )
        .unwrap();
        assert_eq!(
            picked,
            Some((
                root.join("contents_1/artist/a [legacy].aiff"),
                "/contents_1/artist/a [legacy].aiff".to_string()
            ))
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    fn columns(path: &str, ft: i64) -> FileColumns {
        FileColumns {
            folder_path: path.into(),
            file_name: Path::new(path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned()),
            file_type: Some(ft),
            file_size: Some(1),
            sample_rate: Some(44100),
            bit_depth: Some(16),
            bit_rate: Some(0),
            rb_file_id: None,
        }
    }

    #[test]
    fn a_write_lands_once_bumps_the_usn_and_refuses_a_moved_row() {
        let db = db();
        db.conn
            .execute(
                "INSERT INTO djmdContent (ID, FolderPath, rb_local_synced) VALUES ('1', '/a.flac', 1)",
                [],
            )
            .unwrap();
        write_columns(&db, "1", &columns("/a.aiff", 12), "/a.flac").unwrap();
        let now = read_columns(&db, "1").unwrap();
        assert_eq!(now, columns("/a.aiff", 12));
        assert_eq!(db.read_local_usn().unwrap(), 101);
        let synced: i64 = db
            .conn
            .query_row("SELECT rb_local_synced FROM djmdContent", [], |r| r.get(0))
            .unwrap();
        assert_eq!(synced, 0, "cloud sync must see the edit");
        // The row no longer points at /a.flac, so a second write is refused.
        assert!(write_columns(&db, "1", &columns("/b.aiff", 12), "/a.flac").is_err());
        assert_eq!(db.read_local_usn().unwrap(), 101);
    }

    #[test]
    fn undo_restores_the_row_only_while_the_original_is_intact() {
        let db = db();
        let dir = scratch();
        let original = dir.join("a.flac");
        std::fs::write(&original, b"flac").unwrap();
        // A cloud row: its FolderPath is not where the file is on disk.
        let old = FileColumns {
            rb_file_id: Some("246205391".into()),
            ..columns("/contents_1/a.flac", 5)
        };
        let new = columns("/contents_1/a.aiff", 12);
        db.conn
            .execute(
                "INSERT INTO djmdContent (ID, FolderPath) VALUES ('1', ?1)",
                params![new.folder_path],
            )
            .unwrap();
        let note = ConvertNote {
            content_id: "1".into(),
            level: "legacy".into(),
            old: old.clone(),
            new,
            original_path: original.to_string_lossy().into_owned(),
            original_size: 4,
            converted_path: dir.join("a.aiff").to_string_lossy().into_owned(),
            converted_at: String::new(),
            backup: String::new(),
        };

        std::fs::write(&original, b"changed").unwrap();
        assert!(undo(&db, &note).is_err(), "a changed original is refused");
        std::fs::write(&original, b"flac").unwrap();
        undo(&db, &note).unwrap();
        // Sync's file ID comes back with the rest.
        assert_eq!(read_columns(&db, "1").unwrap(), old);
        assert!(
            undo(&db, &note).is_err(),
            "a second undo has nothing to undo"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_newest_note_for_a_track_is_the_one_found() {
        let dir = scratch();
        let note = |level: &str| ConvertNote {
            content_id: "7".into(),
            level: level.into(),
            old: columns("/a.flac", 5),
            new: columns("/a.aiff", 12),
            original_path: "/a.flac".into(),
            original_size: 1,
            converted_path: "/a.aiff".into(),
            converted_at: String::new(),
            backup: String::new(),
        };
        for (backup, level) in [("master.db.1.bak", "old"), ("master.db.2.bak", "new")] {
            let path = ConvertNote::path_beside(&dir.join(backup), "7");
            std::fs::write(&path, serde_json::to_vec(&note(level)).unwrap()).unwrap();
            std::thread::sleep(Duration::from_millis(20));
        }
        // Another track's note must not match: "17" ends in "7".
        let other = ConvertNote::path_beside(&dir.join("master.db.3.bak"), "17");
        std::fs::write(&other, serde_json::to_vec(&note("other")).unwrap()).unwrap();
        let (_, found) = find_note(&dir, "7").unwrap();
        assert_eq!(found.level, "new");
        assert!(find_note(&dir, "8").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Pink noise at `rate`/`depth` with a title tag, or `None` without ffmpeg.
    fn noise_file(dir: &Path, name: &str, rate: u32, fmt: &str) -> Option<PathBuf> {
        let path = dir.join(name);
        let mut cmd = proc::capture("ffmpeg");
        cmd.args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg(format!(
                "anoisesrc=d=4:c=pink:r={rate}:seed=42,aformat=channel_layouts=stereo"
            ))
            .args(["-sample_fmt", fmt, "-metadata", "title=Noise Test"])
            .arg(&path);
        let out = proc::run_with_deadline(cmd, Instant::now() + Duration::from_secs(60)).ok()?;
        out.status.success().then_some(path)
    }

    fn plan_for(path: &Path, level: &Level, allow_lossy: bool) -> Planned {
        let info = audio::probe(path).unwrap();
        let source = classify(&info, path).unwrap();
        let target = convert_to(plan_track(&source, level, allow_lossy));
        let target_path = path.with_extension(target.codec.extension());
        Planned {
            content_id: "1".into(),
            label: "test".into(),
            folder_path: path.to_string_lossy().into_owned(),
            source_path: path.to_path_buf(),
            source,
            source_duration: info.duration_secs,
            channels: 2,
            target,
            target_folder_path: target_path.to_string_lossy().into_owned(),
            target_path,
            cloud: false,
        }
    }

    #[test]
    fn a_hi_res_flac_converts_to_an_aligned_tagged_aiff() {
        let dir = scratch();
        let Some(flac) = noise_file(&dir, "hires.flac", 96000, "s32") else {
            return; // no ffmpeg here
        };
        let plan = plan_for(&flac, &level("legacy"), false);
        assert_eq!(plan.target, aiff(48000, 24));
        let done = convert(&plan).unwrap();
        assert!(plan.target_path.exists());
        assert_eq!(done.format, src(Codec::Aiff, 48000, Some(24)));
        assert!(done.alignment.lag_ms.abs() <= ALIGN_TOLERANCE_MS);
        assert_eq!(done.info.tags.title.as_deref(), Some("Noise Test"));
        // Nothing staged is left behind.
        let leftovers = std::fs::read_dir(&dir)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".rr-conv-")
            })
            .count();
        assert_eq!(leftovers, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_cloud_row_converts_inside_the_sync_folder_and_drops_its_file_id() {
        let root = scratch();
        let folder = root.join("contents_1/artist");
        std::fs::create_dir_all(&folder).unwrap();
        let Some(_) = noise_file(&folder, "a.flac", 96000, "s32") else {
            return;
        };
        let db = db();
        db.conn
            .execute_batch(
                "INSERT INTO djmdContent (ID, Title, FolderPath, OrgFolderPath, FileType,
                    ServiceID, rb_file_id)
                 VALUES ('1', 'a', '/contents_1/artist/a.flac', '/incoming/a.flac', 5, 2, '77');",
            )
            .unwrap();
        let queued = HashSet::new();
        let mut opts = ScanOpts {
            only: None,
            allow_lossy: false,
            include_cloud: false,
            cloud_root: Some(&root),
            queued: &queued,
            jobs: 1,
        };
        // Off unless asked for.
        let skipped = scan(&db, &level("legacy"), &opts, |_, _| {}).unwrap();
        assert!(skipped.planned.is_empty());
        assert_eq!(skipped.skipped[0].reason, SkipReason::Cloud);

        opts.include_cloud = true;
        let found = scan(&db, &level("legacy"), &opts, |_, _| {}).unwrap();
        let plan = &found.planned[0];
        assert!(plan.cloud);
        assert_eq!(plan.target_path, folder.join("a.aiff"));
        assert_eq!(plan.target_folder_path, "/contents_1/artist/a.aiff");

        let converted = convert(plan).unwrap();
        let backup = root.join("master.db.bak");
        repoint(&db, plan, &converted, "legacy", &backup).unwrap();
        let now = read_columns(&db, "1").unwrap();
        assert_eq!(now.folder_path, "/contents_1/artist/a.aiff");
        assert_eq!(now.file_type, Some(12));
        assert_eq!(now.rb_file_id, None);

        // A transfer queued against the download path still protects the row.
        let queued: HashSet<String> = ["/incoming/a.flac".to_string()].into();
        db.conn
            .execute(
                "UPDATE djmdContent SET FolderPath = '/contents_1/artist/a.flac' WHERE ID = '1'",
                [],
            )
            .unwrap();
        let _ = std::fs::remove_file(folder.join("a.aiff"));
        opts.queued = &queued;
        let held = scan(&db, &level("legacy"), &opts, |_, _| {}).unwrap();
        assert_eq!(held.skipped[0].reason, SkipReason::Queued);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_dithered_16_bit_conversion_stays_aligned() {
        let dir = scratch();
        let Some(wav) = noise_file(&dir, "deep.wav", 44100, "flt") else {
            return;
        };
        let plan = plan_for(&wav, &sixteen_bit(), false);
        assert_eq!(plan.target, aiff(44100, 16));
        let done = convert(&plan).unwrap();
        assert_eq!(done.format, src(Codec::Aiff, 44100, Some(16)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_mp3_reencode_is_checked_for_encoder_delay() {
        let dir = scratch();
        let Some(m4a) = noise_file(&dir, "lossy.m4a", 44100, "fltp") else {
            return;
        };
        let mut no_aac = level("legacy");
        no_aac.codecs.retain(|c| *c != Codec::Aac);
        let plan = plan_for(&m4a, &no_aac, true);
        assert_eq!(plan.target.codec, Codec::Mp3);
        let done = convert(&plan).unwrap();
        assert!(done.alignment.lag_ms.abs() <= ALIGN_TOLERANCE_MS);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_shifted_copy_fails_the_alignment_check() {
        let dir = scratch();
        let Some(flac) = noise_file(&dir, "a.flac", 44100, "s16") else {
            return;
        };
        let shifted = dir.join("shifted.aiff");
        let mut cmd = proc::capture("ffmpeg");
        cmd.args(["-v", "error", "-y", "-i"])
            .arg(&flac)
            .args(["-af", "adelay=20:all=1"])
            .arg(&shifted);
        assert!(
            proc::run_with_deadline(cmd, Instant::now() + Duration::from_secs(60))
                .unwrap()
                .status
                .success()
        );
        let err = check_alignment(&flac, &shifted, 4.0)
            .unwrap_err()
            .to_string();
        // 20 ms is 220.5 samples at the alignment rate.
        assert!(err.contains("shifted +19.95 ms"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
