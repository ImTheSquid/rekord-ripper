//! Album art: out of a download, into it, and into the place rekordbox looks.
//!
//! Rekordbox 7 has no artwork table — `imageFile` exists but stays empty, being
//! cloud-sync bookkeeping. Art is a path on the track row itself,
//! `djmdContent.ImagePath`, pointing into a cache under `share/` keyed on that
//! row's own UUID. So a hand-created row can compute where its art belongs
//! before it is inserted, and nothing else has to be reconciled.
//!
//! Embedding into the audio file is separate and additive: it uses `-c:a copy`,
//! which leaves the audio stream bit-identical, so a fingerprint taken either
//! side of an embed still matches. It does change the file's size, which is what
//! [`crate::import::existing_row_for_content`] dedups on and what
//! [`crate::pending`] guards its entries with — so embed before either records
//! the file, never after.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

use crate::proc;

/// One picture stream is quick; a corrupt file is not.
const TIMEOUT: Duration = Duration::from_secs(60);

/// Longest edge rekordbox keeps for the full-size copy. Its own cache tops out
/// here, and a 3000px original is a megabyte spent on a 240px browser tile.
const FULL_MAX: u32 = 800;

/// The two thumbnails rekordbox generates, always exactly square.
const SIZES: [(&str, u32); 2] = [("artwork_m.jpg", 240), ("artwork_s.jpg", 80)];

/// Where a track's art lives, relative to rekordbox's `share/` directory.
///
/// The UUID is split after three characters and a directory hung off each half —
/// the same scheme `AnalysisDataPath` uses, so
/// [`crate::db::MasterDb::resolve_analysis_path`] resolves this too.
pub fn image_path_for(uuid: &str) -> Result<String> {
    // Four is the minimum that leaves something on both sides of the split; a
    // real UUID has 36. Guarding here keeps a malformed uuid from silently
    // producing a path directly under Artwork/.
    if uuid.len() < 4 || !uuid.is_char_boundary(3) {
        bail!("cannot derive an artwork path from uuid {uuid:?}");
    }
    let (head, tail) = uuid.split_at(3);
    Ok(format!("/PIONEER/Artwork/{head}/{tail}/artwork.jpg"))
}

/// Whether this container can carry cover art at all.
///
/// WAV cannot: its muxer rejects every video stream, and roughly an eighth of
/// what gets downloaded is WAV. Those tracks still get rekordbox-side art — only
/// the embed is skipped.
pub fn container_holds_art(audio: &Path) -> bool {
    !matches!(
        audio
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "wav" | "wave"
    )
}

/// True when `audio` already has a cover embedded.
pub fn has_embedded(audio: &Path) -> Result<bool> {
    let mut cmd = proc::capture("ffprobe");
    cmd.args([
        "-v",
        "error",
        "-select_streams",
        "v",
        "-show_entries",
        "stream=codec_name",
        "-of",
        "csv=p=0",
    ])
    .arg(audio);
    let out = proc::run_with_deadline(cmd, Instant::now() + TIMEOUT)?;
    // A file with no picture is not a failure, but an unreadable one is.
    if !out.status.success() {
        bail!(
            "ffprobe failed on {}: {}",
            audio.display(),
            proc::stderr_tail(&out.stderr)
        );
    }
    Ok(!String::from_utf8_lossy(&out.stdout).trim().is_empty())
}

/// Pull the embedded cover out of `audio` into `out` as a JPEG.
///
/// `Ok(false)` means the file simply has no art — the common case, and not an
/// error. Covers are often PNG, so this transcodes rather than copies: every
/// path downstream expects `.jpg`.
pub fn extract(audio: &Path, out: &Path) -> Result<bool> {
    if !has_embedded(audio)? {
        return Ok(false);
    }
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut cmd = proc::capture("ffmpeg");
    cmd.args(["-v", "error", "-nostdin", "-y", "-i"])
        .arg(audio)
        .args([
            "-map",
            "0:v:0",
            "-frames:v",
            "1",
            "-c:v",
            "mjpeg",
            "-f",
            "image2",
        ])
        .arg(out);
    let res = proc::run_with_deadline(cmd, Instant::now() + TIMEOUT)?;
    if !res.status.success() || !out.exists() {
        bail!(
            "extracting art from {}: {}",
            audio.display(),
            proc::stderr_tail(&res.stderr)
        );
    }
    Ok(true)
}

