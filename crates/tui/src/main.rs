mod app;
mod ui;

use ratatui::crossterm::event::{self, Event};

use crate::app::App;

fn main() -> std::io::Result<()> {
    // `ratatui::run` owns all terminal ceremony:
    // raw mode + alternate screen + panic-safe restore in and out.
    // We only supply the event loop.
    ratatui::run(|terminal| {
        let mut app = App::new();

        // The heartbeat: draw current state, wait for an event,
        // let the app react, repeat until the app says stop.
        while !app.quit {
            terminal.draw(|frame| ui::draw(frame, &app))?;

            if let Event::Key(key) = event::read()? {
                app.handle_key(key);
            }
        }

        Ok(())
    })
}
