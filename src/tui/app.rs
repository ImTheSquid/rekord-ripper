//! The TUI's view of a [`Session`]: cursors, focus, filters and selections.
//!
//! Everything here is about what is on screen. What a key *does* is a session
//! call, with the cursor turned into an id first.

use std::collections::HashSet;

use crate::db::{MasterDb, SafetyOpts};
use crate::library::TrackRow;
use crate::session::Session;
use crate::session::filter::{dst_visible, src_visible};

pub use crate::session::queue::{ForceBatch, ImportBatch};
pub use crate::session::shop::{FetchState, OfferFetch, ShopState, ShopTrackState};
pub use crate::session::transfer::PendingBatch;
pub use crate::session::{ConfirmKind, StatusLevel};

pub const DURATION_TOL_SECS: i64 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Src,
    Dst,
}

/// The two full screens.
///
/// The shop used to be an overlay on the transfer view, which forced `Space` to
/// mean two unrelated things at once — "copy target" in the DESTINATIONS column
/// and "shop for this" in SOURCES — and left "select some sources, then move to
/// destinations" with no meaning at all. Each screen now owns its own selection,
/// and each selection means exactly one thing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    Transfer,
    Shop,
    /// Downloads that have not become transfers yet.
    Pending,
}

/// Which pane of the shop screen the keys go to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShopFocus {
    Tracks,
    Offers,
}

/// What the keys mean when no confirmation is open. An open confirmation
/// ([`Session::confirm`]) takes the keys before any of these.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputMode {
    Normal,
    Search(Focus),
    Help,
    /// Typing into the shop screen's track filter.
    ShopSearch,
}

/// Selected rows the column's filter is currently hiding.
///
/// Worth surfacing: a selection you cannot see reads as one the program dropped,
/// even though it is still in the batch.
fn hidden_selected(rows: &[TrackRow], col: &ColumnState) -> usize {
    let shown = col
        .visible
        .iter()
        .filter_map(|&i| rows.get(i))
        .filter(|r| col.selected.contains(&r.id))
        .count();
    col.selected.len().saturating_sub(shown)
}

#[derive(Clone, Copy, Default, Debug)]
pub struct DstFilters {
    pub auto: bool,
    pub fuzzy_from_src: bool,
}

#[derive(Clone, Debug, Default)]
pub struct ColumnState {
    pub query: String,
    pub visible: Vec<usize>,
    pub cursor: usize,
    pub selected: HashSet<String>,
}

impl ColumnState {
    pub fn clamp_cursor(&mut self) {
        if self.visible.is_empty() {
            self.cursor = 0;
        } else if self.cursor >= self.visible.len() {
            self.cursor = self.visible.len() - 1;
        }
    }
    pub fn move_by(&mut self, delta: isize) {
        if self.visible.is_empty() {
            self.cursor = 0;
            return;
        }
        let n = self.visible.len() as isize;
        let mut c = self.cursor as isize + delta;
        if c < 0 {
            c = 0;
        }
        if c >= n {
            c = n - 1;
        }
        self.cursor = c as usize;
    }
    pub fn jump_top(&mut self) {
        self.cursor = 0;
    }
    pub fn jump_bottom(&mut self) {
        if !self.visible.is_empty() {
            self.cursor = self.visible.len() - 1;
        }
    }
}

pub struct App {
    pub session: Session,

    pub src: ColumnState,
    pub dst: ColumnState,
    pub focus: Focus,
    pub screen: Screen,
    /// The screen `p` was pressed on, so Esc goes back there rather than always
    /// to the transfer screen.
    queue_return: Screen,
    pub shop_focus: ShopFocus,
    /// The shop screen's own track list: its own filter, and a selection that
    /// only ever means "search for these".
    pub shop_list: ColumnState,
    pub mode: InputMode,
    pub dst_filters: DstFilters,

