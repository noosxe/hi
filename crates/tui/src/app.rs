use std::sync::Arc;
use std::sync::mpsc::Receiver;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use hi_core::{Event, Message, ModelsConfig, OpenAiCompatProvider, Provider, Settings, spawn_turn};

use crate::catalog::Catalog;
use crate::status::{Status, location_label};
/// All TUI state lives here.
///
/// The contract that keeps a TUI codebase sane as it grows:
/// event handling *mutates* this struct (`handle_key`, `pump`),
/// rendering only *reads* it. No widget, no terminal handle, no
/// drawing inside here.
pub struct App {
    /// Set when the user wants to exit; the event loop watches this.
    pub quit: bool,
    /// Text currently being typed in the input box.
    pub input: String,
    /// The conversation transcript — finalized messages only.
    pub messages: Vec<Message>,
    /// The backend producing assistant turns. A trait object (`dyn`),
    /// so swapping mock → real LLM later is a one-line change here.
    pub provider: Arc<dyn Provider>,
    /// Status-bar facts: which provider and model this session uses.
    /// Passed in from the CLI — the trait object can't name itself.
    pub status: Status,
    /// Left side of the status bar, computed once at startup:
    /// `~/Projects/hi (main)`.
    pub location: String,
    /// What the `/model` picker can offer, gathered once at startup.
    pub catalog: Catalog,
    /// The parsed `models.json` — needed to rebuild providers when
    /// the picker switches models at runtime.
    pub config: ModelsConfig,
    /// The open `/model` picker, if any. `Some` means it owns the
    /// keyboard.
    pub picker: Option<Picker>,
    /// The `/` completion menu while composing a command. `Some`
    /// means it is visible and consumes navigation keys.
    pub completion: Option<Completion>,
    /// The turn in flight, if any. `Some` from Enter until TurnEnd.
    pub turn: Option<Turn>,
}

/// One assistant reply being streamed by a provider thread.
pub struct Turn {
    /// Reply text accumulated so far — rendered live, finalized into
    /// `messages` when the turn ends.
    pub text: String,
    /// Set when the provider reported a failure for this turn;
    /// `Some` turns the finalization into an error transcript entry.
    pub error: Option<String>,
    /// Events from the provider thread. Private on purpose: consumed
    /// only by `App::pump`, so raw events never leak into UI code.
    rx: Receiver<Event>,
}

/// The open `/model` chooser: flat `provider/model` items plus the
/// highlighted row.
pub struct Picker {
    pub items: Vec<String>,
    pub selected: usize,
}

/// The `/` completion menu: filtered commands plus the highlighted
/// row.
#[derive(Clone)]
pub struct Completion {
    pub items: Vec<(&'static str, &'static str)>,
    pub selected: usize,
}

impl App {
    pub fn new(
        provider: Arc<dyn Provider>,
        status: Status,
        catalog: Catalog,
        config: ModelsConfig,
    ) -> Self {
        Self {
            quit: false,
            input: String::new(),
            messages: Vec::new(),
            provider,
            status,
            location: location_label(),
            catalog,
            config,
            picker: None,
            completion: None,
            turn: None,
        }
    }

    /// React to one key press by mutating state.
    pub fn handle_key(&mut self, key: KeyEvent) {
        // Ctrl-C quits from anywhere — keys are typed text otherwise.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }

        // When the picker is open, it owns the keyboard.
        if self.picker.is_some() {
            self.handle_picker_key(key);
            return;
        }
        // While the `/` completion menu is open it consumes the
        // navigation keys; everything else falls through to editing.
        if self.completion.is_some() && self.handle_completion_key(key) {
            return;
        }
        match key.code {
            KeyCode::Char(c) => {
                self.input.push(c);
                self.sync_completion();
            }
            KeyCode::Backspace => {
                // `pop` removes and returns the last char; we only want
                // the removal, so discard the return value explicitly.
                let _ = self.input.pop();
                self.sync_completion();
            }
            KeyCode::Enter => {
                // One turn at a time for now: Enter while the assistant
                // is streaming is a no-op and the typed input is kept.
                // Queueing turns comes in a later milestone.
                if self.turn.is_some() {
                    return;
                }
                // Move the input out, leaving an empty String behind —
                // no clone needed since we own the value anyway.
                let msg = std::mem::take(&mut self.input);
                let trimmed = msg.trim().to_string();
                if let Some(command_text) = trimmed.strip_prefix('/') {
                    // Exact name wins; otherwise the highlighted
                    // completion speaks for the user.
                    let highlighted = self
                        .completion
                        .as_ref()
                        .and_then(|menu| menu.items.get(menu.selected))
                        .map(|(name, _)| *name);
                    let name = crate::commands::find_exact(command_text)
                        .map(|command| command.name)
                        .or(highlighted);
                    self.completion = None;
                    match name {
                        Some(name) => self.run_command(name),
                        None => self.messages.push(Message::System(format!(
                            "unknown command: /{command_text} — type / to see commands"
                        ))),
                    }
                    return;
                }
                self.completion = None;
                if msg.trim().is_empty() {
                    return;
                }
                self.messages.push(Message::User(msg));
                self.start_turn();
            }
            KeyCode::Esc => {
                // Cancel the active turn: `take` drops the Turn, which
                // drops the event Receiver, so the provider thread's
                // next send fails and it returns — the same
                // drop-is-cancellation mechanism as quitting.
                // pi's behavior: partial output stays in the transcript.
                if let Some(turn) = self.turn.take() {
                    if !turn.text.is_empty() {
                        self.messages.push(Message::Assistant(turn.text));
                    }
                }
            }
            _ => {}
        }
    }

