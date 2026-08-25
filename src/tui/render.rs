use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};

use crate::format::{file_type_name, format_bpm, format_length};

use super::app::{App, Focus, InputMode, Screen, ShopFocus, ShopTrackState, StatusLevel};
use super::diff::render_pair;
use crate::library::TrackRow;

/// Takes `&mut App` for one reason: the help popup's scroll offset can only be
/// clamped here, where the popup's final height is known.
pub fn draw(f: &mut Frame, app: &mut App) {
    match app.screen {
        Screen::Transfer => draw_transfer(f, app),
        Screen::Shop => draw_shop_screen(f, app),
    }
    match app.mode {
        InputMode::Confirm => draw_confirm(f, app),
        InputMode::Help => draw_help(f, app),
        _ => {}
    }
}

fn draw_transfer(f: &mut Frame, app: &App) {
    let outer = Layout::vertical([
        Constraint::Length(1), // top bar
        Constraint::Min(0),    // body (columns + preview)
        Constraint::Length(2), // status bar
    ])
    .split(f.area());

    draw_top_bar(f, outer[0], app);

    let body = Layout::vertical([
        Constraint::Min(0),    // columns
        Constraint::Length(5), // preview
    ])
    .split(outer[1]);

    let cols =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(body[0]);
    draw_column(f, cols[0], app, Focus::Src);
    draw_column(f, cols[1], app, Focus::Dst);

    draw_preview(f, body[1], app);
    draw_status(f, outer[2], app);
}

fn draw_top_bar(f: &mut Frame, area: Rect, app: &App) {
    let title = Span::styled("rekord-ripper TUI", Style::new().bold().cyan());
    let opts = format!(
        "  replace={}  lock={}",
        if app.copy_opts.replace { "ON" } else { "off" },
        if app.copy_opts.lock { "ON" } else { "off" },
    );
    let rb = if app.rb_running {
        Span::styled("  rekordbox: RUNNING ", Style::new().fg(Color::Red).bold())
    } else {
        Span::styled("  rekordbox: closed", Style::new().fg(Color::DarkGray))
    };
    let line = Line::from(vec![title, Span::raw(opts), rb]);
    f.render_widget(Paragraph::new(line), area);
}

fn draw_column(f: &mut Frame, area: Rect, app: &App, which: Focus) {
    let (state, label, extras) = match which {
        Focus::Src => (&app.src, "SOURCES", String::new()),
        Focus::Dst => {
            let mut tags = Vec::new();
            if app.dst_filters.auto {
                tags.push("auto".to_string());
            }
            if app.dst_filters.fuzzy_from_src {
                tags.push("fuzzy".to_string());
            }
            // Selected but filtered out is still selected, and still applied.
            let hidden = app.dst_hidden();
            if hidden > 0 {
                tags.push(format!("{hidden} selected hidden"));
            }
            let t = if tags.is_empty() {
                String::new()
            } else {
                format!(" [{}]", tags.join(", "))
            };
            (&app.dst, "DESTINATIONS", t)
        }
    };

    let focused = which == app.focus;
    let border_style = if focused {
        Style::new().fg(Color::Cyan)
    } else {
        Style::new().fg(Color::DarkGray)
    };
    let title = format!(" {label} ({}){} ", state.visible.len(), extras);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(title);

    let inner = block.inner(area);
    f.render_widget(block, area);

    let inner_layout = Layout::vertical([Constraint::Length(2), Constraint::Min(0)]).split(inner);

    // Search bar. The second line is spare, so an empty box advertises the
    // playlist filter rather than leaving it to the help page.
    let search_active = matches!(app.mode, InputMode::Search(f0) if f0 == which);
    let search_caret = if search_active { "_" } else { "" };
    let search = format!(" / {}{}", state.query, search_caret);
    let style = if search_active {
        Style::new().bold()
    } else {
        Style::new().dim()
    };
    let mut bar = vec![Line::styled(search, style)];
    if search_active && state.query.is_empty() {
        bar.push(Line::styled(SEARCH_HINT, Style::new().dim()));
    }
    f.render_widget(Paragraph::new(bar), inner_layout[0]);

    // List — two lines per row. The first line carries a green ✓ when the row
    // is the "apply target" for its column: for SOURCES that's the cursor row
    // (the implicit single source); for DESTINATIONS, the multi-selection set.
    //
    // When the column isn't focused, dim every row *except* the "active" one(s)
    // so the user can see at a glance what would actually be in the apply batch
    // regardless of which side they're currently navigating.
    let dim = Style::new().add_modifier(Modifier::DIM);
    let mut items: Vec<ListItem> = Vec::with_capacity(state.visible.len() * 2);
    for (visible_pos, &row_idx) in state.visible.iter().enumerate() {
        let row = &app.rows[row_idx];
        // A tick means "this row is in the batch": the cursor row for sources
        // (there is only ever one source), and the selection for destinations —
        // falling back to the cursor row when nothing is picked.
        let active = match which {
            Focus::Src => visible_pos == state.cursor,
            Focus::Dst => {
                state.selected.contains(&row.id)
                    || (state.selected.is_empty() && visible_pos == state.cursor)
            }
        };
        let row_style = if !focused && !active {
            dim
        } else {
            Style::new()
        };
        items.push(track_item_line1(row, active).style(row_style));
        items.push(track_item_line2(row).style(row_style));
    }

    let mut list_state = ListState::default();
    // Each row is two ListItems; the *first* line of the cursor row is the
    // selection target.
    if !state.visible.is_empty() {
        list_state.select(Some(state.cursor * 2));
    }

    let highlight_style = if focused {
        // REVERSED swaps fg/bg per-cell, so the row stays legible regardless of
        // the per-span fg color (the previous DarkGray bg hid DarkGray text).
        Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD)
    } else {
        // Per-row dimming above already does the "what's active" work.
        Style::new()
    };
    let list = List::new(items).highlight_style(highlight_style);
    f.render_stateful_widget(list, inner_layout[1], &mut list_state);
}

