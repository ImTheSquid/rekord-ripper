use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::app::{App, ConfirmKind, Focus, InputMode, Screen, ShopFocus};

pub fn handle_key(app: &mut App, key: KeyEvent) {
    // Ignore key-release / repeat events from crossterm's KittyKeyboard-style
    // enhanced events. Only process Press.
    if !matches!(
        key.kind,
        crossterm::event::KeyEventKind::Press | crossterm::event::KeyEventKind::Repeat
    ) {
        return;
    }

    // Any key that isn't 'q' or Esc clears the "press q again to quit" arming.
    let is_quit_key = matches!(key.code, KeyCode::Char('q') | KeyCode::Esc);
    if !is_quit_key {
        app.quit_pending = false;
    }

    // An open confirmation takes the keys whatever the mode, the way its modal
    // covers whatever was on screen.
    if let Some(kind) = app.session.confirm() {
        handle_confirm(app, kind, key);
        return;
    }

    let mode = app.mode.clone();
    match mode {
        InputMode::Normal => match app.screen {
            Screen::Transfer => handle_normal(app, key),
            Screen::Shop => handle_shop(app, key),
            Screen::Pending => handle_pending(app, key),
        },
        InputMode::Search(focus) => handle_search(app, key, focus),
        InputMode::ShopSearch => handle_shop_search(app, key),
        InputMode::Help => handle_help(app, key),
    }
}

/// The queue of downloads that have not become transfers yet.
fn handle_pending(app: &mut App, key: KeyEvent) {
    match (key.code, key.modifiers) {
        // Esc leaves the screen rather than quitting, matching the shop screen,
        // and returns to whichever screen `p` was pressed on.
        (KeyCode::Esc, _) | (KeyCode::Char('q'), _) => {
            app.close_queue();
        }
        (KeyCode::Char('?'), _) => app.mode = InputMode::Help,
        (KeyCode::Up, _) | (KeyCode::Char('k'), _) => app.session.queue.move_by(-1),
        (KeyCode::Down, _) | (KeyCode::Char('j'), _) => app.session.queue.move_by(1),
        (KeyCode::PageUp, _) => app.session.queue.move_by(-10),
        (KeyCode::PageDown, _) => app.session.queue.move_by(10),
        (KeyCode::Char('g'), _) => app.session.queue.jump_top(),
        (KeyCode::Char('G'), _) => app.session.queue.jump_bottom(),
        (KeyCode::Char('R'), _) => app.session.reload_queue(),
        (KeyCode::Char('i'), _) => app.session.start_import(),
        (KeyCode::Char('a'), _) => app.session.start_apply(),
        (KeyCode::Char('r'), _) => app.retry_selected(),
        (KeyCode::Char('c'), _) => app.forget_selected(),
        (KeyCode::Char('F'), _) => app.start_force(),
        (KeyCode::Char('C'), _) => app.session.start_clear_queue(),
        _ => {}
    }
}

/// The help popup is longer than most terminals, so it scrolls.
///
/// It used to close on any key at all. That cannot survive scrolling — `j` has
/// to mean "down" — so closing is now the explicit keys, and the popup's title
/// says which.
fn handle_help(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') | KeyCode::Char('?') => {
            app.mode = InputMode::Normal;
            // Reopening starts at the top rather than wherever you left off.
            app.help_scroll = 0;
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.help_scroll = app.help_scroll.saturating_sub(1);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.help_scroll = app.help_scroll.saturating_add(1);
        }
        KeyCode::PageUp => app.help_scroll = app.help_scroll.saturating_sub(10),
        KeyCode::PageDown => app.help_scroll = app.help_scroll.saturating_add(10),
        KeyCode::Home | KeyCode::Char('g') => app.help_scroll = 0,
        // Render clamps this to the real last page.
        KeyCode::End | KeyCode::Char('G') => app.help_scroll = u16::MAX,
        _ => {}
    }
}

