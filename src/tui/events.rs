use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::analysis;
use crate::db::safety_preflight;

use super::app::{App, ConfirmKind, Focus, InputMode, PendingBatch, Screen, ShopFocus};

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

    let mode = app.mode.clone();
    match mode {
        InputMode::Normal => match app.screen {
            Screen::Transfer => handle_normal(app, key),
            Screen::Shop => handle_shop(app, key),
            Screen::Pending => handle_pending(app, key),
        },
        InputMode::Search(focus) => handle_search(app, key, focus),
        InputMode::ShopSearch => handle_shop_search(app, key),
        InputMode::Confirm(kind) => handle_confirm(app, kind, key),
        InputMode::Help => handle_help(app, key),
    }
}

/// The queue of downloads that have not become transfers yet.
///
/// Read-only for now: navigation, and the state each entry is in.
fn handle_pending(app: &mut App, key: KeyEvent) {
    match (key.code, key.modifiers) {
        // Esc leaves the screen rather than quitting, matching the shop screen.
        (KeyCode::Esc, _) | (KeyCode::Char('q'), _) => {
            app.screen = Screen::Transfer;
        }
        (KeyCode::Char('?'), _) => app.mode = InputMode::Help,
        (KeyCode::Up, _) | (KeyCode::Char('k'), _) => app.queue.move_by(-1),
        (KeyCode::Down, _) | (KeyCode::Char('j'), _) => app.queue.move_by(1),
        (KeyCode::PageUp, _) => app.queue.move_by(-10),
        (KeyCode::PageDown, _) => app.queue.move_by(10),
        (KeyCode::Char('g'), _) => app.queue.jump_top(),
        (KeyCode::Char('G'), _) => app.queue.jump_bottom(),
        (KeyCode::Char('R'), _) => app.reload_queue(),
        (KeyCode::Char('i'), _) => app.start_import(),
        (KeyCode::Char('a'), _) => app.start_apply(),
        (KeyCode::Char('r'), _) => app.retry_selected(),
        (KeyCode::Char('c'), _) => app.forget_selected(),
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
    !app.dst.selected.is_empty() || app.unresolved_errors || !app.work_in_flight().is_empty()
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
    if app.unresolved_errors {
        bits.push("unresolved apply errors".into());
    }
    // Named per kind: a fingerprint reported as a search is a lie, and this is
    // the one message standing between the user and losing minutes of work.
    for (kind, n) in app.work_in_flight() {
        bits.push(format!("{n} {}(s) still running", kind.label()));
    }
    app.status.warn(format!(
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
                app.status.info(
                    "the source is the highlighted row — 's' shops for it, Tab picks destinations.",
                );
                return;
            }
            let id = app
                .dst
                .visible
                .get(app.dst.cursor)
                .and_then(|&i| app.rows.get(i))
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
            app.copy_opts.replace = !app.copy_opts.replace;
            app.status.info(format!(
                "replace = {}",
                if app.copy_opts.replace { "ON" } else { "off" }
            ));
        }
        (KeyCode::Char('l'), _) => {
            app.copy_opts.lock = !app.copy_opts.lock;
            app.status.info(format!(
                "lock = {}",
                if app.copy_opts.lock { "ON" } else { "off" }
            ));
        }
        (KeyCode::Char('R'), _) => match app.reload_db() {
            Ok(()) => app.status.ok(format!("Reloaded {} rows.", app.rows.len())),
            Err(e) => app.status.err(format!("reload failed: {e}")),
        },
        (KeyCode::Enter, _) => build_pending(app),
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
        (KeyCode::Char('y'), _) | (KeyCode::Enter, _) => match kind {
            ConfirmKind::Transfer => apply_pending(app),
            ConfirmKind::ImportRows => app.apply_import_batch(),
        },
        (KeyCode::Char('n'), _) | (KeyCode::Esc, _) | (KeyCode::Char('q'), _) => {
            app.pending = None;
            app.import_batch = None;
            app.mode = InputMode::Normal;
        }
        // The row detail runs to about 24 lines each, so the modal scrolls.
        (KeyCode::Up, _) | (KeyCode::Char('k'), _) => app.scroll_confirm(-1),
        (KeyCode::Down, _) | (KeyCode::Char('j'), _) => app.scroll_confirm(1),
        (KeyCode::PageUp, _) => app.scroll_confirm(-10),
        (KeyCode::PageDown, _) => app.scroll_confirm(10),
        _ => {}
    }
}

