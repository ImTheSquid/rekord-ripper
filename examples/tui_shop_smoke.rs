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
use rekord_ripper::tui::app::{App, Screen, ShopState};
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
    let track = app.current_src().expect("cursor should resolve").clone();
    println!("searching for: {} — {}", track.artist, track.title);

    let backend = TestBackend::new(120, 40);
    let mut term = Terminal::new(backend)?;

    // 's' from the transfer screen: cross to the shop screen and search.
    if !app.open_shop() {
        println!("open_shop refused: {}", app.status.text);
        return Ok(());
    }
    assert_eq!(app.screen, Screen::Shop, "should be on the shop screen");
    assert_eq!(
        app.current_shop_track().map(|r| r.id.clone()),
        Some(track.id.clone()),
        "the shop list should land on the track we came from"
    );

    let began = Instant::now();
    let mut worst_tick = Duration::ZERO;
    // Separately, so a slow cold first draw cannot be mistaken for a slow loop.
    let mut worst_warm = Duration::ZERO;
    let mut ticks = 0u32;

    loop {
        // Exactly what event_loop does each iteration, minus the input poll.
        let t0 = Instant::now();
        term.draw(|f| render::draw(f, &app))?;
        app.poll_rekordbox_if_due();
        app.pump_worker();
        let elapsed = t0.elapsed();
        worst_tick = worst_tick.max(elapsed);
        if ticks >= 3 {
            worst_warm = worst_warm.max(elapsed);
        }
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
        "\nsearch took {:.1}s over {ticks} ticks; slowest tick {:.1}ms, slowest after warmup {:.1}ms",
        began.elapsed().as_secs_f64(),
        worst_tick.as_secs_f64() * 1000.0,
        worst_warm.as_secs_f64() * 1000.0
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
        let before = app.shop.len();
        app.screen = Screen::Transfer; // Esc
        let reopened = app.open_shop(); // s
        println!(
            "reopen after finishing: opened={reopened} on_shop_screen={} offers_kept={} (was {before})",
            app.screen == Screen::Shop,
            app.shop.len()
        );
        assert!(
            reopened && app.screen == Screen::Shop,
            "s must come back to the shop screen"
        );
        assert_eq!(app.shop.len(), before, "results must not be discarded");
        assert!(!app.shop_busy(), "reopening must not start a new search");
    }

    // The reported dead key: browsing offers must not drag the track cursor with
    // it, or 's' silently lands on an already-searched track every time.
    {
        use rekord_ripper::tui::app::ShopFocus;
        app.shop_focus = ShopFocus::Tracks;
        app.shop_list.cursor = 0;
        let chosen = app.current_shop_track().map(|r| r.id.clone());
        app.shop_focus = ShopFocus::Offers;
        app.shop_move(4);
        let after = app.current_shop_track().map(|r| r.id.clone());
        println!(
            "track cursor after moving 4 offers: {} (was {})",
            after.clone().unwrap_or_default(),
            chosen.clone().unwrap_or_default()
        );
        assert_eq!(
            after, chosen,
            "browsing offers must leave the track cursor alone"
        );
        println!(
            "  offer now highlighted belongs to src {:?}",
            app.shop_offer_src()
        );
    }

    // The queue: tap 's' on several tracks and let them run one after another.
    if std::env::var("RR_SMOKE_QUEUE").is_ok() {
        use rekord_ripper::tui::app::ShopState as S;
        app.shop_list.query = std::env::var("RR_SMOKE_QUEUE").unwrap_or_default();
        app.recompute_visible();
        let picks: Vec<usize> = app.shop_list.visible.iter().take(3).copied().collect();
        println!("tapping s on {} tracks", picks.len());

        let before = app.shop.len();
        for (n, _) in picks.iter().enumerate() {
            app.shop_list.cursor = n;
            let label = app
                .current_shop_track()
                .map(|r| format!("{} — {}", r.artist, r.title))
                .unwrap_or_default();
            let ok = app.shop_track();
            println!(
                "  s on {label:<48} accepted={ok} outstanding={}",
                app.shop_outstanding()
            );
        }
        // Pressing s again on the same track must not queue it twice.
        app.shop_list.cursor = 0;
        let dup = app.shop_outstanding();
        app.shop_track();
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
        app.shop_list.query = std::env::var("RR_SMOKE_BULK").unwrap_or_default();
        app.recompute_visible();
        // Fill the basket, the way space does.
        let picks: Vec<String> = app
            .shop_list
            .visible
            .iter()
            .take(3)
            .filter_map(|&i| app.rows.get(i))
            .map(|r| r.id.clone())
            .collect();
        for id in &picks {
            app.shop_list.selected.insert(id.clone());
        }
        println!(
            "basket holds {} of {} visible",
            app.shop_list.selected.len(),
            app.shop_list.visible.len()
        );

        // The reported bug: narrow the filter after filling the basket. The
        // search used to be built from the visible rows, so everything the
        // filter hid was silently dropped.
        app.shop_list.query = "zzz-matches-nothing".into();
        app.recompute_visible();
        println!(
            "  after hiding them: {} visible, basket {}, hidden {}",
            app.shop_list.visible.len(),
            app.shop_list.selected.len(),
            app.basket_hidden()
        );
        assert_eq!(
            app.basket_hidden(),
            picks.len(),
            "the filter should be hiding the whole basket"
        );

        if app.shop_selected(25) {
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
                let answered = picks
                    .iter()
                    .filter(|id| {
                        groups
                            .iter()
                            .any(|g| g.src_id.as_deref() == Some(id.as_str()))
                    })
                    .count();
                println!("  answered {answered} of the {} basket picks", picks.len());
                assert_eq!(
                    answered,
                    picks.len(),
                    "a filter must not drop basket items from the search"
                );
            }
            println!("total offers: {}", app.shop.len());
        } else {
            println!("bulk refused: {}", app.status.text);
        }
    }

    // The help screen, to confirm nothing is clipped off the bottom or right.
    app.mode = rekord_ripper::tui::app::InputMode::Help;
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
    app.mode = rekord_ripper::tui::app::InputMode::Normal;

    // Show what the shop screen actually looks like.
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