fn track_item_line1(row: &TrackRow, marked: bool) -> ListItem<'static> {
    let title = if row.title.is_empty() {
        "(untitled)"
    } else {
        &row.title
    };
    let artist = if row.artist.is_empty() {
        "—"
    } else {
        &row.artist
    };
    let mark_span = if marked {
        Span::styled("✓ ", Style::new().fg(Color::Green).bold())
    } else {
        Span::raw("  ")
    };
    let line = Line::from(vec![
        mark_span,
        Span::styled(title.to_string(), Style::new().bold()),
        Span::styled(format!("  —  {}", artist), Style::new().fg(Color::Gray)),
        Span::styled(
            format!("  —  {}", file_type_name(row.file_type)),
            Style::new().fg(Color::Gray),
        ),
    ]);
    ListItem::new(line)
}

fn track_item_line2(row: &TrackRow) -> ListItem<'static> {
    let lock = if row.locked {
        Span::styled(" 🔒", Style::new().fg(Color::Yellow))
    } else {
        Span::raw("   ")
    };
    let line = Line::from(vec![
        Span::raw("    "),
        Span::styled(format_bpm(row.bpm), Style::new().fg(Color::Magenta)),
        Span::raw(" BPM   "),
        Span::raw(format_length(row.length)),
        Span::raw("   "),
        Span::styled(
            format!("{} cues", row.cue_count),
            Style::new().fg(Color::Green),
        ),
        lock,
    ]);
    ListItem::new(line)
}

fn draw_preview(f: &mut Frame, area: Rect, app: &App) {
    let src = app.current_src();
    let dst = app.current_dst();
    let lines: Vec<Line<'static>> = render_pair(src, dst, app.copy_opts)
        .into_iter()
        .map(Line::from)
        .collect();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(Color::DarkGray))
        .title(" PREVIEW ");
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_status(f: &mut Frame, area: Rect, app: &App) {
    let parts = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(area);

    let hints = match app.screen {
        Screen::Transfer => {
            "tab focus  / search  s shop  space pick dest  a auto  f fuzzy  r replace  l lock  enter apply  ? help  q quit"
        }
        Screen::Shop => {
            "tab pane  / filter  s search  space basket  S search basket  r re-search  enter download  o buy page  y ref  esc back"
        }
    };
    f.render_widget(
        Paragraph::new(hints).style(Style::new().fg(Color::DarkGray)),
        parts[0],
    );

    let style = match app.status.level {
        StatusLevel::Info => Style::new().fg(Color::Gray),
        StatusLevel::Ok => Style::new().fg(Color::Green),
        StatusLevel::Warn => Style::new().fg(Color::Yellow),
        StatusLevel::Err => Style::new().fg(Color::Red).bold(),
    };
    f.render_widget(
        Paragraph::new(app.status.text.as_str()).style(style),
        parts[1],
    );
}

fn draw_confirm(f: &mut Frame, app: &App) {
    let Some(batch) = app.pending.as_ref() else {
        return;
    };
    let area = popup_area(f.area(), 80, 70);
    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(Color::Yellow))
        .title(" CONFIRM APPLY ");

    let mut lines: Vec<Line> = Vec::new();
    if let Some(p) = batch.plans.first() {
        let src_title = p.src.title.as_deref().unwrap_or("?");
        let src_artist = p.src.artist.as_deref().unwrap_or("?");
        lines.push(Line::from(format!(
            "src: \"{src_title}\" — {src_artist}   ({})",
            p.src.id
        )));
        lines.push(Line::from(""));
    }
    for p in &batch.plans {
        let dst_title = p.dst.title.as_deref().unwrap_or("?");
        let bpm_pair = match (p.set_bpm, p.dst.bpm) {
            (Some(s), Some(d)) if s != d => {
                format!("BPM {:.2} → {:.2}", d as f64 / 100.0, s as f64 / 100.0)
            }
            _ => "BPM ≈".into(),
        };
        let cue_delta = format!("cues {} → {}", p.dst.cue_count, p.src.cue_count);
        let len = if p.set_length.is_some() {
            "len ≈"
        } else {
            "len skipped"
        };
        let dst_artist = p.dst.artist.as_deref().unwrap_or("?");
        lines.push(Line::from(vec![
            Span::raw("  → "),
            Span::styled(
                format!("\"{dst_title}\" — {dst_artist}"),
                Style::new().bold(),
            ),
            Span::styled(
                format!("  ({})", p.dst.id),
                Style::new().fg(Color::DarkGray),
            ),
        ]));
        lines.push(Line::from(format!(
            "       {bpm_pair}   {cue_delta}   {len}"
        )));
    }
    if !batch.failures.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::styled(
            "SKIPPED:",
            Style::new().fg(Color::Yellow).bold(),
        ));
        for (dst_id, err) in &batch.failures {
            lines.push(Line::from(format!("  {dst_id}: {err}")));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::styled(
        format!("[y/enter] apply {}     [n/esc] cancel", batch.plans.len()),
        Style::new().fg(Color::Cyan).bold(),
    ));

    let para = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .block(block);
    f.render_widget(para, area);
}

/// The shop screen: a track list on the left, offers on the right.
///
/// A screen rather than an overlay on the transfer view. As an overlay it shared
/// the track cursor and `Space` with that view, so selecting sources and then
/// stepping across to DESTINATIONS was reachable and meant nothing at all. Each
/// screen now owns its own list and its own selection.
fn draw_shop_screen(f: &mut Frame, app: &App) {
    let outer = Layout::vertical([
        Constraint::Length(1), // top bar
        Constraint::Min(0),    // tracks + offers
        Constraint::Length(2), // status bar
    ])
    .split(f.area());

    draw_shop_top_bar(f, outer[0], app);
    let cols = Layout::horizontal([Constraint::Percentage(34), Constraint::Min(0)]).split(outer[1]);
    draw_shop_tracks(f, cols[0], app);
    draw_shop_offers(f, cols[1], app);
    draw_status(f, outer[2], app);
}