/// Return true if quitting now would discard work the user might want.
fn has_pending_work(app: &App) -> bool {
    // Any kind of job, not just searches: quitting mid-fingerprint throws away
    // minutes of decoding with nothing to show for it.
    !app.dst.selected.is_empty() || app.session.unresolved_errors || !app.session.work_in_flight().is_empty()
}

fn try_quit(app: &mut App) {
    if app.quit_pending || !has_pending_work(app) {
        app.should_quit = true;
        return;
    }
    app.quit_pending = true;
    let mut bits = Vec::new();
    if !app.dst.selected.is_empty() {
        bits.push(format!(
            "{} destination(s) selected",
            app.dst.selected.len()
        ));
    }
    if app.session.unresolved_errors {
        bits.push("unresolved apply errors".into());
    }
    // Named per kind: a fingerprint reported as a search is a lie, and this is
    // the one message standing between the user and losing minutes of work.
    for (kind, n) in app.session.work_in_flight() {
        bits.push(format!("{n} {}(s) still running", kind.label()));
    }
    app.session.status.warn(format!(
        "{}. Press 'q' again to confirm quit.",
        bits.join(", ")
    ));
}

fn handle_normal(app: &mut App, key: KeyEvent) {
    match (key.code, key.modifiers) {
        (KeyCode::Tab, _) | (KeyCode::BackTab, _) => {
            app.focus = match app.focus {
                Focus::Src => Focus::Dst,
                Focus::Dst => Focus::Src,
            };
        }
        (KeyCode::Char('q'), _) | (KeyCode::Esc, _) => try_quit(app),
        (KeyCode::Char('?'), _) => {
            app.mode = InputMode::Help;
        }
        (KeyCode::Up, _) | (KeyCode::Char('k'), _) => {
            app.focused_column_mut().move_by(-1);
            // Fuzzy-from-src depends on the src cursor.
            if matches!(app.focus, Focus::Src) {
                // Moving the src cursor changes which row is hidden from dst
                // (the exclude-self predicate) — also nudges the fuzzy match.
                app.recompute_visible();
            }
        }
        (KeyCode::Down, _) | (KeyCode::Char('j'), _) => {
            app.focused_column_mut().move_by(1);
            if matches!(app.focus, Focus::Src) {
                // Moving the src cursor changes which row is hidden from dst
                // (the exclude-self predicate) — also nudges the fuzzy match.
                app.recompute_visible();
            }
        }
        (KeyCode::PageUp, _) => app.focused_column_mut().move_by(-10),
        (KeyCode::PageDown, _) => app.focused_column_mut().move_by(10),
        (KeyCode::Char('g'), _) => app.focused_column_mut().jump_top(),
        (KeyCode::Char('G'), _) => app.focused_column_mut().jump_bottom(),
        (KeyCode::Char('/'), _) => {
            app.mode = InputMode::Search(app.focus);
        }
        // Cross to the shop screen, landing on this track.
        (KeyCode::Char('s'), _) => {
            app.open_shop();
        }
        (KeyCode::Char('p'), _) => {
            app.open_queue();
        }
        // Selection here means one thing only: the copy targets. A source is
        // always the highlighted row, so there is nothing to select on that side.
        (KeyCode::Char(' '), _) => {
            if !matches!(app.focus, Focus::Dst) {
                app.session.status.info(
                    "the source is the highlighted row — 's' shops for it, Tab picks destinations.",
                );
                return;
            }
            let id = app
                .dst
                .visible
                .get(app.dst.cursor)
                .and_then(|&i| app.session.rows.get(i))
                .map(|r| r.id.clone());
            if let Some(id) = id
                && !app.dst.selected.remove(&id)
            {
                app.dst.selected.insert(id);
            }
        }
        (KeyCode::Char('c'), _) => {
            app.dst.selected.clear();
        }
        (KeyCode::Char('a'), _) => {
            app.dst_filters.auto = !app.dst_filters.auto;
            app.recompute_visible();
        }
        (KeyCode::Char('f'), _) => {
            app.dst_filters.fuzzy_from_src = !app.dst_filters.fuzzy_from_src;
            app.recompute_visible();
        }
        (KeyCode::Char('r'), _) => {
            app.session.copy_opts.replace = !app.session.copy_opts.replace;
            app.session.status.info(format!(
                "replace = {}",
                if app.session.copy_opts.replace { "ON" } else { "off" }
            ));
        }
        (KeyCode::Char('l'), _) => {
            app.session.copy_opts.lock = !app.session.copy_opts.lock;
            app.session.status.info(format!(
                "lock = {}",
                if app.session.copy_opts.lock { "ON" } else { "off" }
            ));
        }
        (KeyCode::Char('R'), _) => match app.reload_db() {
            Ok(()) => app.session.status.ok(format!("Reloaded {} rows.", app.session.rows.len())),
            Err(e) => app.session.status.err(format!("reload failed: {e}")),
        },
        (KeyCode::Enter, _) => app.build_pending(),
        _ => {}
    }
}

