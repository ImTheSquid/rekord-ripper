//! Getting a downloaded file into its final place, safely.
//!
//! Two rules every backend shares:
//!
//! * Write to a `.part` sibling and rename on completion, so an interrupted
//!   download never leaves a truncated file that looks finished. This matters
//!   more than usual here: a half-downloaded FLAC that rekordbox imports and
//!   analyses would produce a plausible-looking but wrong beat grid.
//! * Never silently clobber. Overwriting is opt-in; otherwise the new file gets
//!   a ` (2)` suffix.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use super::error::{BackendError, Result};

/// Characters that cause trouble in a filename on either platform, plus the path
/// separator itself.
fn sanitize_component(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    // A leading dot would hide the file; a trailing dot or space breaks Windows.
    let trimmed = cleaned.trim().trim_end_matches('.').trim_start_matches('.');
    let out = trimmed.trim();
    if out.is_empty() {
        "untitled".to_string()
    } else {
        out.to_string()
    }
}

/// `Artist - Title.ext`, sanitised, with a length cap that leaves room for the
/// de-duplication suffix.
pub fn track_filename(artist: Option<&str>, title: Option<&str>, ext: &str) -> String {
    let stem = match (artist, title) {
        (Some(a), Some(t)) if !a.trim().is_empty() => {
            format!("{} - {}", sanitize_component(a), sanitize_component(t))
        }
        (_, Some(t)) => sanitize_component(t),
        (Some(a), None) => sanitize_component(a),
        (None, None) => "untitled".to_string(),
    };
    // 150 chars keeps the whole path clear of the 255-byte limit most
    // filesystems impose, even with a multi-byte title.
    let stem: String = stem.chars().take(150).collect();
    format!("{stem}.{ext}")
}

/// A path that does not exist yet, by appending ` (2)`, ` (3)`, … before the
/// extension.
pub fn unique_path(desired: &Path) -> PathBuf {
    if !desired.exists() {
        return desired.to_path_buf();
    }
    let parent = desired.parent().unwrap_or(Path::new("."));
    let stem = desired
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("untitled");
    let ext = desired.extension().and_then(|s| s.to_str());

    for n in 2..10_000 {
        let name = match ext {
            Some(e) => format!("{stem} ({n}).{e}"),
            None => format!("{stem} ({n})"),
        };
        let candidate = parent.join(name);
        if !candidate.exists() {
            return candidate;
        }
    }
    // Astronomically unlikely; a unique suffix beats failing.
    parent.join(format!("{stem}-{}", uuid::Uuid::new_v4()))
}

/// Move `src` into `dest_dir`, keeping its filename.
///
/// Tries a rename first and falls back to copy-then-delete, because the staging
/// directory and the destination can be on different filesystems.
pub fn place(src: &Path, dest_dir: &Path, overwrite: bool) -> Result<PathBuf> {
    std::fs::create_dir_all(dest_dir)?;
    let name = src
        .file_name()
        .ok_or_else(|| BackendError::Other(anyhow::anyhow!("downloaded path has no filename")))?;
    let desired = dest_dir.join(name);
    let target = if overwrite {
        desired
    } else {
        unique_path(&desired)
    };

    if std::fs::rename(src, &target).is_err() {
        std::fs::copy(src, &target)?;
        std::fs::remove_file(src)?;
    }
    Ok(target)
}

/// Leading bytes that mean "this is markup, not audio".
///
/// The specific failure this exists to prevent: Bandcamp answers a
/// not-yet-prepared download with an HTML page, and writing that out under a
/// `.flac` name gives rekordbox a file it will import and analyse into nonsense.
/// A wrong file that looks plausible is worse than a failed download.
fn looks_like_markup(head: &[u8]) -> bool {
    let s = String::from_utf8_lossy(head);
    let t = s.trim_start().to_ascii_lowercase();
    t.starts_with("<!doctype")
        || t.starts_with("<html")
        || t.starts_with("<?xml")
        || t.starts_with("<head")
}

/// Like [`write_atomically`], but refuses to write markup to an audio path.
pub fn write_audio_atomically(target: &Path, reader: &mut impl Read) -> Result<u64> {
    // Peek before committing, so an interstitial never reaches the target name.
    let mut head = [0u8; 64];
    let mut filled = 0;
    while filled < head.len() {
        match reader.read(&mut head[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) => return Err(BackendError::Io(e)),
        }
    }
    let head = &head[..filled];
    if filled == 0 {
        return Err(BackendError::Other(anyhow::anyhow!(
            "download produced no data"
        )));
    }
    if looks_like_markup(head) {
        return Err(BackendError::Other(anyhow::anyhow!(
            "the server sent an HTML page instead of audio — refusing to save it as {}",
            target
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        )));
    }

    // Put the peeked bytes back in front of the rest of the stream.
    let mut full = std::io::Read::chain(std::io::Cursor::new(head.to_vec()), reader);
    write_atomically(target, &mut full)
}