fn draw_shop_top_bar(f: &mut Frame, area: Rect, app: &App) {
    let mut spans = vec![Span::styled("shop", Style::new().bold().magenta())];
    // The full label of the highlighted track: the track pane is narrow enough to
    // clip a long title, and this is the one place with room for all of it.
    if let Some(row) = app.current_shop_track() {
        spans.push(Span::raw(format!(
            "  {} — {}",
            display_artist(row),
            display_title(row)
        )));
    }
    // The spinner lives here rather than in the table: results stay on screen
    // while more searches run, and a line appearing above them would shove the
    // whole table down a row.
    let queued = app.shop_outstanding();
    if queued > 0 {
        spans.push(Span::styled(
            format!(
                "  {} {queued} search(es) running",
                spinner(app.shop_since())
            ),
            Style::new().fg(Color::Cyan),
        ));
    } else {
        spans.push(Span::styled(
            "  esc → transfer",
            Style::new().fg(Color::DarkGray),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// The library, filtered, with each track's search state.
fn draw_shop_tracks(f: &mut Frame, area: Rect, app: &App) {
    let focused = app.shop_focus == ShopFocus::Tracks;
    let basket = app.shop_list.selected.len();
    let hidden = app.basket_hidden();
    let title = match (basket, hidden) {
        (0, _) => format!(" TRACKS ({}) ", app.shop_list.visible.len()),
        // A basket item the filter is hiding is still in the basket, and 'S' still
        // searches it. Saying so is what stops it reading as forgotten.
        (b, 0) => format!(" TRACKS ({}) [basket {b}] ", app.shop_list.visible.len()),
        (b, h) => format!(
            " TRACKS ({}) [basket {b}, {h} hidden by filter] ",
            app.shop_list.visible.len()
        ),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(pane_border(focused))
        .title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let parts = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(inner);
    let typing = matches!(app.mode, InputMode::ShopSearch);
    let bar = format!(
        " / {}{}",
        app.shop_list.query,
        if typing { "_" } else { "" }
    );
    f.render_widget(
        Paragraph::new(bar).style(if typing {
            Style::new().bold()
        } else {
            Style::new().dim()
        }),
        parts[0],
    );

    let offer_src = app.shop_offer_src();
    let items: Vec<ListItem> = app
        .shop_list
        .visible
        .iter()
        .map(|&i| {
            let row = &app.rows[i];
            let in_basket = app.shop_list.selected.contains(&row.id);
            // Two independent one-character slots: basket membership, and whether
            // the highlighted offer came from this track.
            let is_offer_src = offer_src == Some(row.id.as_str());
            // The state tag is what makes a queue of searches legible: how many
            // offers each track found, and which are still waiting their turn.
            let (tag, tag_style) = match app.shop_track_state(&row.id) {
                ShopTrackState::Done(0) => ("  · ".to_string(), Style::new().fg(Color::DarkGray)),
                ShopTrackState::Done(n) => (format!("{n:>3} "), Style::new().fg(Color::Green)),
                ShopTrackState::Queued => ("  … ".to_string(), Style::new().fg(Color::Cyan)),
                ShopTrackState::Untouched => ("    ".to_string(), Style::new()),
            };
            ListItem::new(Line::from(vec![
                Span::styled(
                    if in_basket { "✓" } else { " " },
                    Style::new().fg(Color::Yellow).bold(),
                ),
                Span::styled(
                    if is_offer_src { "▸" } else { " " },
                    Style::new().fg(Color::Cyan).bold(),
                ),
                Span::styled(tag, tag_style),
                Span::styled(display_title(row).to_string(), Style::new().bold()),
                Span::styled(
                    format!(" — {}", display_artist(row)),
                    Style::new().fg(Color::Gray),
                ),
            ]))
        })
        .collect();

    let mut state = ListState::default();
    if !app.shop_list.visible.is_empty() {
        state.select(Some(app.shop_list.cursor));
    }
    let list = List::new(items).highlight_style(if focused {
        Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD)
    } else {
        Style::new().add_modifier(Modifier::BOLD)
    });
    f.render_stateful_widget(list, parts[1], &mut state);
}

/// The offer table, and — the reason the worker exists — a live "searching"
/// state instead of a frozen screen.
fn draw_shop_offers(f: &mut Frame, area: Rect, app: &App) {
    let focused = app.shop_focus == ShopFocus::Offers;
    let view = offer_body(app, area.width.saturating_sub(2), focused);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(pane_border(focused))
        .title(view.title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let detail = detail_lines(app, inner.width);
    // A fixed share, so the table does not jump every time the highlighted
    // offer's url wraps to a different number of lines.
    let want = (detail.len() as u16).min(inner.height * 2 / 5);
    let parts = Layout::vertical([
        // The column header is pinned: it used to be row 0 of the scrolling body,
        // so scrolling down to a later track's offers left an unlabelled table.
        Constraint::Length(u16::from(view.header.is_some())),
        Constraint::Min(0),
        Constraint::Length(want),
    ])
    .split(inner);

    if let Some(header) = view.header {
        f.render_widget(Paragraph::new(header), parts[0]);
    }
    let scroll = scroll_for(view.cursor_line, view.lines.len(), parts[1].height as usize);
    f.render_widget(
        Paragraph::new(view.lines).scroll((scroll as u16, 0)),
        parts[1],
    );
    if want > 0 {
        f.render_widget(Paragraph::new(detail), parts[2]);
    }
}

/// Scroll offset that keeps `cursor_line` inside a pane `height` rows tall.
///
/// The offer table has no scrolling of its own — it is a `Paragraph`, not a
/// `List` — so once a few tracks are searched the cursor would otherwise walk
/// off the bottom with no way to follow it.
fn scroll_for(cursor_line: Option<usize>, total: usize, height: usize) -> usize {
    cursor_line
        .map(|c| c.saturating_sub(height / 2))
        .unwrap_or(0)
        .min(total.saturating_sub(height))
}

fn pane_border(focused: bool) -> Style {
    if focused {
        Style::new().fg(Color::Cyan)
    } else {
        Style::new().fg(Color::DarkGray)
    }
}

/// Animated from elapsed time, so it visibly advances on every 300ms tick and
/// the user can tell the difference between working and hung.
fn spinner(since: Option<std::time::Instant>) -> &'static str {
    // Process start, so a spinner with no job start time — a queued search while
    // earlier results are still on screen — still animates.
    static START: std::sync::LazyLock<std::time::Instant> =
        std::sync::LazyLock::new(std::time::Instant::now);
    const FRAMES: [&str; 4] = ["|", "/", "-", "\\"];
    let ms = since.unwrap_or(*START).elapsed().as_millis();
    FRAMES[(ms / 300) as usize % FRAMES.len()]
}

fn display_title(row: &TrackRow) -> &str {
    if row.title.is_empty() {
        "(untitled)"
    } else {
        &row.title
    }
}

fn display_artist(row: &TrackRow) -> &str {
    if row.artist.is_empty() {
        "—"
    } else {
        &row.artist
    }
}

/// Everything `draw_shop_offers` needs to lay the pane out.
struct OfferView {
    title: String,
    /// The column header, pinned above the scrolling rows. `None` when there is
    /// no table — searching, failed, or nothing searched yet.
    header: Option<Line<'static>>,
    lines: Vec<Line<'static>>,
    cursor_line: Option<usize>,
}

fn offer_body(app: &App, width: u16, focused: bool) -> OfferView {
    use super::app::ShopState;

    let mut cursor_line: Option<usize> = None;
    let mut header: Option<Line<'static>> = None;
    let (title, body_lines): (String, Vec<Line>) = match &app.shop {
        ShopState::Idle => (
            " OFFERS ".into(),
            vec![
                Line::from(Span::styled(
                    "nothing searched yet.",
                    Style::new().fg(Color::DarkGray),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "  's' searches the highlighted track. Tap it on several and they",
                    Style::new().fg(Color::DarkGray),
                )),
                Line::from(Span::styled(
                    "  search one after another, results accumulating here.",
                    Style::new().fg(Color::DarkGray),
                )),
                Line::from(Span::styled(
                    "  Space fills the basket; 'S' searches all of it.",
                    Style::new().fg(Color::DarkGray),
                )),
            ],
        ),

        ShopState::Searching {
            since,
            what,
            done,
            total,
            ..
        } => {
            let secs = since.elapsed().as_secs();
            let frame = spinner(Some(*since));
            let mut lines = vec![
                Line::from(vec![
                    Span::styled(format!("{frame} "), Style::new().fg(Color::Cyan)),
                    Span::raw(format!("searching for {what}")),
                ]),
                Line::from(Span::styled(
                    format!("  {secs}s elapsed"),
                    Style::new().fg(Color::DarkGray),
                )),
            ];
            // A real progress bar for a bulk search; a single search has nothing
            // meaningful to show beyond the spinner.
            if *total > 1 {
                let width = 40usize;
                let filled = (*done * width) / (*total).max(1);
                lines.push(Line::from(Span::styled(
                    format!(
                        "  [{}{}] {done}/{total}",
                        "#".repeat(filled),
                        "·".repeat(width.saturating_sub(filled))
                    ),
                    Style::new().fg(Color::Cyan),
                )));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  Esc steps away without losing this; 's' brings it back",
                Style::new().fg(Color::DarkGray),
            )));
            (" OFFERS — searching ".into(), lines)
        }

        ShopState::Failed(why) => (
            " OFFERS — failed ".into(),
            vec![Line::from(Span::styled(
                why.clone(),
                Style::new().fg(Color::Red),
            ))],
        ),

        ShopState::Results { groups, cursor, .. } => {
            header = Some(Line::from(Span::styled(
                offer_header(width),
                Style::new().add_modifier(Modifier::BOLD),
            )));
            let mut lines: Vec<Line> = Vec::new();

            // Flatten the groups, inserting a header per group so a bulk search
            // shows which track each block of offers belongs to.
            let multi = groups.len() > 1;
            let mut i = 0usize;
            for g in groups.iter() {
                if multi {
                    lines.push(Line::from(Span::styled(
                        format!(
                            "── for: {}{}",
                            g.label,
                            if g.outcome.offers.is_empty() {
                                "  (nothing found)"
                            } else {
                                ""
                            }
                        ),
                        Style::new().fg(Color::Yellow),
                    )));
                }
                for r in g.outcome.offers.iter() {
                    let o = &r.offer;
                    let selected = i == *cursor;
                    if selected {
                        cursor_line = Some(lines.len());
                    }
                    i += 1;
                    // Dimmed when the pane is not focused, so it is obvious which
                    // list the arrow keys are driving.
                    let style = match (selected, focused) {
                        (true, true) => {
                            Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD)
                        }
                        (true, false) => Style::new().add_modifier(Modifier::BOLD),
                        (false, _) => Style::new(),
                    };
                    // Lossless is the whole point, so mark it in the row itself.
                    lines.push(Line::from(Span::styled(
                        offer_row(
                            Row {
                                marker: lossless_marker(o),
                                backend: o.backend().as_str(),
                                title: &format!("{} — {}", o.artist, o.title),
                                formats: &crate::acquire::render::format_cell(o),
                                price: &crate::acquire::render::price_cell(o),
                                own: crate::acquire::render::ownership_cell(o),
                            },
                            width,
                        ),
                        style,
                    )));
                }
            }

            if i == 0 {
                lines.push(Line::from(Span::styled(
                    "no offers found.",
                    Style::new().fg(Color::DarkGray),
                )));
            }

            // Per-backend failures, so a partial table looks partial. Deduped:
            // a bulk search would otherwise repeat the same backend error once
            // per track.
            let mut seen: Vec<String> = Vec::new();
            for g in groups.iter() {
                for r in g.outcome.failures() {
                    let Some(e) = &r.error else { continue };
                    if e.is_silently_skippable() {
                        continue;
                    }
                    let msg = format!("degraded: {}: {e}", r.backend);
                    if !seen.contains(&msg) {
                        seen.push(msg.clone());
                        lines.push(Line::from(Span::styled(
                            msg,
                            Style::new().fg(Color::Yellow),
                        )));
                    }
                }
            }

            // Currencies are grouped, never converted — say so here too.
            let all_offers: Vec<crate::acquire::shop::RankedOffer> = groups
                .iter()
                .flat_map(|g| g.outcome.offers.iter().cloned())
                .collect();
            let cheap = crate::acquire::shop::cheapest_per_currency(&all_offers);
            if !cheap.is_empty() {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    format!(
                        "cheapest per currency: {}",
                        cheap
                            .iter()
                            .map(|(p, b)| format!("{p} ({b})"))
                            .collect::<Vec<_>>()
                            .join("   ")
                    ),
                    Style::new().fg(Color::DarkGray),
                )));
                if crate::acquire::shop::has_mixed_currencies(&all_offers) {
                    lines.push(Line::from(Span::styled(
                        "different currencies are not compared — no exchange rates available",
                        Style::new().fg(Color::DarkGray),
                    )));
                }
            }

            let title = if multi {
                format!(" OFFERS — {i} across {} tracks ", groups.len())
            } else {
                format!(" OFFERS — {i} ")
            };
            (title, lines)
        }
    };
    OfferView {
        title,
        header,
        lines: body_lines,
        cursor_line,
    }
}