fn build_pending(app: &mut App) {
    let src_id = match app.current_src() {
        Some(r) => r.id.clone(),
        None => {
            app.status.err("no source selected");
            return;
        }
    };

    // Destinations: explicit multi-selection, or cursor row if selection empty.
    let mut dst_ids: Vec<String> = app.dst.selected.iter().cloned().collect();
    if dst_ids.is_empty()
        && let Some(r) = app.current_dst()
        && r.id != src_id
    {
        dst_ids.push(r.id.clone());
    }
    if dst_ids.is_empty() {
        app.status.err("no destinations selected");
        return;
    }
    dst_ids.sort();

    let mut plans = Vec::new();
    let mut failures = Vec::new();
    for dst_id in dst_ids {
        match analysis::build_plan(&app.db, &src_id, &dst_id, &app.copy_opts) {
            Ok(plan) => plans.push(plan),
            Err(e) => failures.push((dst_id, e.to_string())),
        }
    }

    app.pending = Some(PendingBatch {
        plans,
        failures,
        scroll: 0,
    });
    app.mode = InputMode::Confirm(ConfirmKind::Transfer);
}

fn apply_pending(app: &mut App) {
    let Some(batch) = app.pending.take() else {
        app.mode = InputMode::Normal;
        return;
    };

    if let Err(e) = safety_preflight(app.safety) {
        // Put the batch back so the user can read the modal again if they want.
        app.pending = Some(batch);
        app.status.err(format!("{e}"));
        return;
    }

    let total = batch.plans.len();
    let mut errs: Vec<String> = Vec::new();
    let mut backup_path = None;
    for plan in &batch.plans {
        match analysis::apply_plan(&mut app.db, plan) {
            Ok(path) => {
                backup_path.get_or_insert(path);
            }
            Err(e) => {
                let dst_label = plan
                    .dst
                    .title
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&plan.dst.id);
                errs.push(format!("\"{dst_label}\": {e}"));
            }
        }
    }
    let ok = total - errs.len();

    app.dst.selected.clear();
    app.mode = InputMode::Normal;

    match app.reload_db() {
        Ok(()) => {}
        Err(e) => app.status.warn(format!("reload after apply failed: {e}")),
    }

    let backup_hint = backup_path
        .map(|p| format!(" Backup: {}", p.display()))
        .unwrap_or_default();
    if errs.is_empty() {
        app.status.ok(format!("Applied {ok}/{total}.{backup_hint}"));
    } else {
        app.unresolved_errors = true;
        let extra = if errs.len() > 1 {
            format!(" (+{} more)", errs.len() - 1)
        } else {
            String::new()
        };
        app.status.err(format!(
            "Applied {ok}/{total}. Failed → {first}{extra}{backup_hint}",
            first = errs[0],
        ));
    }
}

/// The shop screen: a track list on the left, offers on the right.
///
/// Only downloading writes anything, and it writes a file rather than the
/// database — browsing offers and opening a buy page cannot touch `master.db`,
/// so nothing here needs the safety preflight. The transfer it queues is applied
/// later, through the same gated path as everything else.
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
            app.status.info("basket emptied.");
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
    let Some(offer) = app.shop.selected().map(|r| r.offer.clone()) else {
        app.status.warn("no offer selected.");
        return;
    };
    match crate::proc::open_url(&offer.url) {
        Ok(()) => app
            .status
            .ok(format!("opened {} in your browser.", offer.title)),
        Err(e) => app.status.err(format!("could not open a browser: {e}")),
    }
}

/// Show the stable item ref, for scripting or a follow-up CLI run.
fn show_selected_ref(app: &mut App) {
    match app.shop.selected() {
        Some(r) => {
            let r = r.offer.item_ref.to_string();
            app.status.info(r);
        }
        None => app.status.warn("no offer selected."),
    }
}
