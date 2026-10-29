//! The macOS binding: `Session` exposed to Swift through swift-bridge.
//!
//! Forwarding only. Anything another frontend would need too belongs in
//! `rekord_ripper::session`, not here.
//!
//! swift-bridge cannot pass a `Vec` of shared structs, so lists are `Vec`s of
//! opaque views with getters, and id lists cross as comma-separated strings.

#![allow(clippy::unnecessary_cast)]

use rekord_ripper::acquire::ItemRef;
use rekord_ripper::acquire::shop::{GroupOutcome, RankedOffer};
use rekord_ripper::analysis::CueMark;
use rekord_ripper::db::SafetyOpts;
use rekord_ripper::format::Origin;
use rekord_ripper::library::TrackRow;
use rekord_ripper::pending::Entry;
use rekord_ripper::playlists::PlaylistNode;
use rekord_ripper::session::collections::{Collection, Direction, Scope, Suggestion};
use rekord_ripper::session::filter::{dst_visible, src_visible};
use rekord_ripper::session::review::{Change, PlanReview};
use rekord_ripper::session::shop::{
    BackendAnswer, FetchState, OfferFetch, ShopState, ShopTrackState,
};
use rekord_ripper::session::view::{self, Tone};
use rekord_ripper::session::{ConfirmKind, Notice, NoticeAction, Session, StatusLevel};
use rekord_ripper::settings::{Credential, CredentialState, Field, Kind};

/// Matches the TUI's tolerance for the fuzzy-from-source filter.
const DURATION_TOL_SECS: i64 = 1;

#[swift_bridge::bridge]
mod ffi {
    #[swift_bridge(swift_repr = "struct")]
    struct StatusFfi {
        text: String,
        // 0 info, 1 warn, 2 error, 3 ok.
        level: u8,
    }

    #[swift_bridge(swift_repr = "struct")]
    struct ShopStatusFfi {
        // 0 idle, 1 searching, 2 results, 3 failed.
        kind: u8,
        // What is being searched for, or why it failed.
        text: String,
        done: u32,
        total: u32,
        // The offer worth looking at: each newly arrived block, or the one a
        // search for an already-answered track points to.
        offer_cursor: u32,
        searches_outstanding: u32,
        elapsed_secs: u64,
    }

    #[swift_bridge(swift_repr = "struct")]
    struct FetchStatusFfi {
        // 0 idle, 1 running, 2 done, 3 failed.
        kind: u8,
        // The download, the saved files, or why it failed.
        text: String,
        // The backend's last word on a running download.
        note: String,
        // Transfer queued for a finished download, or -1.
        queued_entry: i64,
        elapsed_secs: u64,
    }

    #[swift_bridge(swift_repr = "struct")]
    struct ConfirmFfi {
        // 0 none, 1 transfer, 2 import rows, 3 force apply, 4 clear queue.
        kind: u8,
        title: String,
        accept: String,
    }