/// Everything about the highlighted offer that the table had to clip, plus
/// whatever a download is currently doing.
///
/// Its own pane rather than more table rows: a long url wraps to a different
/// number of lines per offer, and appending that to the table made it jump
/// around under the cursor.
fn detail_lines(app: &App, width: u16) -> Vec<Line<'static>> {
    use super::app::FetchState;

    let mut lines: Vec<Line<'static>> = Vec::new();

    // A download in flight goes first, because it is the thing you are waiting
    // on and it must never be the part that gets clipped.
    match &app.fetch {
        FetchState::Idle => {}
        FetchState::Running { since, what } => {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{} downloading ", spinner(Some(*since))),
                    Style::new().fg(Color::Cyan),
                ),
                Span::raw(what.clone()),
                Span::styled(
                    format!("  ({}s)", since.elapsed().as_secs()),
                    Style::new().fg(Color::DarkGray),
                ),
            ]));
        }
        FetchState::Done { paths, queued } => {
            for p in paths {
                lines.push(Line::from(Span::styled(
                    format!(
                        "saved {}",
                        p.file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default()
                    ),
                    Style::new().fg(Color::Green),
                )));
            }
            lines.push(Line::from(Span::styled(
                match queued {
                    Some(id) => format!(
                        "queued transfer #{id} — import the download folder, then `pending --apply`"
                    ),
                    None => "no source track was selected, so no transfer was queued".into(),
                },
                Style::new().fg(Color::DarkGray),
            )));
        }
        FetchState::Failed(why) => {
            lines.push(Line::from(Span::styled(
                format!("download failed: {why}"),
                Style::new().fg(Color::Red),
            )));
        }
    }

    if let Some(r) = app.shop.selected() {
        let o = &r.offer;
        lines.push(Line::from(Span::styled(
            MARKER_LEGEND,
            Style::new().fg(Color::DarkGray),
        )));
        for (label, value) in [
            ("artist", o.artist.clone()),
            ("title", o.title.clone()),
            ("album", o.album.clone().unwrap_or_else(|| "—".into())),
            (
                "formats",
                match &o.formats {
                    None => "not checked yet".into(),
                    Some(fs) if fs.is_empty() => "none usable".into(),
                    Some(fs) => fs
                        .iter()
                        .map(|f| f.to_string())
                        .collect::<Vec<_>>()
                        .join(", "),
                },
            ),
            ("price", crate::acquire::render::price_cell(o)),
            ("url", o.url.clone()),
            ("ref", o.item_ref.to_string()),
        ] {
            // Wrapped rather than clipped, so a long url or title is readable
            // in full.
            for (i, chunk) in wrap(&value, detail_width(width)).into_iter().enumerate() {
                let head = if i == 0 { label } else { "" };
                lines.push(Line::from(vec![
                    Span::styled(format!("  {head:<9} "), Style::new().fg(Color::DarkGray)),
                    Span::raw(chunk),
                ]));
            }
        }
        if let Some(e) = &o.enrich_error {
            lines.push(Line::from(Span::styled(
                format!("  {:<9} {e}", "note"),
                Style::new().fg(Color::Yellow),
            )));
        }
    }

    lines
}

