//! The slskd REST API, and the one thing it cannot do.
//!
//! slskd speaks plain JSON over HTTP, so this leans entirely on the shared
//! `http` helpers — including `with_retries`, which matters more than it looks:
//! slskd permits **one** concurrent search and one concurrent enqueue, answering
//! a second with 429, and `map_err` turns that into a retryable `RateLimited`.
//!
//! Three shapes of the wire format are worth knowing, all set in slskd's
//! `Program.cs:977-981`:
//!
//! * Property names are camelCase (ASP.NET's `JsonSerializerDefaults.Web`).
//! * Enums serialize as strings, including `[Flags]` ones — see [`TransferState`].
//! * Null properties are *omitted*, so every optional field needs a default.
//!
//! What it cannot do is hand over the bytes of a completed download: the files
//! API lists and deletes, and nothing else. Collecting the file is the caller's
//! problem — see the parent module.

use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::acquire::error::{BackendError, Result};
use crate::acquire::http;
use crate::acquire::types::BackendId;

const ID: BackendId = BackendId::Soulseek;

/// slskd rejects a search timeout below this.
pub const MIN_SEARCH_WINDOW_SECS: u64 = 5;

/// How often to ask whether a search has settled.
const SEARCH_POLL: Duration = Duration::from_millis(500);

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

/// One file in a peer's search response.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct File {
    pub filename: String,
    pub size: u64,
    /// Pre-parsed by slskd, so there is no need to pick the extension out of a
    /// Windows-shaped path ourselves.
    #[serde(default)]
    pub extension: Option<String>,
    #[serde(default)]
    pub bit_rate: Option<u32>,
    #[serde(default)]
    pub bit_depth: Option<u32>,
    #[serde(default)]
    pub sample_rate: Option<u32>,
    /// True for a VBR encode, which is the only honest way to spot LAME V0.
    #[serde(default)]
    pub is_variable_bit_rate: Option<bool>,
    /// Duration in seconds.
    #[serde(default)]
    pub length: Option<i64>,
    /// Behind the peer's sharing rules — visible but not gettable.
    #[serde(default)]
    pub is_locked: bool,
}

/// One peer's answer to a search.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Response {
    pub username: String,
    #[serde(default)]
    pub has_free_upload_slot: bool,
    #[serde(default)]
    pub upload_speed: u32,
    /// How many transfers are already waiting on this peer. The best single
    /// predictor of whether a download will ever actually start.
    #[serde(default)]
    pub queue_length: u64,
    #[serde(default)]
    pub files: Vec<File>,
    /// Reported separately by slskd; we never offer these.
    #[serde(default)]
    pub locked_files: Vec<File>,
}

/// A search as slskd sees it.
///
/// `is_complete` is the only reliable signal that peers have stopped answering.
/// Note `state` reaching `"Completed, TimedOut"` is the *normal* ending for a
/// search — the idle timer expiring is how a search finishes — so unlike a
/// transfer, TimedOut here is not a failure.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Search {
    #[serde(default)]
    pub is_complete: bool,
    #[serde(default)]
    pub response_count: u32,
    #[serde(default)]
    pub file_count: u32,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchRequest<'a> {
    pub id: String,
    pub search_text: &'a str,
    /// Seconds of quiet before slskd calls the search done. Minimum 5.
    pub search_timeout: u64,
    /// Stop after this many peers have answered. This, not the timeout, is what
    /// keeps a popular query from running long.
    pub response_limit: usize,
    pub file_limit: usize,
    /// Let slskd drop responses that fail its own filters before we see them.
    pub filter_responses: bool,
}

/// A queued or running transfer.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Transfer {
    pub id: String,
    pub username: String,
    pub filename: String,
    #[serde(default)]
    pub size: u64,
    pub state: TransferState,
    #[serde(default)]
    pub bytes_transferred: u64,
    #[serde(default)]
    pub average_speed: f64,
    #[serde(default)]
    pub place_in_queue: Option<u64>,
    #[serde(default)]
    pub exception: Option<String>,
}

