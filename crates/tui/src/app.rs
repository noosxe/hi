use ratatui::crossterm::event::{KeyCode, KeyEvent};

/// All TUI state lives here.
///
/// The contract that keeps a TUI codebase sane as it grows:
/// event handling *mutates* this struct, rendering only *reads* it.
/// No widget, no terminal handle, no drawing inside here.
pub struct App {
    /// Set when the user wants to exit; the event loop watches this.
    pub quit: bool,
    /// Placeholder conversation transcript. Will become real
    /// message types once the core crate starts producing events.
    pub messages: Vec<String>,
}

impl App {
    pub fn new() -> Self {
        Self {
            quit: false,
            messages: Vec::new(),
        }
    }

    /// React to one key press by mutating state.
    pub fn handle_key(&mut self, key: KeyEvent) {
        match key.code {
            // NOTE: temporary. Once the input box captures typing, a plain
            // 'q' must go *into* the input buffer instead of quitting —
            // quitting will move to Ctrl-C / Esc, like pi does.
            KeyCode::Char('q') => self.quit = true,
            _ => {}
        }
    }
}