/// What the `L` column shows.
///
/// Derived from *format* knowledge, not price: a backend can know an offer is
/// free while its format list is still unprobed, so `?` here means "formats not
/// checked", never "price unknown". [`MARKER_LEGEND`] must keep saying that.
fn lossless_marker(o: &crate::acquire::types::Offer) -> &'static str {
    match o.has_lossless() {
        Some(true) => "*",
        Some(false) => " ",
        None => "?",
    }
}

/// The legend for [`lossless_marker`]. Kept next to it so the two stay in step.
const MARKER_LEGEND: &str = "L: * lossless, ? formats not checked (only top offers are probed)";

/// Wide enough for three format names, which is what `format_cell` shows.
const FORMATS_W: u16 = 23;

/// The `own` column, dropped whole when the pane is too narrow for it.
const OWN_W: u16 = 4;

const BACKEND_W: u16 = 11;
const PRICE_W: u16 = 16;
const MARKER_W: u16 = 2;

/// Narrowest artist/title worth showing.
const MIN_TITLE_W: u16 = 20;

/// Every column except artist/title and `own`, plus the three separators between
/// them. Derived from the same pieces [`offer_row`] formats, so the two cannot
/// drift — they did, by one column, and the row silently ran off the pane.
const FIXED_NO_OWN: u16 = MARKER_W + BACKEND_W + FORMATS_W + PRICE_W + 3;

/// The same, with `own` and its separator.
const FIXED_COLUMNS: u16 = FIXED_NO_OWN + 1 + OWN_W;

/// True when the pane has room for the `own` column.
fn shows_ownership(content_width: u16) -> bool {
    content_width >= FIXED_COLUMNS + MIN_TITLE_W
}

/// Width available to artist/title, given the pane's usable width.
fn title_width(content_width: u16) -> usize {
    let fixed = if shows_ownership(content_width) {
        FIXED_COLUMNS
    } else {
        FIXED_NO_OWN
    };
    content_width.saturating_sub(fixed).max(MIN_TITLE_W) as usize
}

/// The cells of one offer-table row.
struct Row<'a> {
    marker: &'a str,
    backend: &'a str,
    title: &'a str,
    formats: &'a str,
    price: &'a str,
    own: &'a str,
}

/// Render one row, sized to `content_width`.
///
/// The header goes through here too, so a column change cannot update one and
/// leave the other behind — they are drawn as separate widgets, since the header
/// is pinned outside the scroll region.
fn offer_row(r: Row<'_>, content_width: u16) -> String {
    let tw = title_width(content_width);
    let mut s = format!(
        "{:<mw$}{:<bw$} {:<tw$} {:<fw$} {:<pw$}",
        r.marker,
        clip_cell(r.backend, BACKEND_W as usize),
        clip_cell(r.title, tw),
        clip_cell(r.formats, FORMATS_W as usize),
        clip_cell(r.price, PRICE_W as usize),
        mw = MARKER_W as usize,
        bw = BACKEND_W as usize,
        fw = FORMATS_W as usize,
        pw = PRICE_W as usize,
    );
    if shows_ownership(content_width) {
        s.push_str(&format!(" {}", clip_cell(r.own, OWN_W as usize)));
    }
    s
}

