//! FLAC metadata, rewritten without touching an audio frame.
//!
//! ffmpeg's FLAC muxer keeps STREAMINFO, tags, pictures and padding and drops
//! every other block, the seek table included, so a player has to scan the file
//! to reach a cue. Editing the blocks directly keeps everything this code does
//! not mean to change.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail, ensure};

const MAGIC: &[u8; 4] = b"fLaC";
const STREAMINFO: u8 = 0;
const PADDING: u8 = 1;
const SEEKTABLE: u8 = 3;
const PICTURE: u8 = 6;
/// Longest body a block header's 24-bit length can describe.
const MAX_BLOCK: usize = (1 << 24) - 1;
/// Padding left after the metadata when a file has to grow, as libFLAC does.
const GROWTH_PADDING: usize = 8192;
/// Seek point spacing, libFLAC's default (`flac -S 10s`).
const SEEK_INTERVAL_SECS: u64 = 10;
const SEEK_POINT_LEN: usize = 18;
const PLACEHOLDER: u64 = u64::MAX;
const FRONT_COVER: u32 = 3;
/// How far past a frame's start its successor is looked for. Above the largest
/// frame the format allows at 8 channels of 32-bit audio.
const FRAME_REACH: usize = 1 << 21;

#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    pub kind: u8,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct Metadata {
    pub blocks: Vec<Block>,
    /// Byte offset of the first audio frame, which is also the room the
    /// metadata may fill without moving any audio.
    pub audio_offset: u64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StreamInfo {
    pub sample_rate: u32,
    pub channels: u8,
    pub bits: u8,
    /// 0 when the encoder did not know.
    pub total_samples: u64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Frame {
    /// First sample in the frame.
    pub sample: u64,
    /// From the first frame's header, as a seek point stores it.
    pub offset: u64,
    pub samples: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Frames {
    Clean(Vec<Frame>),
    /// No valid frame follows the one starting at this sample.
    Damaged {
        at_sample: u64,
    },
}

/// Whether a rewrite fit in the room the old metadata took.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Rewrite {
    SameSize,
    Grown,
}

impl Metadata {
    pub fn stream_info(&self) -> Result<StreamInfo> {
        let b = self
            .blocks
            .first()
            .filter(|b| b.kind == STREAMINFO && b.data.len() == 34)
            .ok_or_else(|| anyhow!("the first metadata block is not STREAMINFO"))?;
        let d = &b.data;
        Ok(StreamInfo {
            sample_rate: (u32::from(d[10]) << 12) | (u32::from(d[11]) << 4) | u32::from(d[12] >> 4),
            channels: ((d[12] >> 1) & 7) + 1,
            bits: (((d[12] & 1) << 4) | (d[13] >> 4)) + 1,
            total_samples: (u64::from(d[13] & 0x0F) << 32)
                | u64::from(u32::from_be_bytes([d[14], d[15], d[16], d[17]])),
        })
    }

    /// True when a seek table holds at least one real point. A table of
    /// placeholders is what an encoder reserves and never filled in.
    pub fn has_seek_points(&self) -> bool {
        self.blocks
            .iter()
            .filter(|b| b.kind == SEEKTABLE)
            .flat_map(|b| b.data.chunks_exact(SEEK_POINT_LEN))
            .any(|p| u64::from_be_bytes(p[..8].try_into().expect("8 bytes")) != PLACEHOLDER)
    }

    /// Bytes the metadata could grow by in place.
    pub fn spare(&self) -> usize {
        let used = MAGIC.len()
            + self
                .blocks
                .iter()
                .filter(|b| b.kind != PADDING)
                .map(|b| 4 + b.data.len())
                .sum::<usize>();
        (self.audio_offset as usize).saturating_sub(used)
    }
}

pub fn read(path: &Path) -> Result<Metadata> {
    let mut r =
        BufReader::new(File::open(path).with_context(|| format!("opening {}", path.display()))?);
    read_from(&mut r).with_context(|| format!("reading FLAC metadata from {}", path.display()))
}

fn read_from(r: &mut impl Read) -> Result<Metadata> {
    let mut magic = [0u8; 4];
    r.read_exact(&mut magic)?;
    ensure!(&magic == MAGIC, "not a FLAC stream (starts {magic:02x?})");
    let mut blocks = Vec::new();
    let mut offset = MAGIC.len() as u64;
    loop {
        let mut h = [0u8; 4];
        r.read_exact(&mut h)?;
        let kind = h[0] & 0x7F;
        ensure!(kind != 127, "invalid metadata block type 127");
        let len = u32::from_be_bytes([0, h[1], h[2], h[3]]) as usize;
        let mut data = vec![0u8; len];
        r.read_exact(&mut data)?;
        offset += 4 + len as u64;
        blocks.push(Block { kind, data });
        if h[0] & 0x80 != 0 {
            break;
        }
    }
    Ok(Metadata {
        blocks,
        audio_offset: offset,
    })
}

/// `blocks` laid out to exactly `room` bytes, the remainder spent on padding.
/// None when they do not fit.
fn layout(blocks: &[Block], room: usize) -> Result<Option<Vec<u8>>> {
    let kept: Vec<&Block> = blocks.iter().filter(|b| b.kind != PADDING).collect();
    if let Some(b) = kept.iter().find(|b| b.data.len() > MAX_BLOCK) {
        bail!(
            "a {}-byte metadata block is over the format's 16 MiB limit",
            b.data.len()
        );
    }
    let used = MAGIC.len() + kept.iter().map(|b| 4 + b.data.len()).sum::<usize>();
    let Some(mut spare) = room.checked_sub(used) else {
        return Ok(None);
    };
    let mut pads = Vec::new();
    while spare > 0 {
        // Every padding block pays for its own 4-byte header.
        if spare < 4 {
            return Ok(None);
        }
        let mut body = (spare - 4).min(MAX_BLOCK);
        if (1..4).contains(&(spare - 4 - body)) {
            body -= 4;
        }
        pads.push(body);
        spare -= 4 + body;
    }

    let mut out = Vec::with_capacity(room);
    out.extend_from_slice(MAGIC);
    let total = kept.len() + pads.len();
    let mut push = |i: usize, kind: u8, body: &[u8]| {
        let last = if i + 1 == total { 0x80 } else { 0 };
        let len = (body.len() as u32).to_be_bytes();
        out.extend_from_slice(&[kind | last, len[1], len[2], len[3]]);
        out.extend_from_slice(body);
    };
    for (i, b) in kept.iter().enumerate() {
        push(i, b.kind, &b.data);
    }
    for (j, &n) in pads.iter().enumerate() {
        push(kept.len() + j, PADDING, &vec![0u8; n]);
    }
    debug_assert_eq!(out.len(), room);
    Ok(Some(out))
}

/// Replace `path`'s metadata with `blocks`, which must start with STREAMINFO.
///
/// When they fit in the room the old metadata took, the file keeps its size and
/// every audio byte its offset. `in_place` then writes over the old metadata;
/// otherwise the whole file is written to a sibling and renamed over it.
/// Growing always goes through a sibling, and `in_place` refuses to grow.
fn rewrite(path: &Path, old: &Metadata, blocks: &[Block], in_place: bool) -> Result<Rewrite> {
    ensure!(
        blocks.first().is_some_and(|b| b.kind == STREAMINFO),
        "STREAMINFO must come first"
    );
    let room = old.audio_offset as usize;
    let (bytes, grew) = match layout(blocks, room)? {
        Some(b) => (b, Rewrite::SameSize),
        None if in_place => bail!(
            "{} has {} bytes of padding, too few to grow the metadata without moving the audio",
            path.display(),
            old.spare()
        ),
        None => {
            let used = MAGIC.len()
                + blocks
                    .iter()
                    .filter(|b| b.kind != PADDING)
                    .map(|b| 4 + b.data.len())
                    .sum::<usize>();
            let b = layout(blocks, used + 4 + GROWTH_PADDING)?.expect("sized to fit");
            (b, Rewrite::Grown)
        }
    };

    if in_place {
        let mut f = OpenOptions::new().write(true).open(path)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
        return Ok(grew);
    }

    let staged = staged_sibling(path)?;
    let result = (|| -> Result<()> {
        let mut src = File::open(path)?;
        src.seek(SeekFrom::Start(old.audio_offset))?;
        let mut out = BufWriter::new(File::create(&staged)?);
        out.write_all(&bytes)?;
        std::io::copy(&mut src, &mut out)?;
        out.into_inner().map_err(|e| e.into_error())?.sync_all()?;
        std::fs::rename(&staged, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    result.with_context(|| format!("rewriting {}", path.display()))?;
    Ok(grew)
}

/// A dotted sibling, out of the way of anything scanning for audio.
fn staged_sibling(path: &Path) -> Result<PathBuf> {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow!("{} has no filename", path.display()))?;
    let stem: String = stem.chars().take(120).collect();
    Ok(path
        .parent()
        .unwrap_or(Path::new("."))
        .join(format!(".rr-flac-{stem}.flac")))
}

/// Every frame in the file, each boundary confirmed by the frame's CRC-16.
pub fn frames(path: &Path, meta: &Metadata) -> Result<Frames> {
    let info = meta.stream_info()?;
    let mut f = File::open(path)?;
    f.seek(SeekFrom::Start(meta.audio_offset))?;
    let mut data = Vec::new();
    f.read_to_end(&mut data)?;
    Ok(walk(&data, &info))
}

fn walk(data: &[u8], info: &StreamInfo) -> Frames {
    let Some(mut head) = parse_header(data, info).filter(|h| h.number == 0) else {
        return Frames::Damaged { at_sample: 0 };
    };
    let variable = head.variable;
    let (mut pos, mut sample, mut index) = (0usize, 0u64, 0u64);
    let mut out = Vec::new();
    loop {
        out.push(Frame {
            sample,
            offset: pos as u64,
            samples: head.samples,
        });
        let want = if variable {
            sample + u64::from(head.samples)
        } else {
            index + 1
        };
        // A frame ends where the CRC-16 running from its first byte comes to
        // zero and a header numbered as its successor begins.
        let limit = data.len().min(pos + FRAME_REACH);
        let mut crc = 0u16;
        let mut ends_clean = false;
        let mut next = None;
        for q in pos..limit {
            crc = crc16_step(crc, data[q]);
            let end = q + 1;
            if crc != 0 || end < pos + head.len + 2 {
                continue;
            }
            // A CRC comes to zero by chance every 64 KiB or so, so only an end
            // followed by nothing but a tag counts as the last frame's.
            ends_clean |= is_trailer(&data[end..]);
            if let Some(h) = data
                .get(end..)
                .and_then(|d| parse_header(d, info))
                .filter(|h| h.number == want && h.variable == variable)
            {
                next = Some((end, h));
                break;
            }
        }
        sample += u64::from(head.samples);
        index += 1;
        match next {
            Some((end, h)) => {
                pos = end;
                head = h;
            }
            // The last frame, possibly followed by a trailing tag.
            None if limit == data.len() && ends_clean => break,
            None => {
                return Frames::Damaged {
                    at_sample: sample - u64::from(head.samples),
                };
            }
        }
    }
    if info.total_samples != 0 && sample != info.total_samples {
        return Frames::Damaged { at_sample: sample };
    }
    Frames::Clean(out)
}

/// What may follow the last frame: nothing, a tag, or zero fill.
fn is_trailer(rest: &[u8]) -> bool {
    rest.is_empty()
        || rest.starts_with(b"TAG")
        || rest.starts_with(b"ID3")
        || rest.starts_with(b"APETAGEX")
        || (rest.len() <= 4096 && rest.iter().all(|&b| b == 0))
}

struct Header {
    len: usize,
    number: u64,
    samples: u32,
    variable: bool,
}

/// A frame header at the start of `d`, if one is there and agrees with
/// STREAMINFO.
fn parse_header(d: &[u8], info: &StreamInfo) -> Option<Header> {
    if d.len() < 6 || d[0] != 0xFF || d[1] & 0xFE != 0xF8 {
        return None;
    }
    let variable = d[1] & 1 == 1;
    let (bs, sr) = (d[2] >> 4, d[2] & 0x0F);
    let (ch, ss) = (d[3] >> 4, (d[3] >> 1) & 7);
    if bs == 0 || sr == 15 || ch > 10 || ss == 3 || d[3] & 1 != 0 {
        return None;
    }
    let channels = if ch < 8 { ch + 1 } else { 2 };
    let bits = [0, 8, 12, 0, 16, 20, 24, 32][ss as usize];
    if channels != info.channels || (bits != 0 && bits != info.bits) {
        return None;
    }
    let mut i = 4;
    let number = read_coded(d, &mut i)?;
    let mut take = |n: usize| -> Option<u32> {
        let v = d
            .get(i..i + n)?
            .iter()
            .fold(0u32, |a, &b| (a << 8) | u32::from(b));
        i += n;
        Some(v)
    };
    let samples = match bs {
        1 => 192,
        2..=5 => 576 << (bs - 2),
        6 => take(1)? + 1,
        7 => take(2)? + 1,
        _ => 256 << (bs - 8),
    };
    let rate = match sr {
        0 => info.sample_rate,
        1 => 88200,
        2 => 176400,
        3 => 192000,
        4 => 8000,
        5 => 16000,
        6 => 22050,
        7 => 24000,
        8 => 32000,
        9 => 44100,
        10 => 48000,
        11 => 96000,
        12 => take(1)? * 1000,
        13 => take(2)?,
        _ => take(2)? * 10,
    };
    if rate != info.sample_rate || crc8(&d[..i]) != *d.get(i)? {
        return None;
    }
    Some(Header {
        len: i + 1,
        number,
        samples,
        variable,
    })
}

/// The frame or sample number, in FLAC's UTF-8-like coding of up to 36 bits.
fn read_coded(d: &[u8], i: &mut usize) -> Option<u64> {
    let first = *d.get(*i)?;
    let (mut v, extra) = match first.leading_ones() {
        0 => (u64::from(first), 0),
        n @ 2..=7 => (u64::from(first & (0x7F >> n)), n as usize - 1),
        _ => return None,
    };
    *i += 1;
    for _ in 0..extra {
        let b = *d.get(*i)?;
        if b & 0xC0 != 0x80 {
            return None;
        }
        v = (v << 6) | u64::from(b & 0x3F);
        *i += 1;
    }
    Some(v)
}

fn crc8(d: &[u8]) -> u8 {
    d.iter().fold(0u8, |mut c, &b| {
        c ^= b;
        for _ in 0..8 {
            c = if c & 0x80 != 0 {
                (c << 1) ^ 0x07
            } else {
                c << 1
            };
        }
        c
    })
}

const CRC16: [u16; 256] = {
    let mut t = [0u16; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = (i as u16) << 8;
        let mut k = 0;
        while k < 8 {
            c = if c & 0x8000 != 0 {
                (c << 1) ^ 0x8005
            } else {
                c << 1
            };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
};

fn crc16_step(c: u16, b: u8) -> u16 {
    (c << 8) ^ CRC16[usize::from((c >> 8) as u8 ^ b)]
}

/// A seek point every ten seconds, each on the frame holding that sample.
pub fn seek_table(frames: &[Frame], info: &StreamInfo) -> Result<Block> {
    let interval = SEEK_INTERVAL_SECS * u64::from(info.sample_rate);
    ensure!(interval > 0, "STREAMINFO gives a sample rate of 0");
    let total = frames.last().map_or(0, |f| f.sample + u64::from(f.samples));
    let mut data = Vec::new();
    let (mut i, mut last) = (0usize, None);
    let mut target = 0u64;
    while target < total {
        while i + 1 < frames.len() && frames[i + 1].sample <= target {
            i += 1;
        }
        if last != Some(i) {
            let f = frames[i];
            let n = u16::try_from(f.samples)
                .map_err(|_| anyhow!("a {}-sample frame cannot be a seek point", f.samples))?;
            data.extend_from_slice(&f.sample.to_be_bytes());
            data.extend_from_slice(&f.offset.to_be_bytes());
            data.extend_from_slice(&n.to_be_bytes());
            last = Some(i);
        }
        target += interval;
    }
    Ok(Block {
        kind: SEEKTABLE,
        data,
    })
}

/// What a FLAC needs before it has usable seek points.
#[derive(Debug, Clone, PartialEq)]
pub enum SeekPlan {
    Present,
    /// The table to add, and the size and modification time it was computed
    /// against.
    Add(Block, Stamp),
    /// More points than the padding has room for.
    NoRoom {
        needed: usize,
        spare: usize,
    },
}

/// A file's size and modification time, to refuse writing a plan made against
/// a file that has since changed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stamp {
    len: u64,
    modified: std::time::SystemTime,
}

impl Stamp {
    fn of(path: &Path) -> Result<Self> {
        let m = std::fs::metadata(path)?;
        Ok(Self {
            len: m.len(),
            modified: m.modified()?,
        })
    }
}

pub struct Inspection {
    pub info: StreamInfo,
    pub frames: Frames,
    pub seek: SeekPlan,
}

/// Walk every frame of `path`, and plan a seek table if it has none.
pub fn inspect(path: &Path) -> Result<Inspection> {
    let stamp = Stamp::of(path)?;
    let meta = read(path)?;
    let info = meta.stream_info()?;
    let frames = frames(path, &meta)?;
    let seek = match &frames {
        _ if meta.has_seek_points() => SeekPlan::Present,
        // A damaged file's frames cannot be trusted as seek targets.
        Frames::Damaged { .. } => SeekPlan::Present,
        Frames::Clean(f) => {
            let table = seek_table(f, &info)?;
            // A placeholder-only table is replaced, so its room is free.
            let reclaimed: usize = meta
                .blocks
                .iter()
                .filter(|b| b.kind == SEEKTABLE)
                .map(|b| 4 + b.data.len())
                .sum();
            let (needed, spare) = (4 + table.data.len(), meta.spare() + reclaimed);
            if layout_fits(needed, spare) {
                SeekPlan::Add(table, stamp)
            } else {
                SeekPlan::NoRoom { needed, spare }
            }
        }
    };
    Ok(Inspection { info, frames, seek })
}

/// Whether `needed` bytes fit in `spare`, leaving either nothing or enough for
/// a padding block header.
fn layout_fits(needed: usize, spare: usize) -> bool {
    spare == needed || spare >= needed + 4
}

/// Write a table [`inspect`] planned, in place. The file keeps its size, its
/// modification time and every audio byte's offset, so rekordbox's row and a
/// USB export of it still describe the same file.
pub fn add_seek_table(path: &Path, table: &Block, stamp: &Stamp) -> Result<()> {
    ensure!(
        Stamp::of(path)? == *stamp,
        "{} changed since it was checked",
        path.display()
    );
    let meta = read(path)?;
    ensure!(
        !meta.has_seek_points(),
        "{} already has a seek table",
        path.display()
    );
    let mut blocks: Vec<Block> = meta
        .blocks
        .iter()
        .filter(|b| b.kind != SEEKTABLE)
        .cloned()
        .collect();
    blocks.insert(1, table.clone());
    rewrite(path, &meta, &blocks, true)?;
    File::options()
        .write(true)
        .open(path)?
        .set_modified(stamp.modified)?;
    Ok(())
}

/// Make `jpeg` the file's only picture, as its front cover. Everything else in
/// the metadata is kept. Written through a sibling, so an interrupted write
/// never leaves a damaged file under the real name.
pub fn set_cover(path: &Path, jpeg: &[u8], width: u32, height: u32) -> Result<Rewrite> {
    let meta = read(path)?;
    let mut blocks: Vec<Block> = meta
        .blocks
        .iter()
        .filter(|b| b.kind != PICTURE)
        .cloned()
        .collect();
    blocks.push(picture(jpeg, width, height)?);
    rewrite(path, &meta, &blocks, false)
}

fn picture(jpeg: &[u8], width: u32, height: u32) -> Result<Block> {
    const MIME: &[u8] = b"image/jpeg";
    let len = u32::try_from(jpeg.len()).map_err(|_| anyhow!("cover over 4 GiB"))?;
    let mut d = Vec::with_capacity(32 + MIME.len() + jpeg.len());
    for v in [FRONT_COVER, MIME.len() as u32] {
        d.extend_from_slice(&v.to_be_bytes());
    }
    d.extend_from_slice(MIME);
    // No description, then dimensions, 24-bit colour, not indexed.
    for v in [0, width, height, 24, 0, len] {
        d.extend_from_slice(&v.to_be_bytes());
    }
    d.extend_from_slice(jpeg);
    Ok(Block {
        kind: PICTURE,
        data: d,
    })
}

impl fmt::Display for Frames {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Clean(v) => write!(f, "{} frames, all intact", v.len()),
            Self::Damaged { at_sample } => write!(f, "damaged after sample {at_sample}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proc;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rr-flac-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A FLAC from ffmpeg's encoder, which writes no seek table. None without
    /// ffmpeg.
    fn encoded(dir: &Path, secs: u32) -> Option<PathBuf> {
        let out = dir.join("t.flac");
        let mut cmd = proc::capture("ffmpeg");
        cmd.args(["-v", "error", "-nostdin", "-y", "-f", "lavfi", "-i"])
            .arg(format!("sine=frequency=440:duration={secs}"))
            .args(["-ac", "2", "-c:a", "flac"])
            .arg(&out);
        let ok = cmd.output().ok()?.status.success();
        ok.then_some(out)
    }

    fn pcm_md5(p: &Path) -> String {
        let mut cmd = proc::capture("ffmpeg");
        cmd.args(["-v", "error", "-nostdin", "-i"])
            .arg(p)
            .args(["-map", "0:a", "-f", "md5", "-"]);
        String::from_utf8(cmd.output().unwrap().stdout).unwrap()
    }

    fn info() -> StreamInfo {
        StreamInfo {
            sample_rate: 44100,
            channels: 2,
            bits: 16,
            total_samples: 0,
        }
    }

    #[test]
    fn coded_numbers_decode_at_every_length() {
        let mut i = 0;
        assert_eq!(read_coded(&[0x7F], &mut i), Some(0x7F));
        let mut i = 0;
        assert_eq!(read_coded(&[0xC2, 0x80], &mut i), Some(0x80));
        assert_eq!(i, 2);
        let mut i = 0;
        // 0xFE leads the 7-byte form that carries 36-bit sample numbers.
        let seven = [0xFE, 0xBF, 0xBF, 0xBF, 0xBF, 0xBF, 0xBF];
        assert_eq!(read_coded(&seven, &mut i), Some(0xF_FFFF_FFFF));
        let mut i = 0;
        assert_eq!(read_coded(&[0xFF], &mut i), None);
        let mut i = 0;
        assert_eq!(read_coded(&[0xC2, 0x00], &mut i), None);
    }

    #[test]
    fn crcs_match_the_reference_check_values() {
        // The CRC catalogue's check input for both polynomials FLAC uses.
        assert_eq!(crc8(b"123456789"), 0xF4);
        let c = b"123456789".iter().fold(0u16, |c, &b| crc16_step(c, b));
        assert_eq!(c, 0xFEE8);
    }

    #[test]
    fn layout_fills_the_room_exactly_or_refuses() {
        let si = Block {
            kind: STREAMINFO,
            data: vec![0; 34],
        };
        let one = std::slice::from_ref(&si);
        let used = 4 + 4 + 34;
        assert_eq!(layout(one, used).unwrap().unwrap().len(), used);
        assert_eq!(layout(one, used + 100).unwrap().unwrap().len(), used + 100);
        // Too little left over for a padding header.
        assert!(layout(one, used + 2).unwrap().is_none());
        assert!(layout(one, used - 1).unwrap().is_none());
        // More padding than one block can describe.
        let big = used + MAX_BLOCK + 6;
        let out = layout(one, big).unwrap().unwrap();
        assert_eq!(out.len(), big);
        let meta = read_from(&mut out.as_slice()).unwrap();
        assert_eq!(meta.audio_offset as usize, big);
        assert_eq!(meta.blocks.iter().filter(|b| b.kind == PADDING).count(), 2);
    }

    #[test]
    fn seek_points_land_on_the_frame_holding_each_ten_second_mark() {
        let frames: Vec<Frame> = (0..2000u64)
            .map(|i| Frame {
                sample: i * 4096,
                offset: i * 1000,
                samples: 4096,
            })
            .collect();
        let t = seek_table(&frames, &info()).unwrap();
        let points: Vec<(u64, u64)> = t
            .data
            .chunks_exact(SEEK_POINT_LEN)
            .map(|p| {
                (
                    u64::from_be_bytes(p[..8].try_into().unwrap()),
                    u64::from_be_bytes(p[8..16].try_into().unwrap()),
                )
            })
            .collect();
        // 2000 × 4096 samples is 185.8 s, so marks 0, 10 … 180.
        assert_eq!(points.len(), 19);
        assert_eq!(points[0], (0, 0));
        // 441000 falls in frame 107 (438272..442368).
        assert_eq!(points[1], (107 * 4096, 107 * 1000));
        assert!(points.windows(2).all(|w| w[0].0 < w[1].0));
    }

    #[test]
    fn a_damaged_byte_is_found_and_a_clean_file_walks_to_its_total() {
        let dir = tmp("walk");
        let Some(p) = encoded(&dir, 20) else {
            return; // no ffmpeg here
        };
        let meta = read(&p).unwrap();
        let info = meta.stream_info().unwrap();
        let Frames::Clean(f) = frames(&p, &meta).unwrap() else {
            panic!("a fresh encode is clean");
        };
        assert_eq!(
            f.last().map(|l| l.sample + u64::from(l.samples)),
            Some(info.total_samples)
        );

        let mut bytes = std::fs::read(&p).unwrap();
        let mid = f.len() / 2;
        let inside = (f[mid].offset + f[mid + 1].offset) / 2;
        bytes[meta.audio_offset as usize + inside as usize] ^= 0x55;
        let audio = &bytes[meta.audio_offset as usize..];
        assert!(
            matches!(walk(audio, &info), Frames::Damaged { at_sample } if at_sample == f[mid].sample)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_seek_table_goes_in_without_moving_audio_or_the_timestamp() {
        let dir = tmp("seek");
        let Some(p) = encoded(&dir, 30) else {
            return;
        };
        let before_len = std::fs::metadata(&p).unwrap().len();
        let before_md5 = pcm_md5(&p);
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        File::options()
            .write(true)
            .open(&p)
            .unwrap()
            .set_modified(old)
            .unwrap();

        let ins = inspect(&p).unwrap();
        let SeekPlan::Add(table, stamp) = ins.seek else {
            panic!("ffmpeg's encoder writes no seek table, so one is planned");
        };
        add_seek_table(&p, &table, &stamp).unwrap();

        let m = std::fs::metadata(&p).unwrap();
        assert_eq!(m.len(), before_len);
        assert_eq!(m.modified().unwrap(), old);
        assert_eq!(pcm_md5(&p), before_md5);
        let meta = read(&p).unwrap();
        assert!(meta.has_seek_points());
        assert_eq!(meta.blocks[1].kind, SEEKTABLE);
        assert_eq!(inspect(&p).unwrap().seek, SeekPlan::Present);
        // Planned against the old stamp, so a second write is refused.
        assert!(add_seek_table(&p, &table, &stamp).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_cover_replaces_the_old_one_and_keeps_the_seek_table() {
        let dir = tmp("cover");
        let Some(p) = encoded(&dir, 15) else {
            return;
        };
        let ins = inspect(&p).unwrap();
        let SeekPlan::Add(table, stamp) = ins.seek else {
            panic!("planned");
        };
        add_seek_table(&p, &table, &stamp).unwrap();
        let before_md5 = pcm_md5(&p);

        set_cover(&p, b"\xFF\xD8\xFFfirst", 1, 1).unwrap();
        // Larger than the padding, so this one grows the file.
        let big = vec![0xAB; 20_000];
        assert_eq!(set_cover(&p, &big, 2, 2).unwrap(), Rewrite::Grown);

        let meta = read(&p).unwrap();
        let pics: Vec<&Block> = meta.blocks.iter().filter(|b| b.kind == PICTURE).collect();
        assert_eq!(pics.len(), 1);
        assert!(pics[0].data.ends_with(&big));
        assert!(meta.has_seek_points());
        assert_eq!(pcm_md5(&p), before_md5);
        assert!(matches!(frames(&p, &meta).unwrap(), Frames::Clean(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
