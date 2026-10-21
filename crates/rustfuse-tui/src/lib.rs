//! `rustfuse tui`: a terminal UI to manage connections and volumes, and
//! `rustfuse daemon`, which the volumes' background services run.
//!
//! It shares the Mac app's library (on a Mac, the same file), keeps each
//! volume syncing in a background service of its own, and talks to the
//! running daemons over their provider sockets, like the app.

use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyEventKind};

pub mod app;
pub mod client;
pub mod daemon;
pub mod form;
pub mod library;
pub mod places;
pub mod secrets;
pub mod service;
pub mod ui;

use app::App;
use places::Places;
use secrets::Secrets;
use service::Services;

/// Runs the TUI until the user quits. Volumes keep syncing afterwards.
pub fn run() -> Result<()> {
    let places = Places::find()?;
    places.create()?;
    let secrets = Secrets::for_platform(&places.secrets);
    let services = Services::for_platform(&places);
    let mut app = App::new(places, secrets, services, std::env::current_exe()?)?;
    let mut terminal = ratatui::try_init()?;
    let result = (|| -> Result<()> {
        while !app.quit {
            app.tick();
            terminal.draw(|f| ui::draw(f, &app))?;
            if event::poll(Duration::from_millis(150))?
                && let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                app.key(key);
            }
        }
        Ok(())
    })();
    ratatui::restore();
    result
}