/// The column header row.
fn offer_header(content_width: u16) -> String {
    offer_row(
        Row {
            marker: "L",
            backend: "backend",
            title: "artist / title",
            formats: "formats",
            price: "price",
            own: "own",
        },
        content_width,
    )
}

/// Width available to a wrapped detail value, after its label gutter.
fn detail_width(content_width: u16) -> usize {
    content_width.saturating_sub(12).max(24) as usize
}

/// Break `s` into chunks of at most `width` characters, on whitespace where
/// possible so words stay intact.
fn wrap(s: &str, width: usize) -> Vec<String> {
    if s.chars().count() <= width {
        return vec![s.to_string()];
    }
    let mut out = Vec::new();
    let mut line = String::new();
    for word in s.split_whitespace() {
        let wlen = word.chars().count();
        if !line.is_empty() && line.chars().count() + 1 + wlen > width {
            out.push(std::mem::take(&mut line));
        }
        // A single word longer than the line (a url) has to be split hard.
        if wlen > width {
            if !line.is_empty() {
                out.push(std::mem::take(&mut line));
            }
            let mut rest: Vec<char> = word.chars().collect();
            while rest.len() > width {
                out.push(rest.drain(..width).collect());
            }
            line = rest.into_iter().collect();
            continue;
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        out.push(line);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// Pad or truncate to exactly `width` display columns.
fn clip_cell(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if n > width {
        let mut out: String = s.chars().take(width.saturating_sub(1)).collect();
        out.push('…');
        out
    } else {
        format!("{s:<width$}")
    }
}

fn draw_help(f: &mut Frame, app: &mut App) {
    // Sized to the content rather than a percentage: at 70% of an 80-column
    // terminal the later lines were being cut off entirely. Taller than the
    // frame it still cannot be, which is what the scrolling is for.
    let area = content_popup(f.area(), HELP_BODY);
    f.render_widget(Clear, area);

    let total = HELP_BODY.lines().count();
    let viewport = area.height.saturating_sub(2) as usize; // borders
    // The only place that knows how tall the popup ended up, so the clamp lives
    // here and is written back. That is what lets `G` ask for u16::MAX and lets
    // scrolling up respond on the first keypress rather than the hundredth.
    let scroll = clamp_help_scroll(app.help_scroll as usize, viewport, total);
    app.help_scroll = scroll as u16;

    let title = if total <= viewport {
        " HELP — esc closes ".to_string()
    } else {
        let last = (scroll + viewport).min(total);
        format!(
            " HELP {}–{last} of {total} — ↑↓ scroll, esc closes ",
            scroll + 1
        )
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(Color::Cyan))
        .title(title);
    f.render_widget(
        Paragraph::new(HELP_BODY)
            .block(block)
            .scroll((scroll as u16, 0)),
        area,
    );
}

/// First visible help line, clamped so the last page cannot be scrolled past.
fn clamp_help_scroll(scroll: usize, viewport: usize, total: usize) -> usize {
    scroll.min(total.saturating_sub(viewport))
}

/// Shown under an empty search box: one example of each operator, and the
/// explaining left to `?`.
const SEARCH_HINT: &str = " words \"…\" -not OR p: is: bpm: len:";

/// The inner width of a column on an 80-column terminal: half the screen, less
/// its two border cells. `SEARCH_HINT` has to fit inside it.
#[cfg(test)]
const NARROWEST_COLUMN: usize = 38;

const HELP_BODY: &str = "\
Two screens. Each one's selection means exactly one thing.

TRANSFER SCREEN — copy analysis from one track onto others
  Tab / Shift-Tab  Switch focus between SOURCES and DESTINATIONS
  ↑ ↓ / k j        Move cursor
  PgUp / PgDn      Page
  g / G            Jump top / bottom
  /                Filter the focused column (Esc/Enter to leave, Ctrl-U clears)
                   Searches like a web box: words are ANDed against title and
                   artist, \"quoted words\" must be adjacent, -word excludes,
                   OR alternates. p:name (or p:\"a name\") is a playlist or a
                   folder of them. is:name is a keyword —
                     local / cloud / stream   where the audio lives
                     present / missing        whether the file is on this
                                              machine (local rows only —
                                              a cloud path cannot be checked)
                     lossy / lossless         and mp3 m4a flac aiff wav
                     cues / locked            what the track already has
                   bpm: and len: take a number, a comparison or a span —
                     bpm:128          128-something, so 128.02 counts
                     bpm:>=128 bpm:<130   comparisons mean what you typed
                     bpm:120-130      a span, inclusive at both ends
                     len:3m len:>6m len:3m-6m len:4:30 len:210
                   e.g. p:\"jn next\" is:stream — what to go shopping for
                        is:local is:missing   — what moved or got deleted
                        bpm:170-176 is:lossless — tracks for the fast half
  Space            Pick a DESTINATION. The source is always the highlighted
                   SOURCES row, so there is nothing to select on that side
  c                Clear the destination selection
  a                Toggle dest auto-mode (unlocked + cueless + audio)
  f                Toggle dest fuzzy-match-from-source filter
  r                Toggle --replace
  l                Toggle --lock (set lock on dst after copy)
  R                Force-reload tracks from master.db
  s                Go shopping for the highlighted source track
  Enter            Build plans and open the confirm modal
  y / Enter        (Confirm) Apply the batch
  n / Esc / q      (Confirm) Cancel

SHOP SCREEN — find and download better copies
  Tab              Switch focus between TRACKS and OFFERS
  ↑ ↓ / k j        Move cursor in the focused pane
  /                Filter the track list, same syntax (Ctrl-U clears)
  s                Search for the highlighted track. Tap it on several and they
                   search one after another, results accumulating. On an
                   already-searched track it just shows what it found
  Space            Add the highlighted track to the basket
  S                Search every track in the basket
  c                Empty the basket
  r                Re-run one search, keeping every other result
  Enter            (Tracks) Search it   (Offers) Download it
  f                Download the highlighted offer, and queue an analysis
                   transfer from its own source track onto the download
  o                Open the offer's page in a browser (to buy it)
  y                Show the offer's stable ref, for use with the CLI
  Esc / q          Back to the transfer screen

  Markers on a track row: ✓ in the basket, ▸ source of the highlighted offer.
  The tag after them is what its search found: a count, · for nothing,
  … for still queued.
  's' and 'S' always act on the TRACKS list, whichever pane has focus.

?                  This help. ↑ ↓ / k j / PgUp / PgDn / g / G scroll it,
                   Esc or q closes it
q / Esc            Quit (from the transfer screen)
";

/// A centred popup just big enough for `body`, clamped to `area`.
///
/// Percentage sizing silently truncates: at 70% of an 80-column terminal a
/// 79-column help table lost its right-hand side and its last rows, which is how
/// the shop keys went missing from the help.
fn content_popup(area: Rect, body: &str) -> Rect {
    let widest = body.lines().map(|l| l.chars().count()).max().unwrap_or(0);
    let want_w = (widest as u16).saturating_add(2).min(area.width);
    let want_h = (body.lines().count() as u16)
        .saturating_add(2)
        .min(area.height);
    Rect {
        x: area.x + (area.width.saturating_sub(want_w)) / 2,
        y: area.y + (area.height.saturating_sub(want_h)) / 2,
        width: want_w,
        height: want_h,
    }
}

fn popup_area(area: Rect, percent_x: u16, percent_y: u16) -> Rect {
    let vertical = Layout::vertical([
        Constraint::Percentage((100 - percent_y) / 2),
        Constraint::Percentage(percent_y),
        Constraint::Percentage((100 - percent_y) / 2),
    ])
    .split(area);
    Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .split(vertical[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acquire::types::*;

    fn offer() -> Offer {
        Offer::new(
            ItemRef::new(BackendId::SoundCloud, "track/1"),
            ItemKind::Track,
            "GAY JAZZ",
            "Jane Remover - Music Baby (Hurricane Edit)",
            "https://soundcloud.com/gay-jazz/jane-remover-music-baby-hurricane-edit",
        )
    }

    #[test]
    fn the_marker_legend_describes_formats_not_price() {
        // The bug this guards: the legend said "not price-checked" while the
        // marker was derived from has_lossless(), so a soundcloud row showed
        // "?" next to a perfectly known price of "free".
        assert!(
            MARKER_LEGEND.contains("formats"),
            "legend must say what the marker means: {MARKER_LEGEND}"
        );
        assert!(
            !MARKER_LEGEND.to_lowercase().contains("price"),
            "the marker has nothing to do with price: {MARKER_LEGEND}"
        );
    }

    #[test]
    fn an_offer_can_have_a_known_price_and_unknown_formats() {
        // Exactly the screenshot case, and it is legitimate: soundcloud knows
        // everything is free at search time but formats need an extraction.
        let mut o = offer();
        o.pricing = Pricing::Free;
        o.ownership = Ownership::NotApplicable;
        assert_eq!(lossless_marker(&o), "?");
        assert_eq!(crate::acquire::render::price_cell(&o), "free");
    }

    #[test]
    fn the_marker_reflects_probed_formats() {
        let mut o = offer();
        o.formats = Some(vec![AudioFormat::Flac]);
        assert_eq!(lossless_marker(&o), "*");
        o.formats = Some(vec![AudioFormat::Mp3(Some(128))]);
        assert_eq!(lossless_marker(&o), " ");
        o.formats = None;
        assert_eq!(lossless_marker(&o), "?");
    }

    #[test]
    fn title_column_grows_with_the_terminal() {
        let narrow = title_width(90);
        let wide = title_width(200);
        assert!(wide > narrow, "{wide} should exceed {narrow}");
        // And never collapses to nothing on a small terminal.
        assert!(title_width(40) >= MIN_TITLE_W as usize);
    }

    #[test]
    fn a_row_never_overruns_its_pane() {
        // Overrun is invisible: ratatui just clips the right-hand columns, which
        // is how `price` ended up reading "name your pric" with `own` gone. So
        // measure the real string, not the arithmetic that produced it.
        for pane in [
            FIXED_NO_OWN + MIN_TITLE_W,
            77,
            79,
            FIXED_COLUMNS + MIN_TITLE_W,
            83,
            100,
            140,
            200,
        ] {
            let header = offer_header(pane);
            let row = offer_row(
                Row {
                    marker: "*",
                    backend: "bandcamp",
                    title: "Some Artist — Some Very Long Track Title Indeed",
                    formats: "FLAC AIFF WAV",
                    price: "name your price",
                    own: "yes",
                },
                pane,
            );
            assert_eq!(
                header.chars().count(),
                pane as usize,
                "header at pane {pane}: {header:?}"
            );
            assert_eq!(
                row.chars().count(),
                pane as usize,
                "row at pane {pane}: {row:?}"
            );
        }
    }

    #[test]
    fn the_header_and_the_rows_line_up() {
        // They are drawn as two separate widgets — the header is pinned outside
        // the scroll region — so nothing else forces their columns to agree.
        // By character, not by byte: an em dash in a title shifts every byte
        // offset after it without moving the column at all.
        fn col_of(s: &str, needle: &str) -> usize {
            let at = s
                .find(needle)
                .unwrap_or_else(|| panic!("no {needle:?} in {s:?}"));
            s[..at].chars().count()
        }
        let header = offer_header(120);
        let row = offer_row(
            Row {
                marker: "*",
                backend: "bandcamp",
                title: "A — B",
                formats: "FLAC",
                price: "free",
                own: "yes",
            },
            120,
        );
        assert_eq!(col_of(&header, "formats"), col_of(&row, "FLAC"));
        assert_eq!(col_of(&header, "price"), col_of(&row, "free"));
        assert_eq!(col_of(&header, "own"), col_of(&row, "yes"));
    }

    #[test]
    fn ownership_is_dropped_before_the_title_is_starved() {
        assert!(
            !shows_ownership(FIXED_NO_OWN + MIN_TITLE_W),
            "the narrowest full row has no room for `own`"
        );
        assert!(shows_ownership(FIXED_COLUMNS + MIN_TITLE_W));
        assert!(shows_ownership(200));
    }

    #[test]
    fn the_legend_fits_the_narrowest_pane_it_is_drawn_in() {
        let narrowest = (FIXED_NO_OWN + MIN_TITLE_W) as usize;
        assert!(
            MARKER_LEGEND.chars().count() <= narrowest,
            "legend is {} wide, pane is {narrowest}",
            MARKER_LEGEND.chars().count()
        );
    }

    #[test]
    fn the_search_hint_fits_the_column_it_sits_under() {
        assert!(
            SEARCH_HINT.chars().count() <= NARROWEST_COLUMN,
            "hint is {} wide, column is {NARROWEST_COLUMN}",
            SEARCH_HINT.chars().count()
        );
    }

    #[test]
    fn wrapping_keeps_words_intact_and_loses_nothing() {
        let s = "Jane Remover - Music Baby Hurricane Edit";
        let lines = wrap(s, 16);
        assert!(lines.len() > 1, "should have wrapped");
        for l in &lines {
            assert!(l.chars().count() <= 16, "line too long: {l:?}");
        }
        // Every word survives, which is the point of not truncating.
        assert_eq!(
            lines.join(" ").split_whitespace().collect::<Vec<_>>(),
            s.split_whitespace().collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_long_url_is_split_rather_than_dropped() {
        let url = "https://soundcloud.com/gay-jazz/jane-remover-music-baby-hurricane-edit";
        let lines = wrap(url, 20);
        for l in &lines {
            assert!(l.chars().count() <= 20, "line too long: {l:?}");
        }
        assert_eq!(lines.concat(), url, "no characters may be lost");
    }

    #[test]
    fn short_text_is_not_wrapped() {
        assert_eq!(wrap("short", 20), vec!["short".to_string()]);
        assert_eq!(wrap("", 20), vec![String::new()]);
    }

    #[test]
    fn the_help_popup_is_sized_to_fit_its_content() {
        // The regression: a percentage-sized popup clipped the shop keys off the
        // bottom and the right.
        let widest = HELP_BODY.lines().map(|l| l.chars().count()).max().unwrap();
        let rows = HELP_BODY.lines().count() as u16;
        // Sized off the content, so the frame has to be bigger than the help
        // rather than a fixed guess — the help grows as keys are added, and
        // scrolling is what covers the terminals it outgrows.
        let big = Rect {
            x: 0,
            y: 0,
            width: 200,
            height: rows + 10,
        };
        let r = content_popup(big, HELP_BODY);
        assert!(
            r.width as usize >= widest + 2,
            "help would be clipped horizontally"
        );
        assert!(r.height >= rows + 2, "help would be clipped vertically");
    }

    #[test]
    fn help_scroll_stops_at_the_last_page() {
        // 56 lines in a 20-line viewport: the last useful offset is 36, which
        // still fills the viewport rather than leaving it half blank.
        assert_eq!(clamp_help_scroll(0, 20, 56), 0);
        assert_eq!(clamp_help_scroll(36, 20, 56), 36);
        assert_eq!(clamp_help_scroll(999, 20, 56), 36);
        // `G` asks for the maximum and lets this decide what that means.
        assert_eq!(clamp_help_scroll(u16::MAX as usize, 20, 56), 36);
        // Content that fits never scrolls at all.
        assert_eq!(clamp_help_scroll(999, 60, 56), 0);
        // A viewport of zero (a frame with no room) must not panic or wrap.
        assert_eq!(clamp_help_scroll(999, 0, 56), 56);
    }

    #[test]
    fn the_help_scrolls_far_enough_to_reach_its_last_line() {
        // The regression this guards: the shop keys and the query reference sit
        // at the bottom, and were simply unreachable on a short terminal.
        let short = 24usize.saturating_sub(2);
        let total = HELP_BODY.lines().count();
        let last_offset = clamp_help_scroll(usize::MAX, short, total);
        assert_eq!(last_offset + short, total, "bottom line must be reachable");
    }

    #[test]
    fn the_help_popup_never_exceeds_the_frame() {
        let tiny = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 10,
        };
        let r = content_popup(tiny, HELP_BODY);
        assert!(r.width <= tiny.width && r.height <= tiny.height);
        assert!(r.x + r.width <= tiny.width && r.y + r.height <= tiny.height);
    }

    #[test]
    fn the_offer_table_scrolls_to_follow_its_cursor() {
        // 60 rows in a 20-row pane.
        assert_eq!(scroll_for(Some(0), 60, 20), 0, "top needs no scroll");
        assert_eq!(scroll_for(Some(5), 60, 20), 0, "still on the first page");
        assert_eq!(scroll_for(Some(30), 60, 20), 20, "centres the cursor");
        assert_eq!(
            scroll_for(Some(59), 60, 20),
            40,
            "the last row must be reachable, and not scrolled past"
        );
    }

    #[test]
    fn a_table_shorter_than_its_pane_never_scrolls() {
        assert_eq!(scroll_for(Some(3), 5, 20), 0);
        assert_eq!(scroll_for(None, 5, 20), 0);
        // And a zero-height pane must not underflow.
        assert_eq!(scroll_for(Some(3), 5, 0), 3);
    }

    #[test]
    fn the_help_documents_both_screens_and_their_keys() {
        for expected in [
            "TRANSFER SCREEN",
            "SHOP SCREEN",
            "Go shopping",
            "basket",
            "Back to the transfer screen",
        ] {
            assert!(HELP_BODY.contains(expected), "help is missing {expected:?}");
        }
    }

    #[test]
    fn the_help_says_what_each_screens_selection_means() {
        // The confusion this split was for: one Space key that meant "copy
        // target" in one column and "shop for this" in another. The help has to
        // state each meaning separately, or the split buys nothing.
        let (transfer, shop) = HELP_BODY.split_once("SHOP SCREEN").expect("two sections");
        assert!(
            transfer.contains("Pick a DESTINATION"),
            "the transfer section must say Space picks destinations"
        );
        assert!(
            transfer.contains("nothing to select on that side"),
            "the transfer section must say sources are not selectable"
        );
        assert!(
            shop.contains("Add the highlighted track to the basket"),
            "the shop section must say Space fills the basket"
        );
        assert!(
            !transfer.contains("basket"),
            "the basket belongs to the shop screen only"
        );
    }
}
