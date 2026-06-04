use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::analysis;
use crate::db::safety_preflight;

use super::app::{App, Focus, InputMode, PendingBatch};

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
        InputMode::Normal => handle_normal(app, key),
        InputMode::Search(focus) => handle_search(app, key, focus),
        InputMode::Confirm => handle_confirm(app, key),
        InputMode::Help => {
            app.mode = InputMode::Normal;
        }
    }
}

/// Return true if quitting now would discard work the user might want.
fn has_pending_work(app: &App) -> bool {
    !app.dst.selected.is_empty() || app.unresolved_errors
}

fn try_quit(app: &mut App) {
    if app.quit_pending || !has_pending_work(app) {
        app.should_quit = true;
        return;
    }
    app.quit_pending = true;
    let mut bits = Vec::new();
    if !app.dst.selected.is_empty() {
        bits.push(format!("{} destination(s) selected", app.dst.selected.len()));
    }
    if app.unresolved_errors {
        bits.push("unresolved apply errors".into());
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
            if matches!(app.focus, Focus::Src) && app.dst_filters.fuzzy_from_src {
                app.recompute_visible();
            }
        }
        (KeyCode::Down, _) | (KeyCode::Char('j'), _) => {
            app.focused_column_mut().move_by(1);
            if matches!(app.focus, Focus::Src) && app.dst_filters.fuzzy_from_src {
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
        (KeyCode::Char(' '), _) => {
            if let Focus::Dst = app.focus {
                let id = app
                    .dst
                    .visible
                    .get(app.dst.cursor)
                    .and_then(|&i| app.rows.get(i))
                    .map(|r| r.id.clone());
                if let Some(id) = id {
                    if !app.dst.selected.remove(&id) {
                        app.dst.selected.insert(id);
                    }
                }
            }
        }
        (KeyCode::Char('c'), _) => {
            if matches!(app.focus, Focus::Dst) {
                app.dst.selected.clear();
            }
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
            if matches!(focus, Focus::Src) && app.dst_filters.fuzzy_from_src {
                app.recompute_visible();
            }
        }
        (KeyCode::Down, _) => {
            match focus {
                Focus::Src => app.src.move_by(1),
                Focus::Dst => app.dst.move_by(1),
            }
            if matches!(focus, Focus::Src) && app.dst_filters.fuzzy_from_src {
                app.recompute_visible();
            }
        }
        _ => {}
    }
}

fn handle_confirm(app: &mut App, key: KeyEvent) {
    match (key.code, key.modifiers) {
        (KeyCode::Char('y'), _) | (KeyCode::Enter, _) => apply_pending(app),
        (KeyCode::Char('n'), _) | (KeyCode::Esc, _) | (KeyCode::Char('q'), _) => {
            app.pending = None;
            app.mode = InputMode::Normal;
        }
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
    if dst_ids.is_empty() {
        if let Some(r) = app.current_dst() {
            if r.id != src_id {
                dst_ids.push(r.id.clone());
            }
        }
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

    app.pending = Some(PendingBatch { plans, failures });
    app.mode = InputMode::Confirm;
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

    let mut ok = 0usize;
    let mut errs: Vec<String> = Vec::new();
    let total = batch.plans.len();
    for plan in &batch.plans {
        match analysis::apply_plan(&mut app.db, plan) {
            Ok(()) => ok += 1,
            Err(e) => errs.push(format!("{} → {}: {e}", plan.src.id, plan.dst.id)),
        }
    }

    app.dst.selected.clear();
    app.mode = InputMode::Normal;

    match app.reload_db() {
        Ok(()) => {}
        Err(e) => app.status.warn(format!("reload after apply failed: {e}")),
    }

    if errs.is_empty() {
        app.status.ok(format!("Applied {ok}/{total}."));
    } else {
        app.unresolved_errors = true;
        app.status
            .err(format!("Applied {ok}/{total}. {} failed.", errs.len()));
    }

}