/// slskd serializes `TransferStates` as a comma-joined flags string, e.g.
/// `"Completed, Succeeded"`.
///
/// Kept as the raw string with predicates over it rather than modelled as an
/// enum, for two reasons. A state we have never seen must not fail the whole
/// poll, and the flag combinations are the thing that carries the meaning:
/// `Completed` on its own says nothing about whether the file is good.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct TransferState(pub String);

impl TransferState {
    fn has(&self, flag: &str) -> bool {
        self.0
            .split(',')
            .any(|f| f.trim().eq_ignore_ascii_case(flag))
    }

    /// The file is on the daemon's disk and correct. **Only** this counts as a
    /// finished download — `Completed` pairs with five different failures
    /// (`TransferStateCategories.cs:69-75`), so matching `Completed` alone would
    /// treat a cancelled transfer as a success and import a partial file.
    pub fn succeeded(&self) -> bool {
        self.has("Completed") && self.has("Succeeded")
    }

    /// Finished, one way or another. Fails closed: a bare `Completed`, or one
    /// paired with a flag we do not know, counts as terminal-not-successful
    /// rather than something to keep waiting on.
    pub fn is_terminal(&self) -> bool {
        self.has("Completed")
    }

    pub fn failed(&self) -> bool {
        self.is_terminal() && !self.succeeded()
    }

    pub fn is_queued(&self) -> bool {
        self.has("Queued")
    }

    pub fn in_progress(&self) -> bool {
        self.has("InProgress")
    }

