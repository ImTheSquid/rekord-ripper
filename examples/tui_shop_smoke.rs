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