/// Mux `image` into `audio` as its cover, in place.
///
/// The audio stream is copied, not re-encoded, so the result decodes to the same
/// samples. Written to a sibling and renamed, so an interrupted mux never leaves
/// a truncated file under the real name.
pub fn embed(audio: &Path, image: &Path) -> Result<()> {
    if !container_holds_art(audio) {
        bail!(
            "{} is a container that cannot carry cover art",
            audio.display()
        );
    }
    let ext = audio
        .extension()
        .and_then(|e| e.to_str())
        .ok_or_else(|| anyhow!("{} has no extension", audio.display()))?
        .to_ascii_lowercase();

    // ffmpeg needs the output extension to pick a muxer, so the temporary keeps
    // the real one and hides behind a dotted prefix instead.
    let staged = staged_sibling(audio, &ext)?;

    let mut cmd = proc::capture("ffmpeg");
    cmd.args(["-v", "error", "-nostdin", "-y", "-i"])
        .arg(audio)
        .arg("-i")
        .arg(image)
        .args(["-map", "0:a", "-map", "1:v", "-c:a", "copy"]);
    match ext.as_str() {
        // ID3 containers want the picture left as JPEG and described, or players
        // show it as a generic attachment rather than the cover.
        "mp3" => cmd.args([
            "-c:v",
            "copy",
            "-id3v2_version",
            "3",
            "-metadata:s:v",
            "title=Album cover",
            "-metadata:s:v",
            "comment=Cover (front)",
        ]),
        "aiff" | "aif" | "aiffc" => cmd.args([
            "-c:v",
            "copy",
            "-write_id3v2",
            "1",
            "-metadata:s:v",
            "title=Album cover",
            "-metadata:s:v",
            "comment=Cover (front)",
        ]),
        // FLAC and MP4 want a stream flagged as the attached picture.
        _ => cmd.args(["-c:v", "mjpeg", "-disposition:v", "attached_pic"]),
    };
    cmd.arg(&staged);

    let out = proc::run_with_deadline(cmd, Instant::now() + TIMEOUT).inspect_err(|_| {
        let _ = std::fs::remove_file(&staged);
    })?;
    if !out.status.success() || std::fs::metadata(&staged).map(|m| m.len()).unwrap_or(0) == 0 {
        let _ = std::fs::remove_file(&staged);
        bail!(
            "embedding art into {}: {}",
            audio.display(),
            proc::stderr_tail(&out.stderr)
        );
    }
    std::fs::rename(&staged, audio).inspect_err(|_| {
        let _ = std::fs::remove_file(&staged);
    })?;
    Ok(())
}

/// Where a cover sits when the audio file itself cannot hold one.
///
/// A WAV rip's art only ever exists as a URL, and the download that knows the
/// URL is not the step that creates the rekordbox row — the pending queue sits
/// between them, holding a path and nothing else. A sidecar at a name derived
/// from the audio file bridges that without a queue column. Dotted, so nothing
/// scanning for audio picks it up.
pub fn sidecar_for(audio: &Path) -> Option<PathBuf> {
    let stem = audio.file_stem()?.to_str()?;
    let stem: String = stem.chars().take(120).collect();
    Some(audio.parent()?.join(format!(".{stem}.rr-cover.jpg")))
}

/// A `.rr-art-<name>` sibling, so the temporary keeps the extension ffmpeg needs
/// to pick a muxer while staying out of the way of anything scanning for audio.
fn staged_sibling(audio: &Path, ext: &str) -> Result<PathBuf> {
    let stem = audio
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow!("{} has no filename", audio.display()))?;
    let parent = audio.parent().unwrap_or(Path::new("."));
    // Capped: the original stem can already be at the filesystem's limit, and
    // the prefix has to fit alongside it.
    let stem: String = stem.chars().take(120).collect();
    Ok(parent.join(format!(".rr-art-{stem}.{ext}")))
}