    extern "Rust" {
        type CoreSession;

        #[swift_bridge(associated_to = CoreSession)]
        fn open() -> Result<CoreSession, String>;

        // Rekordbox status and worker results. Call on every UI tick.
        fn poll(&mut self);
        fn rekordbox_running(&self) -> bool;
        fn bypass_rekordbox_check(&self) -> bool;
        fn set_bypass_rekordbox_check(&mut self, on: bool);
        fn status(&self) -> StatusFfi;
        fn unresolved_errors(&self) -> bool;
        // Work a quit would throw away, or empty.
        fn work_in_flight(&self) -> String;

        fn rows_revision(&self) -> u64;
        fn rows(&self) -> Vec<TrackRowView>;
        fn reload_rows(&mut self);
        // Indices into `rows()` that pass the source filter.
        fn src_visible(&self, query: &str) -> Vec<u32>;
        fn dst_visible(&self, query: &str, auto_mode: bool, src_id: &str, fuzzy: bool) -> Vec<u32>;
        fn pair_preview(&self, src_id: &str, dst_id: &str) -> Vec<String>;

        fn copy_replace(&self) -> bool;
        fn copy_lock(&self) -> bool;
        fn set_copy_opts(&mut self, replace: bool, lock: bool);
        fn build_pending(&mut self, src_id: &str, dst_ids: &str);

        fn confirm(&self) -> ConfirmFfi;
        fn confirm_lines(&self) -> Vec<ToneLine>;
        // True when the writes were attempted, rather than refused.
        fn accept_confirm(&mut self) -> bool;
        fn cancel_confirm(&mut self);

        fn shop_status(&self) -> ShopStatusFfi;
        fn offers(&self) -> Vec<OfferView>;
        // -2 untouched, -1 queued, otherwise the number of offers found.
        fn shop_track_state(&self, id: &str) -> i64;
        fn shop_track(&mut self, id: &str) -> bool;
        fn shop_basket(&mut self, ids: &str) -> bool;
        fn research(&mut self, id: &str) -> bool;
        fn start_fetch(&mut self, src_id: &str, item_ref: &str, fallback_src: &str) -> bool;
        fn open_offer(&mut self, src_id: &str, item_ref: &str);
        fn fetch_status(&self) -> FetchStatusFfi;
        // Every download handed to the worker, the running one first.
        fn fetch_queue(&self) -> Vec<String>;

        fn reload_queue(&mut self);
        fn queue_entries(&self) -> Vec<QueueEntryView>;
        fn start_import(&mut self);
        fn start_apply(&mut self);
        fn retry_entry(&mut self, id: i64);
        fn forget_entry(&mut self, id: i64);
        fn start_force(&mut self, id: i64);
        fn start_clear_queue(&mut self);

        // Library + inspector.
        fn playlists(&self) -> Vec<PlaylistView>;
        fn collections(&self) -> Vec<CollectionView>;
        // Indices into `rows()`: a collection (`CollectionView::code`), or a
        // playlist in its own order, narrowed by `query`.
        fn collection_visible(&self, code: u8, query: &str) -> Vec<u32>;
        fn playlist_visible(&self, id: &str, query: &str) -> Vec<u32>;
        fn suggestions(&self, id: &str) -> Vec<SuggestionView>;
        // What each backend answered for the track's latest search.
        fn shop_answers(&self, src_id: &str) -> Vec<BackendAnswerView>;
        // The top-ranked offer for the track, or none.
        fn best_offer(&self, src_id: &str) -> Vec<OfferView>;
        fn entries_for(&self, src_id: &str) -> Vec<QueueEntryView>;
        fn cue_marks(&self, id: &str) -> Vec<CueMarkView>;

        // Notices after `after`, oldest first.
        fn notices(&self, after: u64) -> Vec<NoticeView>;
        // The newest notice, which may have been reworded since it was read.
        fn latest_notice(&self) -> Vec<NoticeView>;
        // Takes the notice's step. A review step is the frontend's to show:
        // this returns its confirm kind (as `confirm`), otherwise 0.
        fn perform_notice(&mut self, id: u64) -> u8;

        // Open confirm kinds, oldest first.
        fn open_confirms(&self) -> Vec<u8>;
        fn confirm_for(&self, kind: u8) -> ConfirmFfi;
        fn confirm_lines_for(&self, kind: u8) -> Vec<ToneLine>;
        // What a transfer or force-apply confirmation would write, per track.
        fn plan_reviews(&self, kind: u8) -> Vec<PlanReviewView>;
        fn accept(&mut self, kind: u8) -> bool;
        fn cancel(&mut self, kind: u8);

        // The new entry's id, or -1.
        fn queue_file(&mut self, src_id: &str, path: &str) -> i64;
        fn set_auto_advance(&mut self, on: bool);
        fn config_path(&self) -> String;
        // Bumped whenever the settings change, by an edit or in the file.
        fn config_revision(&self) -> u64;
        fn settings(&self) -> Vec<SettingView>;
        // Empty when written, otherwise why not.
        fn set_setting(&mut self, key: &str, value: &str) -> String;
        fn credentials(&self) -> Vec<CredentialView>;
        // Empty value removes it. Empty when written, otherwise why not.
        fn set_credential(&mut self, key: &str, value: &str) -> String;
    }

    extern "Rust" {
        type TrackRowView;
        fn id(&self) -> String;
        fn title(&self) -> String;
        fn artist(&self) -> String;
        // Hundredths of a beat per minute.
        fn bpm(&self) -> Option<i64>;
        fn length_secs(&self) -> Option<i64>;
        fn cue_count(&self) -> i64;
        fn locked(&self) -> bool;
        fn file_type(&self) -> String;
        fn uuid(&self) -> String;
        fn key(&self) -> String;
        // Absolute path to the cover art, or empty.
        fn artwork_path(&self) -> String;
        // 0 local, 1 cloud, 2 stream.
        fn origin(&self) -> u8;
        // 1 lossless, 0 lossy, -1 neither (a stream, or unreadable).
        fn lossless(&self) -> i8;
        // 1 on disk, 0 missing, -1 cannot tell.
        fn present(&self) -> i8;
    }

    extern "Rust" {
        type PlaylistView;
        fn id(&self) -> String;
        fn name(&self) -> String;
        // Empty at the top level.
        fn parent(&self) -> String;
        fn folder(&self) -> bool;
        fn smart(&self) -> bool;
    }

    extern "Rust" {
        type CollectionView;
        fn code(&self) -> u8;
        fn title(&self) -> String;
        fn count(&self) -> u32;
    }

    extern "Rust" {
        type SettingView;
        // section.key, as in config.toml.
        fn key(&self) -> String;
        fn group(&self) -> String;
        fn label(&self) -> String;
        fn help(&self) -> String;
        // 0 bool, 1 integer, 2 number, 3 text, 4 path, 5 comma-separated list,
        // 6 choice.
        fn kind(&self) -> u8;
        // A choice's values and their labels, in order; empty otherwise.
        fn choices(&self) -> Vec<String>;
        fn choice_labels(&self) -> Vec<String>;
        // Bounds of a number; 0 otherwise.
        fn min(&self) -> f64;
        fn max(&self) -> f64;
        fn value(&self) -> String;
    }

    extern "Rust" {
        type CredentialView;
        fn key(&self) -> String;
        fn group(&self) -> String;
        fn label(&self) -> String;
        fn secret(&self) -> bool;
        fn set(&self) -> bool;
        // An env var or file is used instead of what is saved.
        fn overridden(&self) -> bool;
        fn note(&self) -> String;
        // Empty for a secret.
        fn value(&self) -> String;
    }