fn handle_search(app: &mut App, key: KeyEvent, focus: Focus) {
    match (key.code, key.modifiers) {
        (KeyCode::Esc, _) | (KeyCode::Enter, _) => {
            app.mode = InputMode::Normal;
        }
        (KeyCode::Backspace, _) => {
            match focus {
                Focus::Src => {
                    app.src.query.pop();
                }
                Focus::Dst => {
                    app.dst.query.pop();
                }
            }
            app.recompute_visible();
        }
        (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
            match focus {
                Focus::Src => app.src.query.clear(),
                Focus::Dst => app.dst.query.clear(),
            }
            app.recompute_visible();
        }
        (KeyCode::Char(c), m) if !m.contains(KeyModifiers::CONTROL) => {
            match focus {
                Focus::Src => app.src.query.push(c),
                Focus::Dst => app.dst.query.push(c),
            }
            app.recompute_visible();
        }
        (KeyCode::Up, _) => {
            match focus {
                Focus::Src => app.src.move_by(-1),
                Focus::Dst => app.dst.move_by(-1),
            }
            if matches!(focus, Focus::Src) {
                app.recompute_visible();
            }
        }
        (KeyCode::Down, _) => {
            match focus {
                Focus::Src => app.src.move_by(1),
                Focus::Dst => app.dst.move_by(1),
            }
            if matches!(focus, Focus::Src) {
                app.recompute_visible();
            }
        }
        _ => {}
    }
}

fn handle_confirm(app: &mut App, kind: ConfirmKind, key: KeyEvent) {
    match (key.code, key.modifiers) {
        (KeyCode::Char('y'), _) | (KeyCode::Enter, _) => {
            match kind {
                ConfirmKind::Transfer => app.apply_pending(),
                _ => {
                    app.session.accept(kind);
                }
            }
            close_confirm_if_done(app, kind);
        }
        (KeyCode::Char('n'), _) | (KeyCode::Esc, _) | (KeyCode::Char('q'), _) => {
            app.session.cancel(kind);
            close_confirm_if_done(app, kind);
        }
        // The row detail runs to about 24 lines each, so the modal scrolls.
        (KeyCode::Up, _) | (KeyCode::Char('k'), _) => app.scroll_confirm(-1),
        (KeyCode::Down, _) | (KeyCode::Char('j'), _) => app.scroll_confirm(1),
        (KeyCode::PageUp, _) => app.scroll_confirm(-10),
        (KeyCode::PageDown, _) => app.scroll_confirm(10),
        _ => {}
    }
}

/// Move on once the confirmation is gone: to the one waiting behind it, or back
/// to normal keys. A refused yes leaves it open, so the user can read it again.
fn close_confirm_if_done(app: &mut App, kind: ConfirmKind) {
    match app.session.confirm() {
        Some(now) if now == kind => {}
        Some(_) => app.confirm_scroll = 0,
        None => {
            app.mode = InputMode::Normal;
            app.confirm_scroll = 0;
        }
    }
}