/// Write the three copies rekordbox keeps and return the `ImagePath` value.
///
/// `share_dir` is rekordbox's `share/`; `uuid` is the track row's own UUID.
pub fn install(share_dir: &Path, uuid: &str, image: &Path) -> Result<String> {
    let rel = image_path_for(uuid)?;
    let full = share_dir.join(rel.trim_start_matches('/'));
    let dir = full
        .parent()
        .ok_or_else(|| anyhow!("artwork path {rel} has no directory"))?;
    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating artwork directory {}", dir.display()))?;

    // Capped rather than resized: rekordbox stores the source at its own size up
    // to this, and upscaling a 300px cover to 800 invents detail.
    convert(
        image,
        &full,
        &format!(
            "scale=w='min(iw,{FULL_MAX})':h='min(ih,{FULL_MAX})':\
             force_original_aspect_ratio=decrease"
        ),
    )?;
    for (name, px) in SIZES {
        convert(
            image,
            &dir.join(name),
            // Centre-cropped, because these two are always square in rekordbox's
            // cache and a letterboxed tile looks broken next to the rest.
            &format!("scale={px}:{px}:force_original_aspect_ratio=increase,crop={px}:{px}"),
        )?;
    }
    Ok(rel)
}

/// One ffmpeg still-image conversion.
fn convert(src: &Path, dest: &Path, filter: &str) -> Result<()> {
    let mut cmd = proc::capture("ffmpeg");
    cmd.args(["-v", "error", "-nostdin", "-y", "-i"])
        .arg(src)
        .args(["-vf", filter, "-frames:v", "1"])
        .arg(dest);
    let out = proc::run_with_deadline(cmd, Instant::now() + TIMEOUT)?;
    if !out.status.success() || std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0) == 0 {
        let _ = std::fs::remove_file(dest);
        bail!(
            "writing {}: {}",
            dest.display(),
            proc::stderr_tail(&out.stderr)
        );
    }
    Ok(())
}

/// Which inline-image escape sequence this terminal understands, if any.
///
/// Reviewing a cover you cannot see is pointless, so the manual pass draws it in
/// the terminal. Detected from the environment rather than by querying the
/// terminal: a query needs a raw-mode read with a timeout, and guessing wrong
/// there means either a hang or escape bytes printed as text.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InlineImages {
    /// Kitty's graphics protocol — kitty, Ghostty, WezTerm, Konsole.
    Kitty,
    /// iTerm2's `OSC 1337 File=`, also understood by WezTerm.
    Iterm2,
    None,
}

pub fn inline_image_support() -> InlineImages {
    // A pipe or a pager is not a terminal, and neither is a CI log; writing
    // graphics escapes into one dumps kilobytes of base64 as text.
    if !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        return InlineImages::None;
    }
    let env = |k: &str| std::env::var(k).unwrap_or_default();
    detect_inline_images(
        &env("TERM"),
        &env("TERM_PROGRAM"),
        &env("KITTY_WINDOW_ID"),
        &env("ITERM_SESSION_ID"),
    )
}

/// The environment half of [`inline_image_support`], without the tty check.
fn detect_inline_images(
    term: &str,
    term_program: &str,
    kitty_window_id: &str,
    iterm_session_id: &str,
) -> InlineImages {
    let program = term_program.to_ascii_lowercase();
    // Kitty first: WezTerm speaks both, and its graphics support is the better
    // of the two.
    if term.contains("kitty")
        || !kitty_window_id.is_empty()
        || matches!(program.as_str(), "ghostty" | "wezterm")
    {
        return InlineImages::Kitty;
    }
    if program == "iterm.app" || !iterm_session_id.is_empty() {
        return InlineImages::Iterm2;
    }
    InlineImages::None
}

/// Draw `image` inline, `cols` cells wide. `false` if it could not be drawn.
///
/// Cells are roughly twice as tall as they are wide, so a square cover asks for
/// half as many rows as columns.
pub fn show_inline(image: &Path, cols: u16) -> bool {
    let mode = inline_image_support();
    if mode == InlineImages::None {
        return false;
    }
    // Scaled down first: the payload is base64 inside an escape sequence, and a
    // 1200px cover is a megabyte of terminal write for a thumbnail-sized draw.
    let Ok(small) = to_preview_png(image) else {
        return false;
    };
    let Ok(bytes) = std::fs::read(&small.0) else {
        return false;
    };
    let data = base64_standard(&bytes);
    let rows = (cols / 2).max(1);

    let mut out = std::io::stdout().lock();
    let ok = match mode {
        InlineImages::Iterm2 => writeln!(
            out,
            "\x1b]1337;File=inline=1;width={cols};height={rows};\
             preserveAspectRatio=1:{data}\x07"
        )
        .is_ok(),
        InlineImages::Kitty => write_kitty(&mut out, &data, cols, rows).is_ok(),
        InlineImages::None => false,
    };
    let _ = out.flush();
    ok
}