    /// First visible line of the open confirmation. Clamped during render, the
    /// only place the modal's height is known.
    pub confirm_scroll: u16,
    /// First visible line of the help popup. Clamped during render, which is
    /// the only place the popup's height is known.
    pub help_scroll: u16,
    /// Set true on the first `q` press when there's unsaved selection state.
    /// Reset by any other key. A second `q` while this is true actually quits.
    pub quit_pending: bool,
    pub should_quit: bool,
}

impl App {
    pub fn new(db: MasterDb, safety: SafetyOpts) -> anyhow::Result<Self> {
        let mut app = App {
            session: Session::new(db, safety)?,
            src: ColumnState::default(),
            dst: ColumnState::default(),
            focus: Focus::Src,
            screen: Screen::Transfer,
            queue_return: Screen::Transfer,
            shop_focus: ShopFocus::Tracks,
            shop_list: ColumnState::default(),
            mode: InputMode::Normal,
            dst_filters: DstFilters::default(),
            confirm_scroll: 0,
            help_scroll: 0,
            quit_pending: false,
            should_quit: false,
        };
        app.recompute_visible();
        Ok(app)
    }

    pub fn recompute_visible(&mut self) {
        let rows = &self.session.rows;
        self.src.visible = src_visible(rows, &self.src.query);
        self.src.clamp_cursor();

        // The shop list filters the same rows independently, so moving around on
        // one screen never disturbs the other.
        self.shop_list.visible = src_visible(rows, &self.shop_list.query);
        self.shop_list.clamp_cursor();

        // Hand the current src to dst_visible so it always gets excluded from
        // the dst list (you can't copy a track onto itself); the fuzzy flag is
        // a separate axis that further narrows by normalized title + length.
        let src = self.src.visible.get(self.src.cursor).and_then(|&i| rows.get(i));
        self.dst.visible = dst_visible(
            rows,
            &self.dst.query,
            self.dst_filters.auto,
            src,
            self.dst_filters.fuzzy_from_src,
            DURATION_TOL_SECS,
        );
        self.dst.clamp_cursor();
    }

    pub fn reload_db(&mut self) -> anyhow::Result<()> {
        self.session.reload_rows()?;
        self.prune_selections();
        self.recompute_visible();
        Ok(())
    }

    /// Drop selections that no longer correspond to existing rows.
    fn prune_selections(&mut self) {
        let existing: HashSet<&str> = self.session.rows.iter().map(|r| r.id.as_str()).collect();
        self.dst
            .selected
            .retain(|id| existing.contains(id.as_str()));
        self.shop_list
            .selected
            .retain(|id| existing.contains(id.as_str()));
    }

    pub fn poll_rekordbox_if_due(&mut self) {
        self.session.poll_rekordbox_if_due();
    }

    pub fn pump_worker(&mut self) {
        self.session.pump_worker();
    }

    /// Plan the transfer batch, and ask.
    pub fn build_pending(&mut self) {
        let src_id = match self.current_src() {
            Some(r) => r.id.clone(),
            None => {
                self.session.status.err("no source selected");
                return;
            }
        };

        // Destinations: explicit multi-selection, or cursor row if selection empty.
        let mut dst_ids: Vec<String> = self.dst.selected.iter().cloned().collect();
        if dst_ids.is_empty()
            && let Some(r) = self.current_dst()
            && r.id != src_id
        {
            dst_ids.push(r.id.clone());
        }
        self.confirm_scroll = 0;
        self.session.build_pending(&src_id, dst_ids);
    }

    /// Write the batch, and drop the selection it was built from.
    pub fn apply_pending(&mut self) {
        if self.session.apply_pending() {
            self.dst.selected.clear();
            self.prune_selections();
            self.recompute_visible();
        }
    }

    /// What `s` on the transfer screen does: cross to the shop screen, landing on
    /// the track that was highlighted, and search for it.
    pub fn open_shop(&mut self) -> bool {
        self.screen = Screen::Shop;
        let Some(row) = self.current_src().cloned() else {
            self.shop_focus = ShopFocus::Tracks;
            self.session
                .status
                .info("pick a track and press 's' to search for it.");
            return false;
        };
        self.focus_shop_list_on(&row.id);
        self.shop_track()
    }