    pub fn raw(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchRequest<'a> {
    pub id: String,
    pub username: &'a str,
    pub files: Vec<BatchItem<'a>>,
    pub options: BatchOptions<'a>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchItem<'a> {
    pub filename: &'a str,
    pub size: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchOptions<'a> {
    /// Relative to slskd's download directory, and it takes precedence over
    /// slskd's own placement rules — which is what lets us stage each fetch in a
    /// directory we can name in advance.
    pub destination: &'a str,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchResponse {
    #[serde(default)]
    pub batch: Option<Batch>,
    #[serde(default)]
    pub failures: Vec<BatchFailure>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchFailure {
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Batch {
    pub id: String,
    #[serde(default)]
    pub transfers: Vec<Transfer>,
}

/// A directory in slskd's download tree.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FilesystemDirectory {
    #[serde(default)]
    pub files: Vec<FilesystemFile>,
    #[serde(default)]
    pub directories: Vec<FilesystemDirectory>,
}

impl FilesystemDirectory {
    /// Every file in this tree, flattened.
    pub fn walk(&self) -> Vec<&FilesystemFile> {
        let mut out: Vec<&FilesystemFile> = self.files.iter().collect();
        for d in &self.directories {
            out.extend(d.walk());
        }
        out
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FilesystemFile {
    pub name: String,
    /// Relative to whatever was listed — the leaf name when a subdirectory was
    /// listed, a path from the download root when the root was. Deliberately
    /// *not* used as a filesystem path: compose one from
    /// [`Directories::downloads`] instead.
    #[serde(default)]
    pub full_name: String,
    #[serde(default)]
    pub length: u64,
}

/// The directories slskd is configured with, from `GET /api/v0/options`.
///
/// Needed because a completed download's location is only knowable as
/// `downloads` + the destination we asked for + the name slskd gave it.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Options {
    #[serde(default)]
    pub directories: Directories,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Directories {
    #[serde(default)]
    pub downloads: String,
    #[serde(default)]
    pub incomplete: String,
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

pub struct Client {
    base: String,
    api_key: String,
    budget: Duration,
}

impl Client {
    /// `base` is the API root, e.g. `https://slskd.example.com:5030`.
    pub fn new(base: &str, api_key: &str, budget: Duration) -> Self {
        Self {
            base: base.trim().trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            budget,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}/api/v0/{path}", self.base)
    }

    /// Run a search to completion and return the peers that answered.
    ///
    /// Two requests because slskd's POST blocks until the search settles but
    /// does not promise the responses in its reply.
    pub fn search(&self, req: &SearchRequest<'_>, deadline: Instant) -> Result<Vec<Response>> {
        let url = self.url("searches");
        let agent = http::agent(self.budget);
        http::with_retries(3, || {
            agent
                .post(&url)
                .header("X-API-Key", &self.api_key)
                .send_json(req)
                .map_err(|e| http::map_err(ID, &url, e))?;
            Ok(())
        })?;

        // The POST only *registers* the search: it returns at once with
        // `state: InProgress` and an empty response list, so reading results
        // straight away reliably returns nothing. Peers answer over the next few
        // seconds and `isComplete` is what says they have stopped.
        while Instant::now() < deadline {
            std::thread::sleep(SEARCH_POLL);
            let s: Search = self.get_json(&format!("searches/{}", req.id), "search state")?;
            if s.is_complete {
                break;
            }
        }
        // Falling out of that loop on the deadline is fine — whatever arrived is
        // still worth returning.

        let url = self.url(&format!("searches/{}/responses", req.id));
        // A wide search answers with a lot of JSON, so this one is not held to
        // the 15s per-request ceiling.
        let agent = http::download_agent(self.budget);
        let body = http::with_retries(3, || {
            agent
                .get(&url)
                .header("X-API-Key", &self.api_key)
                .call()
                .map_err(|e| http::map_err(ID, &url, e))?
                .body_mut()
                .read_to_string()
                .map_err(|e| http::map_err(ID, &url, e))
        })?;
        serde_json::from_str(&body)
            .map_err(|e| BackendError::parse(ID, "search responses", e.to_string()))
    }

    /// Forget a search, so slskd's history is not filled with ours.
    pub fn forget_search(&self, id: &str) {
        let url = self.url(&format!("searches/{id}"));
        let agent = http::agent(self.budget);
        let _ = agent.delete(&url).header("X-API-Key", &self.api_key).call();
    }

    /// Enqueue a batch. A duplicate id answers 409, which the caller reads as
    /// "already enqueued, attach to it".
    pub fn enqueue(&self, req: &BatchRequest<'_>) -> Result<BatchResponse> {
        let url = self.url("transfers/downloads/batches");
        let agent = http::agent(self.budget);
        let body = http::with_retries(3, || {
            agent
                .post(&url)
                .header("X-API-Key", &self.api_key)
                .send_json(req)
                .map_err(|e| http::map_err(ID, &url, e))?
                .body_mut()
                .read_to_string()
                .map_err(|e| http::map_err(ID, &url, e))
        })?;
        serde_json::from_str(&body)
            .map_err(|e| BackendError::parse(ID, "enqueue response", e.to_string()))
    }

    pub fn batch(&self, id: &str) -> Result<Batch> {
        self.get_json(&format!("transfers/downloads/batches/{id}"), "batch")
    }

    /// slskd's running configuration, for the download directory.
    pub fn options(&self) -> Result<Options> {
        self.get_json("options", "options")
    }

    /// List slskd's download directory. `subdirectory` is relative to it.
    pub fn list_downloads(&self, subdirectory: &str) -> Result<FilesystemDirectory> {
        let path = if subdirectory.is_empty() {
            "files/downloads/directories?recursive=true".to_string()
        } else {
            format!(
                "files/downloads/directories/{}?recursive=true",
                encode_path_segment(&base64_standard(subdirectory.as_bytes()))
            )
        };
        self.get_json(&path, "download listing")
    }

    /// Delete a subdirectory of the download directory.
    ///
    /// Best effort by design: slskd answers 403 unless `remote_file_management`
    /// is enabled, and failing to tidy up is not worth failing a download over.
    pub fn delete_download_subdirectory(&self, subdirectory: &str) -> bool {
        let url = self.url(&format!(
            "files/downloads/directories/{}",
            encode_path_segment(&base64_standard(subdirectory.as_bytes()))
        ));
        let agent = http::agent(self.budget);
        agent
            .delete(&url)
            .header("X-API-Key", &self.api_key)
            .call()
            .is_ok()
    }

    fn get_json<T: DeserializeOwned>(&self, path: &str, what: &'static str) -> Result<T> {
        let url = self.url(path);
        let agent = http::agent(self.budget);
        let body = http::with_retries(3, || {
            agent
                .get(&url)
                .header("X-API-Key", &self.api_key)
                .call()
                .map_err(|e| http::map_err(ID, &url, e))?
                .body_mut()
                .read_to_string()
                .map_err(|e| http::map_err(ID, &url, e))
        })?;
        serde_json::from_str(&body).map_err(|e| BackendError::parse(ID, what, e.to_string()))
    }
}

/// True when this error is slskd saying "that id already exists".
pub fn is_conflict(e: &BackendError) -> bool {
    matches!(e, BackendError::Http { status: 409, .. })
}

// ---------------------------------------------------------------------------
// Encoding helpers
// ---------------------------------------------------------------------------

pub fn base64_standard(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Percent-encode everything outside the unreserved set.
///
/// Used for both a base64 route parameter (which contains `+`, `/` and `=`) and
/// for filenames on the way into a URL, where a Soulseek name brings spaces,
/// brackets, `#` and `?` with it.
pub fn encode_path_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*b as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// `Authorization: Basic …` for the file route, when it is protected.
pub fn basic_auth(user: &str, password: &str) -> String {
    format!(
        "Basic {}",
        base64_standard(format!("{user}:{password}").as_bytes())
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_completed_and_succeeded_is_a_finished_download() {
        let ok = TransferState("Completed, Succeeded".into());
        assert!(ok.succeeded());
        assert!(ok.is_terminal());
        assert!(!ok.failed());
    }

    #[test]
    fn every_other_completed_pairing_is_a_failure() {
        // The bug this guards: matching "Completed" alone would import a
        // cancelled or errored transfer as a finished file.
        for flag in ["Cancelled", "TimedOut", "Errored", "Rejected", "Aborted"] {
            let s = TransferState(format!("Completed, {flag}"));
            assert!(s.is_terminal(), "{flag} is terminal");
            assert!(s.failed(), "{flag} is a failure");
            assert!(!s.succeeded(), "{flag} must not read as success");
        }
    }

    #[test]
    fn an_unknown_or_bare_completed_state_fails_closed() {
        // A state slskd grows later must not be mistaken for success, and must
        // not spin the poll loop forever either.
        let bare = TransferState("Completed".into());
        assert!(bare.is_terminal());
        assert!(bare.failed());

        let future = TransferState("Completed, SomethingNew".into());
        assert!(future.is_terminal());
        assert!(!future.succeeded());
    }

    #[test]
    fn in_flight_states_are_not_terminal() {
        for raw in [
            "Requested",
            "Queued, Remotely",
            "Queued, Locally",
            "InProgress",
            "Initializing",
        ] {
            let s = TransferState(raw.into());
            assert!(!s.is_terminal(), "{raw} should not be terminal");
            assert!(!s.succeeded(), "{raw} should not be a success");
        }
        assert!(TransferState("Queued, Remotely".into()).is_queued());
        assert!(TransferState("InProgress".into()).in_progress());
    }

    #[test]
    fn flags_parse_regardless_of_spacing_and_case() {
        assert!(TransferState("completed,succeeded".into()).succeeded());
        assert!(TransferState("Completed ,  Succeeded".into()).succeeded());
    }

    #[test]
    fn files_deserialize_with_camel_case_and_omitted_nulls() {
        // slskd omits null properties rather than emitting null, so a sparse
        // object has to parse.
        let f: File = serde_json::from_str(
            r#"{"filename":"@@a\\b - c.mp3","size":100,"extension":".mp3",
                "bitRate":320,"isVariableBitRate":false,"length":210,"isLocked":false}"#,
        )
        .unwrap();
        assert_eq!(f.bit_rate, Some(320));
        assert_eq!(f.extension.as_deref(), Some(".mp3"));
        assert_eq!(f.length, Some(210));

        let sparse: File = serde_json::from_str(r#"{"filename":"x.flac","size":1}"#).unwrap();
        assert_eq!(sparse.bit_rate, None);
        assert_eq!(sparse.extension, None);
        assert!(!sparse.is_locked);
    }

    #[test]
    fn a_response_carries_the_peer_facts_used_for_ranking() {
        let r: Response = serde_json::from_str(
            r#"{"username":"peer","hasFreeUploadSlot":true,"uploadSpeed":900,
                "queueLength":7,"files":[],"lockedFiles":[]}"#,
        )
        .unwrap();
        assert!(r.has_free_upload_slot);
        assert_eq!(r.queue_length, 7);
        assert_eq!(r.upload_speed, 900);
    }

    #[test]
    fn a_transfer_deserializes_including_its_queue_position() {
        let t: Transfer = serde_json::from_str(
            r#"{"id":"abc","username":"peer","filename":"a\\b.flac","size":10,
                "state":"Queued, Remotely","bytesTransferred":0,"averageSpeed":0,
                "placeInQueue":12}"#,
        )
        .unwrap();
        assert_eq!(t.place_in_queue, Some(12));
        assert!(t.state.is_queued());
    }

    #[test]
    fn the_download_tree_flattens() {
        let d: FilesystemDirectory = serde_json::from_str(
            r#"{"files":[{"name":"a.flac","fullName":"/d/a.flac","length":1}],
                "directories":[{"files":[{"name":"b.flac","fullName":"/d/s/b.flac","length":2}],
                                "directories":[]}]}"#,
        )
        .unwrap();
        let all = d.walk();
        assert_eq!(all.len(), 2);
        assert!(all.iter().any(|f| f.name == "b.flac" && f.length == 2));
    }

    #[test]
    fn base64_matches_the_rfc_vectors() {
        // slskd decodes these route parameters with plain FromBase64.
        assert_eq!(base64_standard(b""), "");
        assert_eq!(base64_standard(b"f"), "Zg==");
        assert_eq!(base64_standard(b"fo"), "Zm8=");
        assert_eq!(base64_standard(b"foo"), "Zm9v");
        assert_eq!(base64_standard(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn path_segments_escape_everything_a_soulseek_name_can_contain() {
        assert_eq!(
            encode_path_segment("plain-file_1.flac"),
            "plain-file_1.flac"
        );
        assert_eq!(encode_path_segment("with space"), "with%20space");
        assert_eq!(encode_path_segment("a/b"), "a%2Fb");
        assert_eq!(encode_path_segment("q?x=1&y"), "q%3Fx%3D1%26y");
        assert_eq!(encode_path_segment("#hash"), "%23hash");
        assert_eq!(encode_path_segment("[1992]"), "%5B1992%5D");
        assert_eq!(encode_path_segment("it's"), "it%27s");
        // Base64 padding and its two non-alphanumeric characters.
        assert_eq!(encode_path_segment("Zm8+/A=="), "Zm8%2B%2FA%3D%3D");
        // Multi-byte, encoded per UTF-8 byte.
        assert_eq!(encode_path_segment("é"), "%C3%A9");
    }

    #[test]
    fn basic_auth_is_a_base64_user_colon_password() {
        // The RFC 7617 example.
        assert_eq!(
            basic_auth("Aladdin", "open sesame"),
            "Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ=="
        );
    }

    #[test]
    fn a_conflict_is_recognised_as_the_attach_signal() {
        assert!(is_conflict(&BackendError::Http {
            backend: ID,
            status: 409,
            url: String::new(),
        }));
        assert!(!is_conflict(&BackendError::Http {
            backend: ID,
            status: 404,
            url: String::new(),
        }));
    }

    #[test]
    fn urls_join_without_doubling_slashes() {
        let c = Client::new("https://host:5030/", "k", Duration::from_secs(5));
        assert_eq!(c.url("searches"), "https://host:5030/api/v0/searches");
    }
}