fn handle_shop(app: &mut App, key: KeyEvent) {
    match (key.code, key.modifiers) {
        // Esc leaves the screen rather than quitting: quitting is the transfer
        // screen's job, so a stray Esc here cannot lose a queue of searches.
        (KeyCode::Esc, _) | (KeyCode::Char('q'), _) => {
            app.screen = Screen::Transfer;
        }
        (KeyCode::Char('?'), _) => {
            app.mode = InputMode::Help;
        }
        (KeyCode::Tab, _) | (KeyCode::BackTab, _) => app.toggle_shop_focus(),
        (KeyCode::Char('/'), _) => {
            app.shop_focus = ShopFocus::Tracks;
            app.mode = InputMode::ShopSearch;
        }
        (KeyCode::Up, _) | (KeyCode::Char('k'), _) => app.shop_move(-1),
        (KeyCode::Down, _) | (KeyCode::Char('j'), _) => app.shop_move(1),
        (KeyCode::PageUp, _) => app.shop_move(-10),
        (KeyCode::PageDown, _) => app.shop_move(10),
        (KeyCode::Char('g'), _) => app.shop_jump(true),
        (KeyCode::Char('G'), _) => app.shop_jump(false),
        (KeyCode::Char(' '), _) => app.toggle_basket(),
        (KeyCode::Char('c'), _) => {
            app.shop_list.selected.clear();
            app.session.status.info("basket emptied.");
        }
        // Add to the shopping list, or show what it already found.
        (KeyCode::Char('s'), _) => {
            app.shop_track();
        }
        // Where a download goes after it lands.
        (KeyCode::Char('p'), _) => {
            app.open_queue();
        }
        (KeyCode::Char('S'), _) => {
            app.shop_selected();
        }
        // Re-run one search, keeping every other result.
        (KeyCode::Char('r'), _) => {
            app.start_shop();
        }
        // Enter does the useful thing for whichever pane you are in.
        (KeyCode::Enter, _) => match app.shop_focus {
            ShopFocus::Tracks => {
                app.shop_track();
            }
            ShopFocus::Offers => {
                app.start_fetch();
            }
        },
        (KeyCode::Char('f'), _) => {
            app.start_fetch();
        }
        // Opening the page is for buying something you don't own yet.
        (KeyCode::Char('o'), _) => open_selected_offer(app),
        (KeyCode::Char('y'), _) => show_selected_ref(app),
        _ => {}
    }
}

/// Typing into the shop screen's track filter.
fn handle_shop_search(app: &mut App, key: KeyEvent) {
    match (key.code, key.modifiers) {
        (KeyCode::Esc, _) | (KeyCode::Enter, _) => {
            app.mode = InputMode::Normal;
        }
        (KeyCode::Backspace, _) => {
            app.shop_list.query.pop();
            app.recompute_visible();
        }
        (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
            app.shop_list.query.clear();
            app.recompute_visible();
        }
        (KeyCode::Char(c), m) if !m.contains(KeyModifiers::CONTROL) => {
            app.shop_list.query.push(c);
            app.recompute_visible();
        }
        (KeyCode::Up, _) => app.shop_list.move_by(-1),
        (KeyCode::Down, _) => app.shop_list.move_by(1),
        _ => {}
    }
}

/// Open the highlighted offer's page in a browser.
fn open_selected_offer(app: &mut App) {
    let Some(offer) = app.session.shop.selected().map(|r| r.offer.clone()) else {
        app.session.status.warn("no offer selected.");
        return;
    };
    app.session.open_offer(&offer);
}

/// Show the stable item ref, for scripting or a follow-up CLI run.
fn show_selected_ref(app: &mut App) {
    match app.session.shop.selected() {
        Some(r) => {
            let r = r.offer.item_ref.to_string();
            app.session.status.info(r);
        }
        None => app.session.status.warn("no offer selected."),
    }
}
