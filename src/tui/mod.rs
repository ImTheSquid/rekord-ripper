pub mod app;
pub mod data;
pub mod diff;
pub mod events;
pub mod render;
pub mod worker;

use std::io;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, DisableMouseCapture, Event};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::db::{MasterDb, SafetyOpts};

use app::App;

pub fn run(db: MasterDb, safety: SafetyOpts) -> Result<()> {
    // A fingerprint rips streaming sources into a scratch dir, and quitting
    // mid-gate kills the thread before it can clean up. Nothing else called
    // this, so the leftovers accumulated forever.
    let _ = crate::fingerprint::ScratchDir::sweep_stale(Duration::from_secs(24 * 3600));

    let mut app = App::new(db, safety)?;
    let mut terminal = setup_terminal()?;
    install_panic_hook();

    let result = event_loop(&mut terminal, &mut app);

    // Always restore the terminal, even on error.
    let restore = restore_terminal(&mut terminal);
    result.and(restore)
}

type Term = Terminal<CrosstermBackend<io::Stdout>>;

fn event_loop(terminal: &mut Term, app: &mut App) -> Result<()> {
    while !app.should_quit {
        terminal.draw(|f| render::draw(f, app))?;
        if event::poll(Duration::from_millis(300))?
            && let Event::Key(key) = event::read()?
        {
            events::handle_key(app, key);
        }
        app.poll_rekordbox_if_due();
        // Results from the background search land here, so the UI stays
        // responsive while a backend is being slow.
        app.pump_worker();
    }
    Ok(())
}

fn setup_terminal() -> Result<Term> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    Ok(Terminal::new(backend)?)
}

fn restore_terminal(terminal: &mut Term) -> Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    Ok(())
}

/// Make sure a panic leaves the terminal usable. Without this, raw mode + alt
/// screen stay on and the user can't even see their shell.
fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
        original(info);
    }));
}
