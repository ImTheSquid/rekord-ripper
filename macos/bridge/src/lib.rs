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
use rekord_ripper::db::SafetyOpts;
use rekord_ripper::library::TrackRow;
use rekord_ripper::pending::Entry;
use rekord_ripper::session::filter::{dst_visible, src_visible};
use rekord_ripper::session::shop::{FetchState, OfferFetch, ShopState, ShopTrackState};
use rekord_ripper::session::view::{self, Tone};
use rekord_ripper::session::{ConfirmKind, Session, StatusLevel};

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

pub struct TrackRowView(TrackRow);

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
    in_flight: bool,
}

pub struct ToneLine(Tone, String);

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
    csv.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

fn non_empty(s: &str) -> Option<&str> {
    Some(s).filter(|s| !s.is_empty())
}

impl CoreSession {
    fn open() -> Result<CoreSession, String> {
        // Downloads rip into a scratch dir that a quit mid-gate leaves behind.
        let _ = rekord_ripper::fingerprint::ScratchDir::sweep_stale(std::time::Duration::from_secs(
            24 * 3600,
        ));
        Session::open(SafetyOpts::default())
            .map(CoreSession)
            .map_err(|e| format!("{e:#}"))
    }

    fn poll(&mut self) {
        self.0.poll_rekordbox_if_due();
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
        let level = match self.0.status.level {
            StatusLevel::Info => 0,
            StatusLevel::Warn => 1,
            StatusLevel::Err => 2,
            StatusLevel::Ok => 3,
        };
        StatusFfi {
            text: self.0.status.text.clone(),
            level,
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
        self.0.rows.iter().cloned().map(TrackRowView).collect()
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
        match self.0.confirm_view() {
            Some(v) => ConfirmFfi {
                kind: match v.kind {
                    ConfirmKind::Transfer => 1,
                    ConfirmKind::ImportRows => 2,
                    ConfirmKind::ForceApply => 3,
                    ConfirmKind::ClearQueue => 4,
                },
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
            .map(|e| {
                let (state, tone, _) = view::entry_state(&self.0, e);
                QueueEntryView {
                    entry: e.clone(),
                    state,
                    tone,
                    in_flight: self.0.queue.in_flight(e.id),
                }
            })
            .collect()
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
}

impl TrackRowView {
    fn id(&self) -> String {
        self.0.id.clone()
    }
    fn title(&self) -> String {
        self.0.title.clone()
    }
    fn artist(&self) -> String {
        self.0.artist.clone()
    }
    fn bpm(&self) -> Option<i64> {
        self.0.bpm
    }
    fn length_secs(&self) -> Option<i64> {
        self.0.length
    }
    fn cue_count(&self) -> i64 {
        self.0.cue_count
    }
    fn locked(&self) -> bool {
        self.0.locked
    }
    fn file_type(&self) -> String {
        rekord_ripper::format::file_type_name(self.0.file_type).to_string()
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
        self.offer.offer.item_ref.backend.to_string()
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
}

impl ToneLine {
    fn tone(&self) -> u8 {
        tone_code(self.0)
    }
    fn text(&self) -> String {
        self.1.clone()
    }
}
