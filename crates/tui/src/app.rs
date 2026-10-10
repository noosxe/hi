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
        match key.code {
            KeyCode::Char(c) => self.input.push(c),
            KeyCode::Backspace => {
                // `pop` removes and returns the last char; we only want
                // the removal, so discard the return value explicitly.
                let _ = self.input.pop();
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
                if msg.trim().is_empty() {
                    return;
                }
                // Slash commands: `/model` opens the picker instead of
                // sending a chat message. More commands will follow.
                if msg.trim() == "/model" {
                    self.open_picker();
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
