use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// All TUI state lives here.
///
/// The contract that keeps a TUI codebase sane as it grows:
/// event handling *mutates* this struct, rendering only *reads* it.
/// No widget, no terminal handle, no drawing inside here.
pub struct App {
    /// Set when the user wants to exit; the event loop watches this.
    pub quit: bool,
    /// Text currently being typed in the input box.
    pub input: String,
    /// Placeholder conversation transcript. Will become real
    /// message types once the core crate starts producing events.
    pub messages: Vec<String>,
}

impl App {
    pub fn new() -> Self {
        Self {
            quit: false,
            input: String::new(),
            messages: Vec::new(),
        }
    }

    /// React to one key press by mutating state.
    pub fn handle_key(&mut self, key: KeyEvent) {
        // Ctrl-C quits from anywhere — keys are typed text otherwise.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }

        match key.code {
            KeyCode::Char(c) => self.input.push(c),
            KeyCode::Backspace => {
                // `pop` removes and returns the last char; we only want
                // the removal, so discard the return value explicitly.
                let _ = self.input.pop();
            }
            KeyCode::Enter => {
                // Move the input out, leaving an empty String behind —
                // no clone needed since we own the value anyway.
                let msg = std::mem::take(&mut self.input);
                if !msg.trim().is_empty() {
                    self.messages.push(msg);
                }
            }
            _ => {}
        }
    }
}