    extern "Rust" {
        type BackendAnswerView;
        fn backend(&self) -> String;
        fn offers(&self) -> u32;
        // Why the backend failed; empty when it answered.
        fn error(&self) -> String;
    }

    extern "Rust" {
        type SuggestionView;
        fn id(&self) -> String;
        // True when analysis would go from the selected track to this one.
        fn outgoing(&self) -> bool;
    }

    extern "Rust" {
        type CueMarkView;
        fn ms(&self) -> i64;
        // Loop end, or -1.
        fn end_ms(&self) -> i64;
        // 0 memory cue, otherwise a hot cue slot.
        fn kind(&self) -> i64;
    }

    extern "Rust" {
        type PlanReviewView;
        fn src_id(&self) -> String;
        fn src_title(&self) -> String;
        fn src_artist(&self) -> String;
        fn dst_id(&self) -> String;
        fn dst_title(&self) -> String;
        fn dst_artist(&self) -> String;
        fn changes(&self) -> Vec<ChangeView>;
        fn warnings(&self) -> Vec<String>;
        // The length once written, or -1.
        fn length_ms(&self) -> i64;
        fn cues_before(&self) -> Vec<CueMarkView>;
        fn cues_after(&self) -> Vec<CueMarkView>;
    }

    extern "Rust" {
        type ChangeView;
        fn label(&self) -> String;
        fn before(&self) -> String;
        fn after(&self) -> String;
    }

    extern "Rust" {
        type NoticeView;
        fn id(&self) -> u64;
        // As `StatusFfi::level`.
        fn level(&self) -> u8;
        fn text(&self) -> String;
        // 0 none, 1 open buy page, 2 search again, 3 fill basket, 4 next basket
        // batch, 5 import, 6 check, 7 force, 8 forget, 9 review, 10 allow
        // writes while rekordbox runs.
        fn action(&self) -> u8;
    }

    extern "Rust" {
        type OfferView;
        fn src_id(&self) -> String;
        fn group_label(&self) -> String;
        fn item_ref(&self) -> String;
        fn backend(&self) -> String;
        fn artist(&self) -> String;
        fn title(&self) -> String;
        fn album(&self) -> String;
        fn formats(&self) -> String;
        fn price(&self) -> String;
        fn ownership(&self) -> String;
        fn url(&self) -> String;
        fn note(&self) -> String;
        // 1 lossless, 0 not, -1 formats not checked.
        fn lossless(&self) -> i8;
        fn requires_purchase(&self) -> bool;
        // 0 none, 1 queued, 2 running, 3 done.
        fn fetch(&self) -> u8;
        fn match_score(&self) -> i64;
    }

    extern "Rust" {
        type QueueEntryView;
        fn id(&self) -> i64;
        fn title(&self) -> String;
        fn artist(&self) -> String;
        fn file_name(&self) -> String;
        fn state(&self) -> String;
        fn tone(&self) -> u8;
        fn in_flight(&self) -> bool;
        fn src_id(&self) -> String;
        // The step that moves it along, as `NoticeView::action`.
        fn action(&self) -> u8;
    }

    extern "Rust" {
        type ToneLine;
        // 0 plain, 1 strong, 2 active, 3 warn, 4 good, 5 bad, 6 muted.
        fn tone(&self) -> u8;
        fn text(&self) -> String;
    }
}

use ffi::{ConfirmFfi, FetchStatusFfi, ShopStatusFfi, StatusFfi};

pub struct CoreSession(Session);

pub struct TrackRowView {
    row: TrackRow,
    artwork: Option<std::path::PathBuf>,
}

pub struct OfferView {
    src_id: Option<String>,
    group_label: String,
    offer: RankedOffer,
    fetch: OfferFetch,
}

pub struct QueueEntryView {
    entry: Entry,
    state: String,
    tone: Tone,
    action: Option<NoticeAction>,
    in_flight: bool,
}

pub struct ToneLine(Tone, String);

pub struct PlaylistView(PlaylistNode);

pub struct CollectionView(Collection, usize);