/// Kitty wants the payload split into 4096-byte chunks, each flagged with
/// whether another follows.
fn write_kitty(out: &mut impl Write, data: &str, cols: u16, rows: u16) -> std::io::Result<()> {
    const CHUNK: usize = 4096;
    let bytes = data.as_bytes();
    let mut chunks = bytes.chunks(CHUNK).peekable();
    let mut first = true;
    while let Some(chunk) = chunks.next() {
        let more = u8::from(chunks.peek().is_some());
        if first {
            // f=100 is PNG; a=T transmits and displays in one go.
            write!(out, "\x1b_Gf=100,a=T,c={cols},r={rows},m={more};")?;
            first = false;
        } else {
            write!(out, "\x1b_Gm={more};")?;
        }
        out.write_all(chunk)?;
        write!(out, "\x1b\\")?;
    }
    writeln!(out)
}

/// A small PNG copy, since Kitty's inline format is PNG and a preview does not
/// need the full-size cover.
fn to_preview_png(image: &Path) -> Result<ScratchFile> {
    let dest = crate::paths::scratch_root()?.join(format!("preview-{}.png", uuid::Uuid::new_v4()));
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    convert(
        image,
        &dest,
        "scale=w='min(iw,320)':h='min(ih,320)':force_original_aspect_ratio=decrease",
    )?;
    Ok(ScratchFile(dest))
}

/// A temporary that removes itself, so previews do not pile up in scratch.
pub struct ScratchFile(pub PathBuf);

