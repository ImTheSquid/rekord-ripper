//! Whether a track's audio is actually on this machine.
//!
//! Only a row whose `FolderPath` is a real filesystem path can be answered. A
//! cloud row's path is relative to a Cloud Library Sync root this tool cannot
//! locate, and a stream has no file at all, so both answer `None` rather than
//! guess — they get neither `present` nor `missing`.
//!
//! A path belonging to another machine — `C:/…` from a synced Windows library,
//! or somebody else's `/Users/…` — is deliberately not a special case. This
//! machine cannot open it, so the file is missing, which is exactly what the
//! question asks.

use std::path::Path;

use crate::format::Origin;

/// `Some(true)` present, `Some(false)` missing, `None` unanswerable.
pub(crate) fn check(origin: Origin, folder_path: Option<&str>) -> Option<bool> {
    match origin {
        // `exists()` reports false for an unreadable directory as well as an
        // absent file. Both mean the same thing here: this process cannot open
        // the audio.
        Origin::Local => Some(Path::new(folder_path?).exists()),
        Origin::Cloud | Origin::Stream => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_local_row_gets_an_answer() {
        assert_eq!(check(Origin::Cloud, Some("/contents_1/a.flac")), None);
        assert_eq!(check(Origin::Stream, Some("soundcloud:tracks:1")), None);
        assert_eq!(check(Origin::Local, None), None);
    }

    #[test]
    fn a_local_path_is_answered_from_the_filesystem() {
        // This source file is the one path a test can be sure about.
        let here = concat!(env!("CARGO_MANIFEST_DIR"), "/src/presence.rs");
        assert_eq!(check(Origin::Local, Some(here)), Some(true));
        assert_eq!(
            check(Origin::Local, Some("/nope/definitely/not/here.flac")),
            Some(false)
        );
    }

    #[test]
    fn another_machines_path_is_missing_not_an_error() {
        assert_eq!(
            check(Origin::Local, Some("C:/users/someone/Music/a.mp3")),
            Some(false)
        );
    }
}