/// Stream `reader` to `target` via a `.part` sibling, renaming only on success.
///
/// The temporary is removed if the copy fails, so a failed download leaves
/// nothing behind for rekordbox to find.
pub fn write_atomically(target: &Path, reader: &mut impl Read) -> Result<u64> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let part = target.with_extension(format!(
        "{}.part",
        target
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("download")
    ));

    let written = {
        let mut file = std::fs::File::create(&part)?;
        match std::io::copy(reader, &mut file) {
            Ok(n) => {
                file.flush()?;
                n
            }
            Err(e) => {
                drop(file);
                let _ = std::fs::remove_file(&part);
                return Err(BackendError::Io(e));
            }
        }
    };

    if written == 0 {
        let _ = std::fs::remove_file(&part);
        return Err(BackendError::Other(anyhow::anyhow!(
            "download produced no data"
        )));
    }

    std::fs::rename(&part, target).map_err(|e| {
        let _ = std::fs::remove_file(&part);
        BackendError::Io(e)
    })?;
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let p = std::env::temp_dir().join(format!("rr-fs-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn builds_an_artist_title_filename() {
        assert_eq!(
            track_filename(Some("Burial"), Some("Archangel"), "flac"),
            "Burial - Archangel.flac"
        );
        assert_eq!(
            track_filename(None, Some("Archangel"), "mp3"),
            "Archangel.mp3"
        );
        assert_eq!(track_filename(None, None, "wav"), "untitled.wav");
    }

    #[test]
    fn strips_path_separators_so_a_title_cannot_escape_the_directory() {
        // A remote-supplied title must never be able to write outside dest_dir.
        let name = track_filename(Some("../../etc"), Some("passwd"), "flac");
        assert!(!name.contains('/'), "got {name}");
        assert!(!name.contains('\\'), "got {name}");
        // Separators become underscores and the leading dots are stripped too.
        assert_eq!(name, "_.._etc - passwd.flac");
    }

    #[test]
    fn strips_characters_that_break_windows_paths() {
        let name = track_filename(Some("A:B"), Some(r#"C*D?E"F<G>H|I"#), "mp3");
        for bad in [':', '*', '?', '"', '<', '>', '|'] {
            assert!(!name.contains(bad), "{bad} survived in {name}");
        }
    }

    #[test]
    fn a_leading_dot_cannot_produce_a_hidden_file() {
        let name = track_filename(None, Some(".hidden"), "flac");
        assert!(!name.starts_with('.'), "got {name}");
    }

    #[test]
    fn an_all_punctuation_title_still_yields_a_usable_name() {
        assert_eq!(track_filename(None, Some("///"), "flac"), "___.flac");
        assert_eq!(track_filename(None, Some("   "), "flac"), "untitled.flac");
    }

    #[test]
    fn long_titles_are_capped_below_the_filesystem_limit() {
        let name = track_filename(Some(&"a".repeat(300)), Some(&"b".repeat(300)), "flac");
        assert!(
            name.chars().count() <= 156,
            "got {} chars",
            name.chars().count()
        );
    }

    #[test]
    fn unique_path_leaves_a_free_name_alone() {
        let dir = tmp();
        let p = dir.join("x.flac");
        assert_eq!(unique_path(&p), p);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unique_path_suffixes_before_the_extension() {
        let dir = tmp();
        let p = dir.join("x.flac");
        std::fs::write(&p, b"1").unwrap();
        assert_eq!(unique_path(&p), dir.join("x (2).flac"));
        std::fs::write(dir.join("x (2).flac"), b"2").unwrap();
        assert_eq!(unique_path(&p), dir.join("x (3).flac"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn place_moves_the_file_and_does_not_clobber_by_default() {
        let dir = tmp();
        let dest = dir.join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("song.flac"), b"existing").unwrap();

        let staged = dir.join("song.flac");
        std::fs::write(&staged, b"new").unwrap();

        let out = place(&staged, &dest, false).unwrap();
        assert_eq!(out.file_name().unwrap(), "song (2).flac");
        assert_eq!(std::fs::read(dest.join("song.flac")).unwrap(), b"existing");
        assert!(!staged.exists(), "the staged file should have moved");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn place_overwrites_only_when_asked() {
        let dir = tmp();
        let dest = dir.join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("song.flac"), b"existing").unwrap();
        let staged = dir.join("song.flac");
        std::fs::write(&staged, b"new").unwrap();

        let out = place(&staged, &dest, true).unwrap();
        assert_eq!(out.file_name().unwrap(), "song.flac");
        assert_eq!(std::fs::read(&out).unwrap(), b"new");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn atomic_write_produces_the_file_and_leaves_no_part_behind() {
        let dir = tmp();
        let target = dir.join("out.flac");
        let mut src = std::io::Cursor::new(b"audio data".to_vec());
        let n = write_atomically(&target, &mut src).unwrap();
        assert_eq!(n, 10);
        assert_eq!(std::fs::read(&target).unwrap(), b"audio data");
        assert!(!dir.join("out.flac.part").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn refuses_to_save_an_html_page_as_audio() {
        // The real regression: bandcamp answers a not-yet-prepared download with
        // its download page, and this used to land as a 229KB ".flac" that
        // rekordbox would happily import and analyse into nonsense.
        let dir = tmp();
        let target = dir.join("track.flac");
        let page = b"<!DOCTYPE html>\n<html><head><title>Bandcamp</title></head><body>preparing</body></html>";
        let mut src = std::io::Cursor::new(page.to_vec());

        let err = write_audio_atomically(&target, &mut src).unwrap_err();
        assert!(
            err.to_string().contains("HTML page instead of audio"),
            "got: {err}"
        );
        assert!(!target.exists(), "nothing may be written");
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            0,
            "not even a .part"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn detects_markup_regardless_of_leading_whitespace_or_case() {
        // The observed page began with spaces and newlines before the doctype.
        assert!(looks_like_markup(b"    \n\n<!DOCTYPE html>"));
        assert!(looks_like_markup(b"<html>"));
        assert!(looks_like_markup(b"<?xml version=\"1.0\"?>"));
        assert!(looks_like_markup(b"<!doctype HTML"));
    }

    #[test]
    fn real_audio_magic_is_not_mistaken_for_markup() {
        assert!(!looks_like_markup(b"fLaC\x00\x00\x00\x22"));
        assert!(!looks_like_markup(b"FORM\x00\x00AIFF"));
        assert!(!looks_like_markup(b"RIFF\x00\x00\x00\x00WAVE"));
        assert!(!looks_like_markup(b"ID3\x04\x00"));
        assert!(!looks_like_markup(&[0xff, 0xfb, 0x90, 0x00]));
    }

    #[test]
    fn audio_shorter_than_the_peek_buffer_still_writes() {
        // A stream that ends inside the 64-byte peek must not be truncated or
        // rejected.
        let dir = tmp();
        let target = dir.join("tiny.flac");
        let mut src = std::io::Cursor::new(b"fLaC short".to_vec());
        let n = write_audio_atomically(&target, &mut src).unwrap();
        assert_eq!(n, 10);
        assert_eq!(std::fs::read(&target).unwrap(), b"fLaC short");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_peeked_bytes_are_not_lost_from_a_longer_stream() {
        // The head is read before the decision, so it has to be put back.
        let dir = tmp();
        let target = dir.join("big.flac");
        let mut body = b"fLaC".to_vec();
        body.extend(std::iter::repeat_n(b'A', 500));
        let mut src = std::io::Cursor::new(body.clone());
        let n = write_audio_atomically(&target, &mut src).unwrap();
        assert_eq!(n as usize, body.len());
        assert_eq!(std::fs::read(&target).unwrap(), body);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_empty_audio_download_is_an_error() {
        let dir = tmp();
        let target = dir.join("empty.flac");
        let mut src = std::io::Cursor::new(Vec::new());
        assert!(write_audio_atomically(&target, &mut src).is_err());
        assert!(!target.exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_empty_download_is_an_error_and_writes_nothing() {
        // A zero-byte file would import into rekordbox and analyse to nonsense.
        let dir = tmp();
        let target = dir.join("out.flac");
        let mut src = std::io::Cursor::new(Vec::new());
        assert!(write_atomically(&target, &mut src).is_err());
        assert!(!target.exists(), "nothing should be left behind");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_failed_read_leaves_no_partial_file() {
        struct Failing(usize);
        impl Read for Failing {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.0 == 0 {
                    return Err(std::io::Error::other("connection reset"));
                }
                self.0 -= 1;
                buf[..4].copy_from_slice(b"data");
                Ok(4)
            }
        }
        let dir = tmp();
        let target = dir.join("out.flac");
        assert!(write_atomically(&target, &mut Failing(2)).is_err());
        assert!(!target.exists(), "a truncated file must not survive");
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            0,
            "and neither must its .part"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