impl Drop for ScratchFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn base64_standard(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Leading bytes of the image formats worth accepting.
///
/// The failure this prevents is the same one [`crate::acquire::fs`] guards
/// against for audio: an artwork URL that answers with an HTML error page, saved
/// as `artwork.jpg`, gives rekordbox a browser tile of nothing and no clue why.
pub fn looks_like_image(head: &[u8]) -> bool {
    head.starts_with(&[0xff, 0xd8, 0xff])                       // JPEG
        || head.starts_with(b"\x89PNG\r\n\x1a\n")               // PNG
        || head.starts_with(b"GIF87a")
        || head.starts_with(b"GIF89a")
        || (head.len() >= 12 && &head[0..4] == b"RIFF" && &head[8..12] == b"WEBP")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let p = std::env::temp_dir().join(format!("rr-art-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// A square JPEG, via ffmpeg's own test source, so these tests need no fixture.
    fn test_image(dir: &Path, px: u32) -> Option<PathBuf> {
        let out = dir.join(format!("src{px}.jpg"));
        let mut cmd = proc::capture("ffmpeg");
        cmd.args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg(format!("testsrc=size={px}x{px}:duration=1"))
            .args(["-frames:v", "1"])
            .arg(&out);
        let ok = proc::run_with_deadline(cmd, Instant::now() + TIMEOUT)
            .map(|o| o.status.success())
            .unwrap_or(false);
        ok.then_some(out)
    }

    fn dimensions(p: &Path) -> Option<(u32, u32)> {
        let mut cmd = proc::capture("ffprobe");
        cmd.args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height",
            "-of",
            "csv=p=0",
        ])
        .arg(p);
        let out = proc::run_with_deadline(cmd, Instant::now() + TIMEOUT).ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        let (w, h) = text.trim().split_once(',')?;
        Some((w.parse().ok()?, h.parse().ok()?))
    }

    #[test]
    fn the_terminals_that_can_draw_a_cover_are_recognised() {
        use InlineImages::*;
        // Ghostty, which is what this gets used in.
        assert_eq!(
            detect_inline_images("xterm-256color", "ghostty", "", ""),
            Kitty
        );
        assert_eq!(detect_inline_images("xterm-kitty", "", "", ""), Kitty);
        assert_eq!(detect_inline_images("xterm-256color", "", "3", ""), Kitty);
        // WezTerm speaks both; the better protocol wins.
        assert_eq!(
            detect_inline_images("xterm-256color", "WezTerm", "", ""),
            Kitty
        );
        assert_eq!(
            detect_inline_images("xterm-256color", "iTerm.app", "", ""),
            Iterm2
        );
        assert_eq!(
            detect_inline_images("xterm-256color", "", "", "w0t0p0"),
            Iterm2
        );
        // Anything unrecognised must fall back to describing the cover, not to
        // dumping base64 into the scrollback.
        assert_eq!(
            detect_inline_images("xterm-256color", "Apple_Terminal", "", ""),
            None
        );
        assert_eq!(detect_inline_images("dumb", "", "", ""), None);
    }

    #[test]
    fn a_kitty_payload_is_chunked_with_a_continuation_flag_on_all_but_the_last() {
        // The protocol caps a chunk at 4096 bytes, and a wrong m= flag leaves
        // the terminal waiting for a chunk that never comes.
        let data = "A".repeat(9000);
        let mut out = Vec::new();
        write_kitty(&mut out, &data, 20, 10).unwrap();
        let s = String::from_utf8(out).unwrap();

        // 9000 bytes over 4096 is three chunks: two with more, one without.
        assert_eq!(s.matches("\x1b_G").count(), 3, "three chunks");
        assert_eq!(s.matches("m=1;").count(), 2, "two say more follows");
        assert_eq!(s.matches("m=0;").count(), 1, "the last says it is done");
        // Only the first carries the format and placement.
        assert_eq!(s.matches("f=100,a=T").count(), 1);
        assert!(s.contains("c=20,r=10"));
        // Every chunk is terminated, or the terminal eats what follows.
        assert_eq!(s.matches("\x1b\\").count(), 3);
        assert_eq!(s.matches('A').count(), 9000, "no payload lost");
    }

    #[test]
    fn a_payload_that_fits_in_one_chunk_is_sent_as_one() {
        let mut out = Vec::new();
        write_kitty(&mut out, "SHORT", 8, 4).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert_eq!(s.matches("\x1b_G").count(), 1);
        assert_eq!(s.matches("m=0;").count(), 1, "nothing more follows");
        assert!(!s.contains("m=1;"));
    }

    #[test]
    fn the_image_path_splits_the_uuid_the_way_rekordbox_does() {
        // Taken from a real row: uuid a013c4dd-ee8d-4b87-8c5d-74b9d82f622a.
        assert_eq!(
            image_path_for("a013c4dd-ee8d-4b87-8c5d-74b9d82f622a").unwrap(),
            "/PIONEER/Artwork/a01/3c4dd-ee8d-4b87-8c5d-74b9d82f622a/artwork.jpg"
        );
    }

    #[test]
    fn a_uuid_too_short_to_split_is_refused_rather_than_producing_a_stray_path() {
        // Without the guard this would write directly under Artwork/.
        assert!(image_path_for("abc").is_err());
        assert!(image_path_for("").is_err());
    }

    #[test]
    fn wav_is_the_container_that_cannot_hold_art() {
        assert!(!container_holds_art(Path::new("a.wav")));
        assert!(!container_holds_art(Path::new("a.WAV")));
        for ok in ["a.flac", "a.mp3", "a.m4a", "a.aiff"] {
            assert!(container_holds_art(Path::new(ok)), "{ok}");
        }
    }

    #[test]
    fn embedding_into_a_wav_is_refused_by_name_not_by_running_ffmpeg() {
        let err = embed(Path::new("x.wav"), Path::new("c.jpg"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("cannot carry cover art"), "got: {err}");
    }

    #[test]
    fn image_magic_is_recognised_and_markup_is_not() {
        assert!(looks_like_image(&[0xff, 0xd8, 0xff, 0xe0]));
        assert!(looks_like_image(b"\x89PNG\r\n\x1a\n"));
        assert!(looks_like_image(b"GIF89a..."));
        assert!(looks_like_image(b"RIFF\x00\x00\x00\x00WEBPVP8 "));
        // The actual failure mode: an error page saved as artwork.jpg.
        assert!(!looks_like_image(b"<!DOCTYPE html><html>"));
        assert!(!looks_like_image(b"{\"error\":\"not found\"}"));
        assert!(!looks_like_image(b""));
    }

    #[test]
    fn install_writes_the_three_sizes_rekordbox_keeps() {
        let dir = tmp();
        let Some(src) = test_image(&dir, 1400) else {
            return; // no ffmpeg here; covered where there is one
        };
        let share = dir.join("share");
        let uuid = "a013c4dd-ee8d-4b87-8c5d-74b9d82f622a";

        let rel = install(&share, uuid, &src).unwrap();
        assert_eq!(rel, image_path_for(uuid).unwrap());

        let art_dir = share.join("PIONEER/Artwork/a01/3c4dd-ee8d-4b87-8c5d-74b9d82f622a");
        // Capped, not resized to a fixed number.
        assert_eq!(dimensions(&art_dir.join("artwork.jpg")), Some((800, 800)));
        assert_eq!(dimensions(&art_dir.join("artwork_m.jpg")), Some((240, 240)));
        assert_eq!(dimensions(&art_dir.join("artwork_s.jpg")), Some((80, 80)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_cover_smaller_than_the_cap_is_not_upscaled() {
        let dir = tmp();
        let Some(src) = test_image(&dir, 300) else {
            return;
        };
        let share = dir.join("share");
        install(&share, "a013c4dd-ee8d-4b87-8c5d-74b9d82f622a", &src).unwrap();
        let art_dir = share.join("PIONEER/Artwork/a01/3c4dd-ee8d-4b87-8c5d-74b9d82f622a");
        assert_eq!(dimensions(&art_dir.join("artwork.jpg")), Some((300, 300)));
        // The thumbnails are fixed sizes, so they do scale up.
        assert_eq!(dimensions(&art_dir.join("artwork_m.jpg")), Some((240, 240)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_file_with_no_cover_reports_absence_rather_than_failing() {
        let dir = tmp();
        let audio = dir.join("silence.flac");
        let mut cmd = proc::capture("ffmpeg");
        cmd.args([
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "anullsrc=r=44100:cl=mono",
            "-t",
            "1",
        ])
        .arg(&audio);
        if proc::run_with_deadline(cmd, Instant::now() + TIMEOUT)
            .map(|o| !o.status.success())
            .unwrap_or(true)
        {
            return; // no ffmpeg here
        }
        assert!(!has_embedded(&audio).unwrap());
        assert!(!extract(&audio, &dir.join("out.jpg")).unwrap());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_embedded_cover_round_trips_and_leaves_the_audio_stream_alone() {
        // The property the whole design rests on: `-c:a copy` means a fingerprint
        // taken either side of an embed still matches.
        let dir = tmp();
        let Some(cover) = test_image(&dir, 500) else {
            return;
        };
        let audio = dir.join("track.flac");
        let mut cmd = proc::capture("ffmpeg");
        cmd.args([
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=2",
        ])
        .arg(&audio);
        if proc::run_with_deadline(cmd, Instant::now() + TIMEOUT)
            .map(|o| !o.status.success())
            .unwrap_or(true)
        {
            return;
        }
        let before = audio_stream_md5(&audio);

        embed(&audio, &cover).unwrap();
        assert!(
            has_embedded(&audio).unwrap(),
            "the cover should be readable"
        );
        assert_eq!(before, audio_stream_md5(&audio), "audio must not change");
        assert!(
            !dir.join(".rr-art-track.flac").exists(),
            "the staged sibling should be gone"
        );

        // And it comes back out.
        let out = dir.join("recovered.jpg");
        assert!(extract(&audio, &out).unwrap());
        assert_eq!(dimensions(&out), Some((500, 500)));
        std::fs::remove_dir_all(&dir).ok();
    }

    fn audio_stream_md5(p: &Path) -> String {
        let mut cmd = proc::capture("ffmpeg");
        cmd.args(["-v", "error", "-i"])
            .arg(p)
            .args(["-map", "0:a", "-c:a", "copy", "-f", "md5", "-"]);
        let out = proc::run_with_deadline(cmd, Instant::now() + TIMEOUT).unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }
}