    /// Spawn the provider thread for a fresh turn. It starts running
    /// immediately; events queue up in the channel until we pump.
    fn start_turn(&mut self) {
        let rx = spawn_turn(Arc::clone(&self.provider), &self.messages);
        self.turn = Some(Turn {
            text: String::new(),
            error: None,
            rx,
        });
    }

    /// `/model`: fill the picker from the startup catalog and
    /// preselect the current choice, if it is still listed.
    fn open_picker(&mut self) {
        let items = self.catalog.items();
        let current = format!("{}/{}", self.status.provider, self.status.model);
        let selected = items.iter().position(|item| *item == current).unwrap_or(0);
        self.picker = Some(Picker { items, selected });
    }

    /// Keyboard owner while the picker is open.
    fn handle_picker_key(&mut self, key: KeyEvent) {
        let Some(picker) = self.picker.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Esc => self.picker = None,
            KeyCode::Up => picker.selected = picker.selected.saturating_sub(1),
            KeyCode::Down => {
                if picker.selected + 1 < picker.items.len() {
                    picker.selected += 1;
                }
            }
            // pi's contract: Ctrl-S persists the choice as the startup
            // default; Enter only switches this session.
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(item) = picker.items.get(picker.selected) {
                    let settings = Settings {
                        default_model: Some(item.clone()),
                    };
                    match settings.save() {
                        Ok(()) => self
                            .messages
                            .push(Message::System(format!("default model saved: {item}"))),
                        Err(err) => self.messages.push(Message::Error(err)),
                    }
                }
            }
            KeyCode::Enter => {
                // Copy the choice out of the picker borrow first — the
                // borrow must end before `apply_model_choice` can take
                // `&mut self` whole.
                let choice = picker.items.get(picker.selected).cloned();
                if let Some(item) = choice {
                    self.apply_model_choice(item);
                }
                self.picker = None;
            }
            _ => {}
        }
    }

    /// Switch this session to `provider/model`. Safe even mid-turn:
    /// the running turn thread holds its own `Arc` to the old provider
    /// and finishes on it; only new turns pick up the new one.
    fn apply_model_choice(&mut self, item: String) {
        let Some((provider, model)) = item.split_once('/') else {
            return;
        };
        let Some(entry) = self.config.providers.get(provider) else {
            return;
        };
        self.provider = Arc::new(OpenAiCompatProvider::from_config(entry, model));
        self.status = Status {
            provider: provider.to_string(),
            model: model.to_string(),
        };
    }


    /// Rebuild the `/` completion menu from the current input: open
    /// while the text is a bare `/prefix` (no spaces yet — argument
    /// parsing is a future problem), closed otherwise.
    fn sync_completion(&mut self) {
        let Some(typed) = self.input.strip_prefix('/') else {
            self.completion = None;
            return;
        };
        if typed.contains(' ') {
            self.completion = None;
            return;
        }
        let matches = crate::commands::matching(typed);
        self.completion = if matches.is_empty() {
            None
        } else {
            Some(Completion {
                items: matches
                    .iter()
                    .map(|command| (command.name, command.description))
                    .collect(),
                selected: 0,
            })
        };
    }

    /// Key routing while the completion menu is open. Returns true
    /// when the key was consumed; anything else falls through to
    /// normal text editing, which re-syncs the menu afterwards.
    fn handle_completion_key(&mut self, key: KeyEvent) -> bool {
        // Clone the small menu out so `self` is free for the arms
        // that need it (the same NLL dance as the picker's Enter).
        let Some(completion) = self.completion.clone() else {
            return false;
        };
        match key.code {
            KeyCode::Esc => {
                // Dismiss the menu but keep the typed text.
                self.completion = None;
                true
            }
            KeyCode::Up => {
                if let Some(menu) = self.completion.as_mut() {
                    menu.selected = menu.selected.saturating_sub(1);
                }
                true
            }
            KeyCode::Down => {
                if let Some(menu) = self.completion.as_mut() {
                    if menu.selected + 1 < menu.items.len() {
                        menu.selected += 1;
                    }
                }
                true
            }
            KeyCode::Tab => {
                if let Some((name, _)) = completion.items.get(completion.selected) {
                    self.input = format!("/{name}");
                    self.sync_completion();
                }
                true
            }
            KeyCode::Enter => {
                self.completion = None;
                self.input.clear();
                if let Some((name, _)) = completion.items.get(completion.selected) {
                    self.run_command(name);
                }
                true
            }
            _ => false,
        }
    }

    /// Execute a slash command by name. One arm per registry entry —
    /// a new registry entry without an arm lands here as an honest
    /// "unknown command" instead of a silent nothing.
    fn run_command(&mut self, name: &'static str) {
        match name {
            "model" => self.open_picker(),
            other => self
                .messages
                .push(Message::System(format!("unknown command: /{other}"))),
        }
    }

    /// Drain every event the provider thread has delivered so far.
    /// Called once per frame. `try_recv` never blocks — it reports
    /// empty, ready, or disconnected — so the UI stays responsive
    /// even in the middle of a streaming turn.
    pub fn pump(&mut self) {
        let Some(turn) = self.turn.as_mut() else {
            return;
        };
        let mut finished = false;
        while let Ok(event) = turn.rx.try_recv() {
            match event {
                Event::TurnStart => {} // nothing to do yet — tokens follow
                Event::Token(token) => turn.text.push_str(&token),
                Event::Error(message) => turn.error = Some(message),
                Event::TurnEnd => {
                    finished = true;
                    break;
                }
            }
        }
        if finished {
            // `Option::take` removes the turn and leaves `None` behind.
            // The mutable borrow above has ended by now, so taking
            // ownership to move the text out is allowed.
            if let Some(turn) = self.turn.take() {
                // Partial output is still worth keeping; the error
                // entry goes below it so the failure is visible.
                if !turn.text.is_empty() {
                    self.messages.push(Message::Assistant(turn.text));
                }
                if let Some(err) = turn.error {
                    self.messages.push(Message::Error(err));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hi_core::MockProvider;
    use ratatui::crossterm::event::KeyEvent;

    fn app() -> App {
        App::new(
            Arc::new(MockProvider::default()),
            Status {
                provider: "mock".into(),
                model: "mock".into(),
            },
            Catalog::default(),
            ModelsConfig::default(),
        )
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn type_string(app: &mut App, text: &str) {
        for c in text.chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
    }

    #[test]
    fn slash_alone_lists_every_command() {
        let mut app = app();
        type_string(&mut app, "/");
        let completion = app.completion.as_ref().unwrap();
        assert!(!completion.items.is_empty());
        assert_eq!(completion.selected, 0);
    }

    #[test]
    fn completion_filters_by_prefix() {
        let mut app = app();
        type_string(&mut app, "/mo");
        let completion = app.completion.as_ref().unwrap();
        assert!(completion.items.iter().all(|(name, _)| name.starts_with("mo")));
        assert!(completion.items.iter().any(|(name, _)| *name == "model"));
    }

    #[test]
    fn plain_text_keeps_completion_closed() {
        let mut app = app();
        type_string(&mut app, "hello");
        assert!(app.completion.is_none());
    }

    #[test]
    fn enter_on_completion_executes_the_command() {
        let mut app = app();
        type_string(&mut app, "/mod");
        app.handle_key(key(KeyCode::Enter));
        // Running /model opens the picker, closes the menu, clears input.
        assert!(app.picker.is_some());
        assert!(app.completion.is_none());
        assert!(app.input.is_empty());
    }

    #[test]
    fn tab_completes_without_executing() {
        let mut app = app();
        type_string(&mut app, "/m");
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.input, "/model");
        assert!(app.picker.is_none());
    }

    #[test]
    fn esc_dismisses_the_menu_and_keeps_the_text() {
        let mut app = app();
        type_string(&mut app, "/model");
        app.handle_key(key(KeyCode::Esc));
        assert!(app.completion.is_none());
        assert_eq!(app.input, "/model");
    }

    #[test]
    fn dismissed_menu_still_dispatches_exact_command() {
        let mut app = app();
        type_string(&mut app, "/model");
        app.handle_key(key(KeyCode::Esc));
        app.handle_key(key(KeyCode::Enter));
        assert!(app.picker.is_some());
    }

    #[test]
    fn unknown_command_reports_instead_of_sending() {
        let mut app = app();
        type_string(&mut app, "/nope");
        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(app.messages.last(), Some(Message::System(_))));
        assert!(!app.messages.iter().any(|m| matches!(m, Message::User(_))));
    }
}

