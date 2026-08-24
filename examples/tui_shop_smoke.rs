// Scratch harness: prove the TUI stays responsive while a search runs.
//
// Renders through ratatui's TestBackend so no real terminal is needed, and times
// every tick. If the search were still on the event thread, one tick would block
// for seconds; the whole point is that none of them do.
//
// Reads master.db and hits the network. Not part of the test suite.
use std::time::{Duration, Instant};

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use rekord_ripper::db::{MasterDb, SafetyOpts};
use rekord_ripper::tui::app::{App, InputMode, ShopState};
use rekord_ripper::tui::render;

fn main() -> anyhow::Result<()> {
    let db = MasterDb::open()?;
    let mut app = App::new(db, SafetyOpts::default())?;
    println!("loaded {} rows", app.rows.len());

    // Pick a source row that has a title to search for.
    let idx = app
        .src
        .visible
        .iter()
        .position(|&i| !app.rows[i].title.trim().is_empty())
        .expect("no titled track in the library");
    app.src.cursor = idx;
    app.recompute_visible();
    let track = app.current_src().expect("cursor should resolve");
    println!("searching for: {} — {}", track.artist, track.title);

    let backend = TestBackend::new(120, 40);
    let mut term = Terminal::new(backend)?;

    if !app.start_shop() {
        println!("start_shop refused: {}", app.status.text);
        return Ok(());
    }
    assert_eq!(app.mode, InputMode::Shop, "overlay should open immediately");

    let began = Instant::now();
    let mut worst_tick = Duration::ZERO;
    let mut ticks = 0u32;

    loop {
        // Exactly what event_loop does each iteration, minus the input poll.
        let t0 = Instant::now();
        term.draw(|f| render::draw(f, &app))?;
        app.poll_rekordbox_if_due();
        app.pump_worker();
        let elapsed = t0.elapsed();
        worst_tick = worst_tick.max(elapsed);
        ticks += 1;

        if matches!(app.shop, ShopState::Results { .. } | ShopState::Failed(_)) {
            break;
        }
        if began.elapsed() > Duration::from_secs(90) {
            println!("gave up waiting after 90s");
            break;
        }
        std::thread::sleep(Duration::from_millis(300));
    }

    println!(
        "\nsearch took {:.1}s over {ticks} ticks; slowest tick {:.1}ms",
        began.elapsed().as_secs_f64(),
        worst_tick.as_secs_f64() * 1000.0
    );
    println!("status: {}\n", app.status.text);

    // Download the highlighted offer, the way Enter does, and keep ticking.
    if std::env::var("RR_SMOKE_FETCH").is_ok() {
        use rekord_ripper::tui::app::FetchState;
        // Pick the first free offer, since a paid one is refused by design.
        let free = app
            .shop
            .flattened()
            .position(|(_, r)| !r.offer.requires_purchase());
        if let ShopState::Results { cursor, .. } = &mut app.shop {
            if let Some(i) = free {
                *cursor = i;
            }
        }
        println!(
            "fetching: {:?}",
            app.shop.selected().map(|r| r.offer.title.clone())
        );
        if app.start_fetch() {
            let t = Instant::now();
            let mut worst = Duration::ZERO;
            loop {
                let t0 = Instant::now();
                term.draw(|f| render::draw(f, &app))?;
                app.pump_worker();
                worst = worst.max(t0.elapsed());
                if matches!(app.fetch, FetchState::Done { .. } | FetchState::Failed(_)) {
                    break;
                }
                if t.elapsed() > Duration::from_secs(240) {
                    println!("fetch timed out");
                    break;
                }
                std::thread::sleep(Duration::from_millis(300));
            }
            println!(
                "fetch finished in {:.1}s, slowest tick {:.1}ms",
                t.elapsed().as_secs_f64(),
                worst.as_secs_f64() * 1000.0
            );
            match &app.fetch {
                FetchState::Done { paths, queued } => {
                    for p in paths {
                        println!(
                            "  saved {} ({} bytes)",
                            p.display(),
                            std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
                        );
                    }
                    println!("  queued: {queued:?}");
                }
                FetchState::Failed(e) => println!("  failed: {e}"),
                _ => {}
            }
        } else {
            println!("start_fetch refused: {}", app.status.text);
        }
    }

    // The reported bug: step away from finished results, come back with 's'.
    {
        use rekord_ripper::tui::app::InputMode as M;
        let before = app.shop.len();
        app.mode = M::Normal; // Esc
        let reopened = app.open_shop(); // s
        println!(
            "reopen after finishing: opened={reopened} mode_is_shop={} offers_kept={} (was {before})",
            app.mode == M::Shop,
            app.shop.len()
        );
        assert!(reopened && app.mode == M::Shop, "s must reopen the overlay");
        assert_eq!(app.shop.len(), before, "results must not be discarded");
        assert!(!app.shop_busy(), "reopening must not start a new search");
    }

    // The queue: tap 's' on several tracks and let them run one after another.
    if std::env::var("RR_SMOKE_QUEUE").is_ok() {
        use rekord_ripper::tui::app::ShopState as S;
        app.src.query = std::env::var("RR_SMOKE_QUEUE").unwrap_or_default();
        app.recompute_visible();
        let picks: Vec<usize> = app.src.visible.iter().take(3).copied().collect();
        println!("tapping s on {} tracks", picks.len());

        let before = app.shop.len();
        for (n, _) in picks.iter().enumerate() {
            app.src.cursor = n;
            app.recompute_visible();
            app.src.cursor = n;
            let label = app
                .current_src()
                .map(|r| format!("{} — {}", r.artist, r.title))
                .unwrap_or_default();
            let ok = app.open_shop();
            println!(
                "  s on {label:<48} accepted={ok} outstanding={}",
                app.shop_outstanding()
            );
        }
        // Pressing s again on the same track must not queue it twice.
        app.src.cursor = 0;
        app.recompute_visible();
        app.src.cursor = 0;
        let dup = app.shop_outstanding();
        app.open_shop();
        println!(
            "  s again on the first track: outstanding {dup} -> {}",
            app.shop_outstanding()
        );

        let t = Instant::now();
        let mut seen_counts = Vec::new();
        loop {
            term.draw(|f| render::draw(f, &app))?;
            app.pump_worker();
            let n = app.shop.len();
            if seen_counts.last().copied() != Some(n) {
                seen_counts.push(n);
            }
            if !app.shop_busy() {
                break;
            }
            if t.elapsed() > Duration::from_secs(240) {
                println!("queue timed out");
                break;
            }
            std::thread::sleep(Duration::from_millis(300));
        }
        println!("  offer count over time: {seen_counts:?} (started at {before})");
        if let S::Results { groups, .. } = &app.shop {
            println!("  accumulated {} groups:", groups.len());
            for g in groups.iter() {
                println!("    {:<48} {} offers", g.label, g.outcome.offers.len());
            }
        }
    }

    // Bulk: search for several visible source tracks at once.
    if std::env::var("RR_SMOKE_BULK").is_ok() {
        use rekord_ripper::tui::app::ShopState as S;
        app.src.query = std::env::var("RR_SMOKE_BULK").unwrap_or_default();
        app.recompute_visible();
        // Select a few of the filtered rows, the way space does.
        let picks: Vec<String> = app
            .src
            .visible
            .iter()
            .take(3)
            .filter_map(|&i| app.rows.get(i))
            .map(|r| r.id.clone())
            .collect();
        for id in &picks {
            app.src.selected.insert(id.clone());
        }
        println!(
            "bulk over {} selected of {} visible",
            app.src.selected.len(),
            app.src.visible.len()
        );
        if app.shop_selected(3) {
            let t = Instant::now();
            loop {
                term.draw(|f| render::draw(f, &app))?;
                app.pump_worker();
                // Wait on the worker, not on the state: results may already be
                // showing from an earlier search.
                if !app.shop_busy() {
                    break;
                }
                if t.elapsed() > Duration::from_secs(180) {
                    println!("bulk timed out");
                    break;
                }
                std::thread::sleep(Duration::from_millis(300));
            }
            if let S::Results { groups, .. } = &app.shop {
                println!("bulk groups: {}", groups.len());
                for g in groups.iter() {
                    println!("  {:<44} {} offers", g.label, g.outcome.offers.len());
                }
            }
            println!("total offers: {}", app.shop.len());
        } else {
            println!("bulk refused: {}", app.status.text);
        }
    }

    // The help screen, to confirm nothing is clipped off the bottom or right.
    app.mode = InputMode::Help;
    term.draw(|f| render::draw(f, &app))?;
    {
        let buf = term.backend().buffer().clone();
        println!("---- HELP ----");
        for y in 0..buf.area.height {
            let mut line = String::new();
            for x in 0..buf.area.width {
                line.push_str(buf[(x, y)].symbol());
            }
            let line = line.trim_end();
            if line.contains('│') || line.contains('┌') || line.contains('└') {
                println!("{line}");
            }
        }
        println!("---- /HELP ----\n");
    }
    app.mode = InputMode::Shop;

    // Show what the overlay actually looks like.
    term.draw(|f| render::draw(f, &app))?;
    let buf = term.backend().buffer().clone();
    for y in 0..buf.area.height {
        let mut line = String::new();
        for x in 0..buf.area.width {
            line.push_str(buf[(x, y)].symbol());
        }
        let line = line.trim_end();
        if !line.is_empty() {
            println!("{line}");
        }
    }
    Ok(())
}