    /// Search for the shop screen's highlighted track, or show what is already
    /// known about it.
    ///
    /// Focus stays where it is — stealing it to the offers pane is what made the
    /// next `s` look like a dead key.
    pub fn shop_track(&mut self) -> bool {
        let id = self
            .current_shop_track()
            .map(|r| r.id.clone())
            .unwrap_or_default();
        self.session.shop_track(&id)
    }

    /// Put the shop list's cursor on `id`.
    ///
    /// A filter hiding the row loses to the seeded cursor: arriving from the
    /// transfer screen on some unrelated track would be worse than a cleared
    /// filter.
    fn focus_shop_list_on(&mut self, id: &str) {
        if let Some(p) = self.position_in_shop_list(id) {
            self.shop_list.cursor = p;
            return;
        }
        self.shop_list.query.clear();
        self.recompute_visible();
        if let Some(p) = self.position_in_shop_list(id) {
            self.shop_list.cursor = p;
        }
    }

    fn position_in_shop_list(&self, id: &str) -> Option<usize> {
        self.shop_list
            .visible
            .iter()
            .position(|&i| self.session.rows.get(i).is_some_and(|r| r.id == id))
    }

    pub fn toggle_shop_focus(&mut self) {
        self.shop_focus = match self.shop_focus {
            ShopFocus::Tracks => ShopFocus::Offers,
            ShopFocus::Offers => ShopFocus::Tracks,
        };
    }

    pub fn shop_move(&mut self, delta: isize) {
        match self.shop_focus {
            ShopFocus::Tracks => self.shop_list.move_by(delta),
            // Deliberately does not move the track cursor. It used to follow the
            // offer cursor, which meant `s` acted on whichever track the offers
            // happened to belong to rather than the one the user had highlighted
            // — so from the offers pane `s` always landed on an already-searched
            // track and did nothing visible.
            ShopFocus::Offers => self.session.shop.move_cursor(delta),
        }
    }

    pub fn shop_jump(&mut self, top: bool) {
        match (self.shop_focus, top) {
            (ShopFocus::Tracks, true) => self.shop_list.jump_top(),
            (ShopFocus::Tracks, false) => self.shop_list.jump_bottom(),
            (ShopFocus::Offers, true) => self.shop_move(isize::MIN / 2),
            (ShopFocus::Offers, false) => self.shop_move(isize::MAX / 2),
        }
    }

    /// Add or remove the highlighted track from the basket.
    pub fn toggle_basket(&mut self) {
        if self.shop_focus != ShopFocus::Tracks {
            self.session
                .status
                .info("space fills the basket — Tab back to the track list.");
            return;
        }
        let Some(id) = self.current_shop_track().map(|r| r.id.clone()) else {
            return;
        };
        if !self.shop_list.selected.remove(&id) {
            self.shop_list.selected.insert(id);
        }
    }

    /// Search for everything in the basket.
    pub fn shop_selected(&mut self) -> bool {
        self.session.shop_basket(&self.shop_list.selected)
    }

    /// Re-run one search, replacing just that track's results.
    ///
    /// On the offers pane that means the group the highlighted offer belongs to,
    /// which after a queue of searches is not necessarily the track under the
    /// list cursor.
    pub fn start_shop(&mut self) -> bool {
        let id = match self.shop_focus {
            ShopFocus::Offers => self
                .session
                .shop_offer_src()
                .map(str::to_string)
                .or_else(|| self.current_shop_track().map(|r| r.id.clone())),
            ShopFocus::Tracks => self.current_shop_track().map(|r| r.id.clone()),
        };
        self.session.research(id.as_deref())
    }

