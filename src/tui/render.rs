use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};

use crate::format::{file_type_name, format_bpm, format_length};

use super::app::{App, Focus, InputMode, StatusLevel};
use super::data::TrackRow;
use super::diff::render_pair;

pub fn draw(f: &mut Frame, app: &App) {
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

    match app.mode {
        InputMode::Confirm => draw_confirm(f, app),
        InputMode::Help => draw_help(f),
        InputMode::Shop => draw_shop(f, app),
        _ => {}
    }
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
                tags.push("auto");
            }
            if app.dst_filters.fuzzy_from_src {
                tags.push("fuzzy");
            }
            let t = if tags.is_empty() {
                String::new()
            } else {
                format!(" [{}]", tags.join("+"))
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

    // Search bar
    let search_active = matches!(app.mode, InputMode::Search(f0) if f0 == which);
    let search_caret = if search_active { "_" } else { "" };
    let search = format!(" / {}{}", state.query, search_caret);
    let style = if search_active {
        Style::new().bold()
    } else {
        Style::new().dim()
    };
    f.render_widget(Paragraph::new(search).style(style), inner_layout[0]);

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
        let marked = match which {
            Focus::Src => visible_pos == state.cursor,
            Focus::Dst => state.selected.contains(&row.id),
        };
        // "Active" = would participate in apply: src cursor row, or dst
        // selections (or dst cursor row if no explicit selection).
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
        items.push(track_item_line1(row, marked).style(row_style));
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

    let hints = "tab focus  / search  s shop  space select  a auto  f fuzzy  r replace  l lock  enter apply  ? help  q quit";
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

/// The offer table, and — the reason the worker exists — a live "searching"
/// state instead of a frozen screen.
fn draw_shop(f: &mut Frame, app: &App) {
    use super::app::{FetchState, ShopState};

    // Wide, because the point of the table is reading artist and title.
    let area = popup_area(f.area(), 96, 84);
    f.render_widget(Clear, area);

    let (title, body_lines): (String, Vec<Line>) = match &app.shop {
        ShopState::Idle => (" SHOP ".into(), vec![Line::from("nothing to show.")]),

        ShopState::Searching { since, what } => {
            // Animated from elapsed time, so it visibly advances on every 300ms
            // tick and the user can tell the difference between working and hung.
            const FRAMES: [&str; 4] = ["|", "/", "-", "\\"];
            let secs = since.elapsed().as_secs();
            let frame = FRAMES[(since.elapsed().as_millis() / 300) as usize % FRAMES.len()];
            (
                " SHOP — searching ".into(),
                vec![
                    Line::from(vec![
                        Span::styled(format!("{frame} "), Style::new().fg(Color::Cyan)),
                        Span::raw(format!("searching for {what}")),
                    ]),
                    Line::from(Span::styled(
                        format!("  {secs}s elapsed"),
                        Style::new().fg(Color::DarkGray),
                    )),
                    Line::from(""),
                    Line::from(Span::styled(
                        "  the interface stays responsive; Esc to hide this",
                        Style::new().fg(Color::DarkGray),
                    )),
                ],
            )
        }

        ShopState::Failed(why) => (
            " SHOP — failed ".into(),
            vec![Line::from(Span::styled(
                why.clone(),
                Style::new().fg(Color::Red),
            ))],
        ),

        ShopState::Results { outcome, cursor } => {
            // Give the leftover width to artist/title instead of a fixed 38, so a
            // wide terminal actually shows the name.
            let title_w = title_width(area.width);
            let fmt_w = FORMATS_W as usize;
            let mut lines = vec![Line::from(Span::styled(
                format!(
                    "{:<2}{:<11} {:<title_w$} {:<fmt_w$} {:<16} {:<4}",
                    "L", "backend", "artist / title", "formats", "price", "own"
                ),
                Style::new().add_modifier(Modifier::BOLD),
            ))];

            for (i, r) in outcome.offers.iter().enumerate() {
                let o = &r.offer;
                let selected = i == *cursor;
                let style = if selected {
                    Style::new()
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::new()
                };
                // Lossless is the whole point, so mark it in the row itself.
                let marker = lossless_marker(o);
                lines.push(Line::from(Span::styled(
                    format!(
                        "{marker} {:<11} {:<title_w$} {:<fmt_w$} {:<16} {:<4}",
                        o.backend().as_str(),
                        clip_cell(&format!("{} — {}", o.artist, o.title), title_w),
                        clip_cell(&crate::acquire::render::format_cell(o), fmt_w),
                        clip_cell(&crate::acquire::render::price_cell(o), 16),
                        crate::acquire::render::ownership_cell(o),
                    ),
                    style,
                )));
            }

            if outcome.offers.is_empty() {
                lines.push(Line::from(Span::styled(
                    "no offers found.",
                    Style::new().fg(Color::DarkGray),
                )));
            }

            // Per-backend failures, so a partial table looks partial.
            for r in outcome.failures() {
                if let Some(e) = &r.error {
                    if !e.is_silently_skippable() {
                        lines.push(Line::from(Span::styled(
                            format!("degraded: {}: {e}", r.backend),
                            Style::new().fg(Color::Yellow),
                        )));
                    }
                }
            }

            // Currencies are grouped, never converted — say so here too.
            let cheap = crate::acquire::shop::cheapest_per_currency(&outcome.offers);
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
                if crate::acquire::shop::has_mixed_currencies(&outcome.offers) {
                    lines.push(Line::from(Span::styled(
                        "different currencies are not compared — no exchange rates available",
                        Style::new().fg(Color::DarkGray),
                    )));
                }
            }

            // Full, untruncated detail for the highlighted row. The table has to
            // fit columns; this does not, so nothing is hidden behind an ellipsis.
            if let Some(r) = outcome.offers.get(*cursor) {
                let o = &r.offer;
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "selected".to_string(),
                    Style::new().add_modifier(Modifier::BOLD),
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
                    // Wrapped rather than clipped, so a long url or title is
                    // readable in full.
                    for (i, chunk) in wrap(&value, detail_width(area.width))
                        .into_iter()
                        .enumerate()
                    {
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

            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                MARKER_LEGEND,
                Style::new().fg(Color::DarkGray),
            )));
            lines.push(Line::from(Span::styled(
                "j/k move   Enter download   o open buy page   y show ref   r re-search   Esc close",
                Style::new().fg(Color::DarkGray),
            )));
            (format!(" SHOP — {} offers ", outcome.offers.len()), lines)
        }
    };

    // A download in flight is shown alongside the table, not instead of it, so
    // you can still see what you picked.
    let mut body_lines = body_lines;
    match &app.fetch {
        FetchState::Idle => {}
        FetchState::Running { since, what } => {
            const FRAMES: [&str; 4] = ["|", "/", "-", "\\"];
            let frame = FRAMES[(since.elapsed().as_millis() / 300) as usize % FRAMES.len()];
            body_lines.push(Line::from(vec![
                Span::styled(
                    format!("{frame} downloading "),
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
                body_lines.push(Line::from(Span::styled(
                    format!(
                        "saved {}",
                        p.file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default()
                    ),
                    Style::new().fg(Color::Green),
                )));
            }
            body_lines.push(Line::from(Span::styled(
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
            body_lines.push(Line::from(Span::styled(
                format!("download failed: {why}"),
                Style::new().fg(Color::Red),
            )));
        }
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(Color::Cyan))
        .title(title);
    f.render_widget(Paragraph::new(body_lines).block(block), area);
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
const MARKER_LEGEND: &str =
    "L: * has lossless, ? formats not checked yet (only the top few offers are probed)";

/// Fixed columns either side of artist/title: marker, backend, formats, price,
/// own, plus their separators.
const FIXED_COLUMNS: u16 = 2 + 11 + 1 + FORMATS_W + 1 + 16 + 1 + 4;

/// Wide enough for three format names, which is what `format_cell` shows.
const FORMATS_W: u16 = 23;

/// Width available to artist/title inside the overlay.
fn title_width(popup_width: u16) -> usize {
    let inner = popup_width.saturating_sub(2); // borders
    inner.saturating_sub(FIXED_COLUMNS).max(24) as usize
}

/// Width available to a wrapped detail value, after its label gutter.
fn detail_width(popup_width: u16) -> usize {
    popup_width.saturating_sub(2 + 12).max(24) as usize
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

fn draw_help(f: &mut Frame) {
    // Sized to the content rather than a percentage: at 70% of an 80-column
    // terminal the later lines were being cut off entirely.
    let area = content_popup(f.area(), HELP_BODY);
    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(Color::Cyan))
        .title(" HELP ");
    f.render_widget(Paragraph::new(HELP_BODY).block(block), area);
}

const HELP_BODY: &str = "\
Tab / Shift-Tab    Switch focus between SOURCES and DESTINATIONS
↑ ↓ / k j          Move cursor
PgUp / PgDn        Page
g / G              Jump top / bottom
/                  Search the focused column (Esc/Enter to leave)
s                  Shop online for the highlighted source track (runs in the
                   background; the interface stays responsive)
Ctrl-U             Clear search query (in search mode)
Space              Toggle destination selection (multi-select)
c                  Clear destination selection
a                  Toggle dest auto-mode (unlocked + cueless + audio)
f                  Toggle dest fuzzy-match-from-source filter
r                  Toggle --replace
l                  Toggle --lock (set lock on dst after copy)
R                  Force-reload tracks from master.db
Enter              Build plans and open confirm modal
y / Enter          (Confirm) Apply the batch
n / Esc / q        (Confirm) Cancel
Enter / f          (Shop) Download the highlighted offer, and queue an analysis
                   transfer from the selected source track onto it
o                  (Shop) Open the offer's page in a browser (to buy it)
y                  (Shop) Show the offer's stable ref, for use with the CLI
r                  (Shop) Re-run the search
j / k              (Shop) Move; full details for the highlighted row are shown
                   below the table
?                  This help
q / Esc            Quit
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
        let narrow = title_width(80);
        let wide = title_width(200);
        assert!(wide > narrow, "{wide} should exceed {narrow}");
        // And never collapses to nothing on a small terminal.
        assert!(title_width(40) >= 24);
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
        let big = Rect {
            x: 0,
            y: 0,
            width: 200,
            height: 60,
        };
        let r = content_popup(big, HELP_BODY);
        assert!(
            r.width as usize >= widest + 2,
            "help would be clipped horizontally"
        );
        assert!(r.height >= rows + 2, "help would be clipped vertically");
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
    fn the_help_lists_the_shop_keys() {
        for expected in ["s ", "Shop online", "(Shop) Download", "(Shop) Open"] {
            assert!(HELP_BODY.contains(expected), "help is missing {expected:?}");
        }
    }
}
