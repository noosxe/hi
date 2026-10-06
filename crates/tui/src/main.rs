use ratatui::crossterm::event::{self, Event, KeyCode};
use ratatui::layout::Alignment;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Wrap};

fn main() -> std::io::Result<()> {
    // `ratatui::run` does ALL the terminal ceremony for us:
    //   before: enable raw mode, switch to the alternate screen,
    //           install a panic hook that restores the terminal
    //   after:  restore the terminal (even if we panic or return an error)
    // We only supply the event loop.
    ratatui::run(|terminal| {
        loop {
            // 1. draw one frame
            terminal.draw(|frame| {
                let hello = format!("Hello, {}! Press q to quit.", hi_core::NAME);
                let paragraph = Paragraph::new(vec![
                    Line::from("ratatui is alive 🎉".bold()),
                    Line::from(hello),
                ])
                .alignment(Alignment::Center)
                .wrap(Wrap { trim: false });
                frame.render_widget(paragraph, frame.area());
            })?;

            // 2. block until an event arrives; quit on 'q'
            //    (on unix every key event is a Press, so we can skip
            //     checking key.kind here — relevant on Windows only)
            if let Event::Key(key) = event::read()? {
                if key.code == KeyCode::Char('q') {
                    return Ok(());
                }
            }
        }
    })
}