pub struct SettingView(&'static Field, String);

pub struct CredentialView(&'static Credential, CredentialState);

pub struct SuggestionView(Suggestion);

pub struct BackendAnswerView(BackendAnswer);

pub struct CueMarkView(CueMark);

pub struct NoticeView(Notice);

pub struct PlanReviewView(PlanReview);

pub struct ChangeView(Change);

/// Codes in the order of `Collection::ALL`.
fn collection_code(code: u8) -> Option<Collection> {
    Collection::ALL.get(code as usize).copied()
}

fn confirm_code(kind: ConfirmKind) -> u8 {
    match kind {
        ConfirmKind::Transfer => 1,
        ConfirmKind::ImportRows => 2,
        ConfirmKind::ForceApply => 3,
        ConfirmKind::ClearQueue => 4,
    }
}

fn confirm_kind(code: u8) -> Option<ConfirmKind> {
    Some(match code {
        1 => ConfirmKind::Transfer,
        2 => ConfirmKind::ImportRows,
        3 => ConfirmKind::ForceApply,
        4 => ConfirmKind::ClearQueue,
        _ => return None,
    })
}

fn level_code(level: StatusLevel) -> u8 {
    match level {
        StatusLevel::Info => 0,
        StatusLevel::Warn => 1,
        StatusLevel::Err => 2,
        StatusLevel::Ok => 3,
    }
}

fn action_code(action: Option<&NoticeAction>) -> u8 {
    match action {
        None => 0,
        Some(NoticeAction::OpenBuyPage { .. }) => 1,
        Some(NoticeAction::Research { .. }) => 2,
        Some(NoticeAction::FillBasket) => 3,
        Some(NoticeAction::NextBasketBatch { .. }) => 4,
        Some(NoticeAction::ImportDownloads) => 5,
        Some(NoticeAction::CheckDownloads) => 6,
        Some(NoticeAction::ForceEntry { .. }) => 7,
        Some(NoticeAction::ForgetEntry { .. }) => 8,
        Some(NoticeAction::Review(_)) => 9,
        Some(NoticeAction::AllowWritesWhileRunning) => 10,
    }
}

fn indices(v: Vec<usize>) -> Vec<u32> {
    v.into_iter().map(|i| i as u32).collect()
}

fn tone_code(t: Tone) -> u8 {
    match t {
        Tone::Plain => 0,
        Tone::Strong => 1,
        Tone::Active => 2,
        Tone::Warn => 3,
        Tone::Good => 4,
        Tone::Bad => 5,
        Tone::Muted => 6,
    }
}

fn ids(csv: &str) -> impl Iterator<Item = String> + '_ {
    csv.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn confirm_ffi(view: Option<view::ConfirmView>) -> ConfirmFfi {
    match view {
        Some(v) => ConfirmFfi {
            kind: confirm_code(v.kind),
            title: v.title,
            accept: v.accept,
        },
        None => ConfirmFfi {
            kind: 0,
            title: String::new(),
            accept: String::new(),
        },
    }
}

fn non_empty(s: &str) -> Option<&str> {
    Some(s).filter(|s| !s.is_empty())
}

impl CoreSession {
    fn open() -> Result<CoreSession, String> {
        // Downloads rip into a scratch dir that a quit mid-gate leaves behind.
        let _ = rekord_ripper::fingerprint::ScratchDir::sweep_stale(
            std::time::Duration::from_secs(24 * 3600),
        );
        Session::open(SafetyOpts::default())
            .map(CoreSession)
            .map_err(|e| format!("{e:#}"))
    }

    fn poll(&mut self) {
        self.0.poll_if_due();
        self.0.pump_worker();
    }

    fn rekordbox_running(&self) -> bool {
        self.0.rb_running
    }

    fn bypass_rekordbox_check(&self) -> bool {
        self.0.safety.bypass_rekordbox_check
    }

    fn set_bypass_rekordbox_check(&mut self, on: bool) {
        self.0.safety.bypass_rekordbox_check = on;
    }

    fn status(&self) -> StatusFfi {
        StatusFfi {
            text: self.0.status.text.clone(),
            level: level_code(self.0.status.level),
        }
    }

    fn unresolved_errors(&self) -> bool {
        self.0.unresolved_errors
    }

    fn work_in_flight(&self) -> String {
        self.0
            .work_in_flight()
            .into_iter()
            .map(|(kind, n)| format!("{n} {}(s) still running", kind.label()))
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn rows_revision(&self) -> u64 {
        self.0.rows_revision
    }

    fn rows(&self) -> Vec<TrackRowView> {
        self.0
            .rows
            .iter()
            .map(|r| TrackRowView {
                artwork: r
                    .artwork
                    .as_deref()
                    .map(|p| self.0.db.resolve_analysis_path(p)),
                row: r.clone(),
            })
            .collect()
    }

    fn reload_rows(&mut self) {
        match self.0.reload_rows() {
            Ok(()) => self
                .0
                .status
                .ok(format!("Reloaded {} rows.", self.0.rows.len())),
            Err(e) => self.0.status.err(format!("reload failed: {e}")),
        }
    }

    fn src_visible(&self, query: &str) -> Vec<u32> {
        src_visible(&self.0.rows, query)
            .into_iter()
            .map(|i| i as u32)
            .collect()
    }

    fn dst_visible(&self, query: &str, auto_mode: bool, src_id: &str, fuzzy: bool) -> Vec<u32> {
        dst_visible(
            &self.0.rows,
            query,
            auto_mode,
            self.0.row(src_id),
            fuzzy,
            DURATION_TOL_SECS,
        )
        .into_iter()
        .map(|i| i as u32)
        .collect()
    }

    fn pair_preview(&self, src_id: &str, dst_id: &str) -> Vec<String> {
        rekord_ripper::session::diff::render_pair(
            self.0.row(src_id),
            self.0.row(dst_id),
            self.0.copy_opts,
        )
    }

    fn copy_replace(&self) -> bool {
        self.0.copy_opts.replace
    }

    fn copy_lock(&self) -> bool {
        self.0.copy_opts.lock
    }

    fn set_copy_opts(&mut self, replace: bool, lock: bool) {
        self.0.copy_opts.replace = replace;
        self.0.copy_opts.lock = lock;
    }

    fn build_pending(&mut self, src_id: &str, dst_ids: &str) {
        self.0.build_pending(src_id, ids(dst_ids).collect());
    }

    fn confirm(&self) -> ConfirmFfi {
        confirm_ffi(self.0.confirm_view())
    }

    fn confirm_lines(&self) -> Vec<ToneLine> {
        self.0
            .confirm_view()
            .map(|v| v.lines.into_iter().map(|(t, s)| ToneLine(t, s)).collect())
            .unwrap_or_default()
    }

    fn accept_confirm(&mut self) -> bool {
        self.0.confirm().is_some_and(|kind| self.0.accept(kind))
    }

    fn cancel_confirm(&mut self) {
        if let Some(kind) = self.0.confirm() {
            self.0.cancel(kind);
        }
    }

    fn shop_status(&self) -> ShopStatusFfi {
        let elapsed_secs = self
            .0
            .shop_since()
            .map_or(0, |since| since.elapsed().as_secs());
        let searches_outstanding = self.0.shop_outstanding() as u32;
        let (kind, text, done, total, offer_cursor) = match &self.0.shop {
            ShopState::Idle => (0, String::new(), 0, 0, 0),
            ShopState::Searching {
                what, done, total, ..
            } => (1, what.clone(), *done as u32, *total as u32, 0),
            ShopState::Results { cursor, .. } => (2, String::new(), 0, 0, *cursor as u32),
            ShopState::Failed(why) => (3, why.clone(), 0, 0, 0),
        };
        ShopStatusFfi {
            kind,
            text,
            done,
            total,
            offer_cursor,
            searches_outstanding,
            elapsed_secs,
        }
    }

    fn offers(&self) -> Vec<OfferView> {
        self.0
            .shop
            .flattened()
            .map(|(g, r): (&GroupOutcome, &RankedOffer)| OfferView {
                src_id: g.src_id.clone(),
                group_label: g.label.clone(),
                offer: r.clone(),
                fetch: self.0.offer_fetch(&r.offer.item_ref),
            })
            .collect()
    }

    fn shop_track_state(&self, id: &str) -> i64 {
        match self.0.shop_track_state(id) {
            ShopTrackState::Untouched => -2,
            ShopTrackState::Queued => -1,
            ShopTrackState::Done(n) => n as i64,
        }
    }

    fn shop_track(&mut self, id: &str) -> bool {
        self.0.shop_track(id)
    }

    fn shop_basket(&mut self, ids_csv: &str) -> bool {
        self.0.shop_basket(&ids(ids_csv).collect())
    }

    fn research(&mut self, id: &str) -> bool {
        self.0.research(non_empty(id))
    }

    fn start_fetch(&mut self, src_id: &str, item_ref: &str, fallback_src: &str) -> bool {
        let Ok(item) = item_ref.parse::<ItemRef>() else {
            self.0.status.warn("no offer selected.");
            return false;
        };
        self.0.start_fetch(
            non_empty(src_id),
            &item,
            non_empty(fallback_src).map(str::to_string),
        )
    }

    fn open_offer(&mut self, src_id: &str, item_ref: &str) {
        let offer = item_ref
            .parse::<ItemRef>()
            .ok()
            .and_then(|item| self.0.shop.find(non_empty(src_id), &item))
            .map(|(_, r)| r.offer.clone());
        match offer {
            Some(offer) => self.0.open_offer(&offer),
            None => self.0.status.warn("no offer selected."),
        }
    }

    fn fetch_status(&self) -> FetchStatusFfi {
        let mut out = FetchStatusFfi {
            kind: 0,
            text: String::new(),
            note: String::new(),
            queued_entry: -1,
            elapsed_secs: 0,
        };
        match &self.0.fetch {
            FetchState::Idle => {}
            FetchState::Running { since, what, note } => {
                out.kind = 1;
                out.text = what.clone();
                out.note = note.clone().unwrap_or_default();
                out.elapsed_secs = since.elapsed().as_secs();
            }
            FetchState::Done { paths, queued } => {
                out.kind = 2;
                out.text = paths
                    .iter()
                    .filter_map(|p| p.file_name())
                    .map(|n| n.to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("\n");
                out.queued_entry = queued.unwrap_or(-1);
            }
            FetchState::Failed(why) => {
                out.kind = 3;
                out.text = why.clone();
            }
        }
        out
    }

    fn fetch_queue(&self) -> Vec<String> {
        self.0
            .fetch_queue
            .iter()
            .map(|(what, _)| what.to_string())
            .collect()
    }

    fn reload_queue(&mut self) {
        self.0.reload_queue();
    }

    fn queue_entries(&self) -> Vec<QueueEntryView> {
        self.0
            .queue
            .entries
            .iter()
            .map(|e| self.entry_view(e))
            .collect()
    }

    fn entry_view(&self, e: &Entry) -> QueueEntryView {
        let (state, tone, action) = view::entry_state(&self.0, e);
        QueueEntryView {
            entry: e.clone(),
            state,
            tone,
            action,
            in_flight: self.0.queue.in_flight(e.id),
        }
    }

    fn start_import(&mut self) {
        self.0.start_import();
    }

    fn start_apply(&mut self) {
        self.0.start_apply();
    }

    fn retry_entry(&mut self, id: i64) {
        self.0.retry_entry(id);
    }

    fn forget_entry(&mut self, id: i64) {
        self.0.forget_entry(id);
    }

    fn start_force(&mut self, id: i64) {
        self.0.start_force(id);
    }

    fn start_clear_queue(&mut self) {
        self.0.start_clear_queue();
    }

    fn playlists(&self) -> Vec<PlaylistView> {
        match rekord_ripper::playlists::tree(&self.0.db) {
            Ok(nodes) => nodes.into_iter().map(PlaylistView).collect(),
            Err(_) => Vec::new(),
        }
    }

    fn collections(&self) -> Vec<CollectionView> {
        self.0
            .collection_counts()
            .into_iter()
            .map(|(c, n)| CollectionView(c, n))
            .collect()
    }

    fn collection_visible(&self, code: u8, query: &str) -> Vec<u32> {
        let Some(c) = collection_code(code) else {
            return Vec::new();
        };
        indices(self.0.scoped_visible(&Scope::Collection(c), query))
    }

    fn playlist_visible(&self, id: &str, query: &str) -> Vec<u32> {
        indices(
            self.0
                .scoped_visible(&Scope::Playlist(id.to_string()), query),
        )
    }

    fn shop_answers(&self, src_id: &str) -> Vec<BackendAnswerView> {
        self.0
            .shop_answers(src_id)
            .into_iter()
            .map(BackendAnswerView)
            .collect()
    }
    fn suggestions(&self, id: &str) -> Vec<SuggestionView> {
        self.0
            .suggestions(id)
            .into_iter()
            .map(SuggestionView)
            .collect()
    }

    fn best_offer(&self, src_id: &str) -> Vec<OfferView> {
        self.0
            .best_offer(src_id)
            .map(|r| OfferView {
                src_id: Some(src_id.to_string()),
                group_label: String::new(),
                offer: r.clone(),
                fetch: self.0.offer_fetch(&r.offer.item_ref),
            })
            .into_iter()
            .collect()
    }

    fn entries_for(&self, src_id: &str) -> Vec<QueueEntryView> {
        self.0
            .entries_for(src_id)
            .map(|e| self.entry_view(e))
            .collect()
    }

    fn cue_marks(&self, id: &str) -> Vec<CueMarkView> {
        rekord_ripper::analysis::cue_marks(&self.0.db, id)
            .unwrap_or_default()
            .into_iter()
            .map(CueMarkView)
            .collect()
    }

    fn notices(&self, after: u64) -> Vec<NoticeView> {
        self.0
            .status
            .since(after)
            .cloned()
            .map(NoticeView)
            .collect()
    }

    fn latest_notice(&self) -> Vec<NoticeView> {
        self.0
            .status
            .since(0)
            .last()
            .cloned()
            .map(NoticeView)
            .into_iter()
            .collect()
    }

    fn perform_notice(&mut self, id: u64) -> u8 {
        let Some(action) = self
            .0
            .status
            .since(0)
            .find(|n| n.id == id)
            .and_then(|n| n.action.clone())
        else {
            return 0;
        };
        match action {
            NoticeAction::OpenBuyPage { src_id, item } => {
                let offer = self
                    .0
                    .shop
                    .find(src_id.as_deref(), &item)
                    .map(|(_, r)| r.offer.clone());
                match offer {
                    Some(offer) => self.0.open_offer(&offer),
                    None => self.0.status.warn("that offer is no longer listed."),
                }
            }
            NoticeAction::Research { src_id } => {
                self.0.research(Some(&src_id));
            }
            NoticeAction::ImportDownloads => self.0.start_import(),
            NoticeAction::CheckDownloads => self.0.start_apply(),
            NoticeAction::ForceEntry { entry_id } => self.0.start_force(entry_id),
            NoticeAction::ForgetEntry { entry_id } => self.0.forget_entry(entry_id),
            NoticeAction::Review(kind) => return confirm_code(kind),
            // The app takes these itself: it searches its selection rather
            // than a basket, and asks before allowing writes.
            NoticeAction::FillBasket
            | NoticeAction::NextBasketBatch { .. }
            | NoticeAction::AllowWritesWhileRunning => {}
        }
        0
    }

    fn open_confirms(&self) -> Vec<u8> {
        self.0.confirmations.kinds().map(confirm_code).collect()
    }

    fn confirm_for(&self, kind: u8) -> ConfirmFfi {
        confirm_ffi(confirm_kind(kind).and_then(|k| self.0.confirm_view_for(k)))
    }

    fn confirm_lines_for(&self, kind: u8) -> Vec<ToneLine> {
        confirm_kind(kind)
            .and_then(|k| self.0.confirm_view_for(k))
            .map(|v| v.lines.into_iter().map(|(t, s)| ToneLine(t, s)).collect())
            .unwrap_or_default()
    }

    fn plan_reviews(&self, kind: u8) -> Vec<PlanReviewView> {
        confirm_kind(kind)
            .map(|k| self.0.plan_reviews(k))
            .unwrap_or_default()
            .into_iter()
            .map(PlanReviewView)
            .collect()
    }

    fn accept(&mut self, kind: u8) -> bool {
        confirm_kind(kind).is_some_and(|k| self.0.accept(k))
    }

    fn cancel(&mut self, kind: u8) {
        if let Some(k) = confirm_kind(kind) {
            self.0.cancel(k);
        }
    }

    fn queue_file(&mut self, src_id: &str, path: &str) -> i64 {
        self.0
            .queue_file(src_id, std::path::Path::new(path))
            .unwrap_or(-1)
    }

    fn set_auto_advance(&mut self, on: bool) {
        self.0.set_auto_advance(on);
    }

    fn config_path(&self) -> String {
        self.0.config_path().to_string_lossy().into_owned()
    }

    fn config_revision(&self) -> u64 {
        self.0.config_revision
    }

    fn settings(&self) -> Vec<SettingView> {
        self.0.settings().map(|(f, v)| SettingView(f, v)).collect()
    }

    fn set_setting(&mut self, key: &str, value: &str) -> String {
        match self.0.set_setting(key, value) {
            Ok(()) => String::new(),
            Err(e) => format!("{e:#}"),
        }
    }

    fn credentials(&self) -> Vec<CredentialView> {
        self.0
            .credentials()
            .map(|(c, state)| CredentialView(c, state))
            .collect()
    }

    fn set_credential(&mut self, key: &str, value: &str) -> String {
        match self.0.set_credential(key, value) {
            Ok(()) => String::new(),
            Err(e) => format!("{e:#}"),
        }
    }
}

impl TrackRowView {
    fn id(&self) -> String {
        self.row.id.clone()
    }
    fn title(&self) -> String {
        self.row.title.clone()
    }
    fn artist(&self) -> String {
        self.row.artist.clone()
    }
    fn bpm(&self) -> Option<i64> {
        self.row.bpm
    }
    fn length_secs(&self) -> Option<i64> {
        self.row.length
    }
    fn cue_count(&self) -> i64 {
        self.row.cue_count
    }
    fn locked(&self) -> bool {
        self.row.locked
    }
    fn file_type(&self) -> String {
        rekord_ripper::format::file_type_name(self.row.file_type).to_string()
    }
    fn uuid(&self) -> String {
        self.row.uuid.clone()
    }
    fn key(&self) -> String {
        self.row.key.clone()
    }
    fn artwork_path(&self) -> String {
        self.artwork
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
    fn origin(&self) -> u8 {
        match self.row.origin {
            Origin::Local => 0,
            Origin::Cloud => 1,
            Origin::Stream => 2,
        }
    }
    fn lossless(&self) -> i8 {
        match self.row.lossless() {
            Some(true) => 1,
            Some(false) => 0,
            None => -1,
        }
    }
    fn present(&self) -> i8 {
        match self.row.present {
            Some(true) => 1,
            Some(false) => 0,
            None => -1,
        }
    }
}

impl PlaylistView {
    fn id(&self) -> String {
        self.0.id.clone()
    }
    fn name(&self) -> String {
        self.0.name.clone()
    }
    fn parent(&self) -> String {
        self.0.parent.clone().unwrap_or_default()
    }
    fn folder(&self) -> bool {
        self.0.folder
    }
    fn smart(&self) -> bool {
        self.0.smart
    }
}

impl CollectionView {
    fn code(&self) -> u8 {
        Collection::ALL
            .iter()
            .position(|&c| c == self.0)
            .unwrap_or(0) as u8
    }
    fn title(&self) -> String {
        self.0.title().to_string()
    }
    fn count(&self) -> u32 {
        self.1 as u32
    }
}

impl SettingView {
    fn key(&self) -> String {
        self.0.key.into()
    }
    fn group(&self) -> String {
        self.0.group.into()
    }
    fn label(&self) -> String {
        self.0.label.into()
    }
    fn help(&self) -> String {
        self.0.help.into()
    }
    fn kind(&self) -> u8 {
        match self.0.kind {
            Kind::Bool => 0,
            Kind::Int { .. } => 1,
            Kind::Float { .. } => 2,
            Kind::Text => 3,
            Kind::Path => 4,
            Kind::List => 5,
            Kind::Choice(_) => 6,
        }
    }
    fn choices(&self) -> Vec<String> {
        self.choice_list().map(|(v, _)| v.to_string()).collect()
    }
    fn choice_labels(&self) -> Vec<String> {
        self.choice_list().map(|(_, l)| l.to_string()).collect()
    }
    fn choice_list(&self) -> impl Iterator<Item = &'static (&'static str, &'static str)> {
        match self.0.kind {
            Kind::Choice(c) => c.iter(),
            _ => [].iter(),
        }
    }
    fn min(&self) -> f64 {
        match self.0.kind {
            Kind::Int { min, .. } => min as f64,
            Kind::Float { min, .. } => min,
            _ => 0.0,
        }
    }
    fn max(&self) -> f64 {
        match self.0.kind {
            Kind::Int { max, .. } => max as f64,
            Kind::Float { max, .. } => max,
            _ => 0.0,
        }
    }
    fn value(&self) -> String {
        self.1.clone()
    }
}

impl CredentialView {
    fn key(&self) -> String {
        self.0.key.into()
    }
    fn group(&self) -> String {
        self.0.group.into()
    }
    fn label(&self) -> String {
        self.0.label.into()
    }
    fn secret(&self) -> bool {
        self.0.secret
    }
    fn set(&self) -> bool {
        self.1.set
    }
    fn overridden(&self) -> bool {
        self.1.overridden
    }
    fn note(&self) -> String {
        self.1.note.clone()
    }
    fn value(&self) -> String {
        self.1.value.clone()
    }
}

impl BackendAnswerView {
    fn backend(&self) -> String {
        self.0.backend.label().into()
    }
    fn offers(&self) -> u32 {
        self.0.offers as u32
    }
    fn error(&self) -> String {
        self.0.error.clone().unwrap_or_default()
    }
}

impl SuggestionView {
    fn id(&self) -> String {
        self.0.id.clone()
    }
    fn outgoing(&self) -> bool {
        self.0.direction == Direction::To
    }
}

impl CueMarkView {
    fn ms(&self) -> i64 {
        self.0.ms
    }
    fn end_ms(&self) -> i64 {
        self.0.end_ms.unwrap_or(-1)
    }
    fn kind(&self) -> i64 {
        self.0.kind
    }
}

impl PlanReviewView {
    fn src_id(&self) -> String {
        self.0.src_id.clone()
    }
    fn src_title(&self) -> String {
        self.0.src_title.clone()
    }
    fn src_artist(&self) -> String {
        self.0.src_artist.clone()
    }
    fn dst_id(&self) -> String {
        self.0.dst_id.clone()
    }
    fn dst_title(&self) -> String {
        self.0.dst_title.clone()
    }
    fn dst_artist(&self) -> String {
        self.0.dst_artist.clone()
    }
    fn changes(&self) -> Vec<ChangeView> {
        self.0.changes.iter().cloned().map(ChangeView).collect()
    }
    fn warnings(&self) -> Vec<String> {
        self.0.warnings.clone()
    }
    fn length_ms(&self) -> i64 {
        self.0.length_ms.unwrap_or(-1)
    }
    fn cues_before(&self) -> Vec<CueMarkView> {
        self.0
            .cues_before
            .iter()
            .copied()
            .map(CueMarkView)
            .collect()
    }
    fn cues_after(&self) -> Vec<CueMarkView> {
        self.0.cues_after.iter().copied().map(CueMarkView).collect()
    }
}

impl ChangeView {
    fn label(&self) -> String {
        self.0.label.to_string()
    }
    fn before(&self) -> String {
        self.0.before.clone()
    }
    fn after(&self) -> String {
        self.0.after.clone()
    }
}

impl NoticeView {
    fn id(&self) -> u64 {
        self.0.id
    }
    fn level(&self) -> u8 {
        level_code(self.0.level)
    }
    fn text(&self) -> String {
        self.0.text.clone()
    }
    fn action(&self) -> u8 {
        action_code(self.0.action.as_ref())
    }
}

impl OfferView {
    fn src_id(&self) -> String {
        self.src_id.clone().unwrap_or_default()
    }
    fn group_label(&self) -> String {
        self.group_label.clone()
    }
    fn item_ref(&self) -> String {
        self.offer.offer.item_ref.to_string()
    }
    fn backend(&self) -> String {
        self.offer.offer.item_ref.backend.label().into()
    }
    fn artist(&self) -> String {
        self.offer.offer.artist.clone()
    }
    fn title(&self) -> String {
        self.offer.offer.title.clone()
    }
    fn album(&self) -> String {
        self.offer.offer.album.clone().unwrap_or_default()
    }
    fn formats(&self) -> String {
        rekord_ripper::acquire::render::format_cell(&self.offer.offer)
    }
    fn price(&self) -> String {
        rekord_ripper::acquire::render::price_cell(&self.offer.offer)
    }
    fn ownership(&self) -> String {
        rekord_ripper::acquire::render::ownership_cell(&self.offer.offer).to_string()
    }
    fn url(&self) -> String {
        self.offer.offer.url.clone()
    }
    fn note(&self) -> String {
        self.offer.offer.enrich_error.clone().unwrap_or_default()
    }
    fn lossless(&self) -> i8 {
        match self.offer.offer.has_lossless() {
            Some(true) => 1,
            Some(false) => 0,
            None => -1,
        }
    }
    fn requires_purchase(&self) -> bool {
        self.offer.offer.requires_purchase()
    }
    fn fetch(&self) -> u8 {
        match self.fetch {
            OfferFetch::None => 0,
            OfferFetch::Queued => 1,
            OfferFetch::Running => 2,
            OfferFetch::Done => 3,
        }
    }
    fn match_score(&self) -> i64 {
        self.offer.match_score as i64
    }
}

impl QueueEntryView {
    fn id(&self) -> i64 {
        self.entry.id
    }
    fn title(&self) -> String {
        self.entry.src_title.clone().unwrap_or_default()
    }
    fn artist(&self) -> String {
        self.entry.src_artist.clone().unwrap_or_default()
    }
    fn file_name(&self) -> String {
        self.entry
            .acquired_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
    fn state(&self) -> String {
        self.state.clone()
    }
    fn tone(&self) -> u8 {
        tone_code(self.tone)
    }
    fn in_flight(&self) -> bool {
        self.in_flight
    }
    fn src_id(&self) -> String {
        self.entry.src_content_id.clone()
    }
    fn action(&self) -> u8 {
        action_code(self.action.as_ref())
    }
}

impl ToneLine {
    fn tone(&self) -> u8 {
        tone_code(self.0)
    }
    fn text(&self) -> String {
        self.1.clone()
    }
}