    /// Download the highlighted offer.
    pub fn start_fetch(&mut self) -> bool {
        let Some((src, item)) = self
            .session
            .shop
            .selected_with_group()
            .map(|(g, r)| (g.src_id.clone(), r.offer.item_ref.clone()))
        else {
            self.session.status.warn("no offer selected.");
            return false;
        };
        let fallback = self.current_shop_track().map(|r| r.id.clone());
        self.session.start_fetch(src.as_deref(), &item, fallback)
    }

    /// Basket items the current filter is hiding.
    pub fn basket_hidden(&self) -> usize {
        hidden_selected(&self.session.rows, &self.shop_list)
    }

    /// Selected destinations the current filters are hiding.
    ///
    /// `fuzzy_from_src` narrows the destination list from the *source* cursor, so
    /// simply moving around on the left can hide rows that are still selected and
    /// still in the apply batch.
    pub fn dst_hidden(&self) -> usize {
        hidden_selected(&self.session.rows, &self.dst)
    }

    /// Open the queue screen, retiring anything that can no longer progress.
    pub fn open_queue(&mut self) {
        if self.screen != Screen::Pending {
            self.queue_return = self.screen;
        }
        self.screen = Screen::Pending;
        self.session.reload_queue();
    }

    /// Leave the queue for whichever screen opened it.
    pub fn close_queue(&mut self) {
        self.screen = self.queue_return;
    }

    fn selected_entry(&self) -> Option<i64> {
        self.session.queue.selected().map(|e| e.id)
    }

    pub fn retry_selected(&mut self) {
        if let Some(id) = self.selected_entry() {
            self.session.retry_entry(id);
        }
    }

    pub fn forget_selected(&mut self) {
        if let Some(id) = self.selected_entry() {
            self.session.forget_entry(id);
        }
    }

    pub fn start_force(&mut self) {
        if let Some(id) = self.selected_entry() {
            self.confirm_scroll = 0;
            self.session.start_force(id);
        }
    }

    /// Scroll whichever confirmation is open.
    pub fn scroll_confirm(&mut self, delta: i32) {
        self.confirm_scroll = (self.confirm_scroll as i32 + delta).max(0) as u16;
    }

    pub fn focused_column_mut(&mut self) -> &mut ColumnState {
        match self.focus {
            Focus::Src => &mut self.src,
            Focus::Dst => &mut self.dst,
        }
    }
    pub fn focused_column(&self) -> &ColumnState {
        match self.focus {
            Focus::Src => &self.src,
            Focus::Dst => &self.dst,
        }
    }

    pub fn current_src(&self) -> Option<&TrackRow> {
        self.src
            .visible
            .get(self.src.cursor)
            .and_then(|&i| self.session.rows.get(i))
    }
    pub fn current_dst(&self) -> Option<&TrackRow> {
        self.dst
            .visible
            .get(self.dst.cursor)
            .and_then(|&i| self.session.rows.get(i))
    }
    pub fn current_shop_track(&self) -> Option<&TrackRow> {
        self.shop_list
            .visible
            .get(self.shop_list.cursor)
            .and_then(|&i| self.session.rows.get(i))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn basket(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    fn library() -> Vec<TrackRow> {
        vec![
            TrackRow::stub("1", "alpha"),
            TrackRow::stub("2", "beta"),
            TrackRow::stub("3", "gamma"),
            TrackRow::stub("4", "delta"),
        ]
    }

    #[test]
    fn a_filter_hiding_a_selection_is_counted_not_lost() {
        let rows = library();
        let mut col = ColumnState {
            selected: basket(&["1", "4"]),
            ..ColumnState::default()
        };
        // Everything visible: nothing hidden.
        col.visible = vec![0, 1, 2, 3];
        assert_eq!(hidden_selected(&rows, &col), 0);
        // A filter matching only "alpha" hides the other pick.
        col.visible = vec![0];
        assert_eq!(hidden_selected(&rows, &col), 1);
        // A filter matching neither hides both, and the selection still stands.
        col.visible = vec![1, 2];
        assert_eq!(hidden_selected(&rows, &col), 2);
        assert_eq!(col.selected.len(), 2, "counting must not mutate anything");
    }
}
