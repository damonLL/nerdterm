mod app;
mod config;
mod events;
mod network;
mod terminal;
mod ui;

use std::io;

use anyhow::Result;
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture, EventStream},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::StreamExt;
use ratatui::prelude::*;
use tokio::sync::mpsc;

use app::App;
use events::AppEvent;

#[tokio::main]
async fn main() -> Result<()> {
    // Panic hook to restore terminal on crash
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stderr(), LeaveAlternateScreen, DisableMouseCapture);
        original_hook(panic_info);
    }));

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run(&mut terminal).await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;

    result
}

async fn run(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    let (event_tx, mut event_rx) = mpsc::channel::<AppEvent>(256);
    let mut app = App::new(event_tx);

    let size = terminal.size()?;
    app.resize(size.width, size.height);

    let mut crossterm_events = EventStream::new();

    loop {
        terminal.draw(|f| ui::draw(f, &mut app))?;

        tokio::select! {
            Some(Ok(event)) = crossterm_events.next() => {
                app.handle_crossterm_event(event).await?;
            }
            Some(event) = event_rx.recv() => {
                app.handle_app_event(event).await?;
            }
        }

        if app.should_quit() {
            break;
        }
    }

    Ok(())
}
