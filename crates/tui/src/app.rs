use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::Instant;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use hi_core::{
    DEFAULT_MAX_TOOL_ROUNDS, Event, Message, ModelsConfig, OpenAiCompatProvider, Provider,
    Settings, ThinkingLevel, ToolRegistry, spawn_agent_turn,
};

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
    /// Tools the model may call this session. Empty until built-ins
    /// land; the agent loop already runs whatever is registered here.
    pub tools: Arc<ToolRegistry>,
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
    /// Render tick — advances once per frame to animate the
    /// activity spinner.
    pub tick: u8,
    /// The `/` completion menu while composing a command. `Some`
    /// means it is visible and consumes navigation keys.
    pub completion: Option<Completion>,
    /// Messages typed mid-turn, merged into one user message that
    /// fires the moment the current reply finishes.
    pub queued: Option<String>,
    /// The resolved system prompt, re-attached whenever the model
    /// picker rebuilds the provider.
    pub system_prompt: Option<String>,
    /// The session's thinking level, re-attached whenever the model
    /// picker rebuilds the provider.
    pub thinking: Option<ThinkingLevel>,
    /// The turn in flight, if any. `Some` from Enter until TurnEnd.
    pub turn: Option<Turn>,
}

/// Braille frames for the activity spinner, cycled once per frame.
const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// One assistant reply being streamed by a provider thread.
pub struct Turn {
    /// Reply text accumulated so far — rendered live, finalized into
    /// `messages` when the turn ends.
    pub text: String,
    /// Set when the provider reported a failure for this turn;
    /// `Some` turns the finalization into an error transcript entry.
    pub error: Option<String>,
    /// When this turn started — feeds the elapsed time in the
    /// activity title.
    pub started_at: Instant,
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
        system_prompt: Option<String>,
        thinking: Option<ThinkingLevel>,
        tools: Arc<ToolRegistry>,
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
            system_prompt,
            thinking,
            tools,
            picker: None,
            completion: None,
            tick: 0,
            queued: None,
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
                // Turn already in flight: group this message into the
                // queue. Everything queued merges into ONE user
                // message (blank line between parts) that fires the
                // moment the current reply finishes — the "also add
                // tests" workflow.
                if self.turn.is_some() {
                    self.messages
                        .push(Message::System(format!("queued: {msg}")));
                    match self.queued.as_mut() {
                        Some(queued) => {
                            queued.push_str("\n\n");
                            queued.push_str(&msg);
                        }
                        None => self.queued = Some(msg),
                    }
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
                if let Some(turn) = self.turn.take()
                    && !turn.text.is_empty()
                {
                    self.messages.push(Message::Assistant(turn.text));
                }
                // Cancel means cancel everything: hand the queued
                // text back so nothing typed is lost.
                if let Some(queued) = self.queued.take() {
                    if self.input.is_empty() {
                        self.input = queued;
                    } else {
                        self.input = format!("{queued}\n\n{}", self.input);
                    }
                }
            }
            _ => {}
        }
    }

    /// Spawn the provider thread for a fresh turn. It starts running
    /// immediately; events queue up in the channel until we pump.
    fn start_turn(&mut self) {
        // The agent loop, not a bare provider round: with an empty
        // registry it behaves exactly like a single `spawn_turn`, but it
        // already knows how to execute tools and feed results back.
        let rx = spawn_agent_turn(
            Arc::clone(&self.provider),
            Arc::clone(&self.tools),
            None, // gate: permission UX arrives with the extension host
            &self.messages,
            DEFAULT_MAX_TOOL_ROUNDS,
        );
        self.turn = Some(Turn {
            text: String::new(),
            error: None,
            started_at: Instant::now(),
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
                        thinking: None,
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
        self.provider = Arc::new(
            OpenAiCompatProvider::from_config(entry, model)
                .with_system_prompt(self.system_prompt.clone())
                .with_thinking_level(self.thinking),
        );
        self.status = Status {
            provider: provider.to_string(),
            model: model.to_string(),
        };
    }

    /// Height of the open menu, borders included: pickers are 4-5
    /// rows, completion menus 3-4; 0 when no menu is open. The menu
    /// takes its rows from the streaming area (elastic 8 → 2), so it
    /// lands directly above the input.
    pub fn menu_height(&self) -> u16 {
        if let Some(picker) = &self.picker {
            (picker.items.len() as u16 + 2).clamp(5, 6)
        } else if let Some(completion) = &self.completion {
            (completion.items.len() as u16 + 2).clamp(3, 6)
        } else {
            0
        }
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
                if let Some(menu) = self.completion.as_mut()
                    && menu.selected + 1 < menu.items.len()
                {
                    menu.selected += 1;
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
            "exit" | "quit" => self.quit = true,
            "model" => self.open_picker(),
            other => self
                .messages
                .push(Message::System(format!("unknown command: /{other}"))),
        }
    }

    /// What the input's right-side title shows while work is in
    /// flight; `None` renders a plain rule. New activity states
    /// (context compaction, tool runs) become arms here.
    pub fn activity_label(&self) -> Option<String> {
        let turn = self.turn.as_ref()?;
        let spinner = SPINNER[usize::from(self.tick) % SPINNER.len()];
        let queued_note = match &self.queued {
            Some(queued) => format!(" · {} queued", queued.matches("\n\n").count() + 1),
            None => String::new(),
        };
        Some(format!(
            "{spinner} {}s{queued_note} · esc to cancel",
            turn.started_at.elapsed().as_secs()
        ))
    }
    /// Drain every event the provider thread has delivered so far.
    /// Called once per frame. `try_recv` never blocks — it reports
    /// empty, ready, or disconnected — so the UI stays responsive
    /// even in the middle of a streaming turn.
    pub fn pump(&mut self) {
        // One frame, one tick — this drives the activity spinner.
        self.tick = self.tick.wrapping_add(1);
        let Some(turn) = self.turn.as_mut() else {
            return;
        };
        let mut finished = false;
        // Messages produced by tool events queue here and land in the
        // transcript after the loop — `self.messages` can't be touched
        // while `turn` holds a borrow of `self`.
        let mut transcript: Vec<Message> = Vec::new();
        while let Ok(event) = turn.rx.try_recv() {
            match event {
                Event::TurnStart => {} // nothing to do yet — tokens follow
                Event::Token(token) => turn.text.push_str(&token),
                Event::ToolCall(call) => {
                    // Narration before a call is its own assistant
                    // message: flush the accumulated tokens so the
                    // transcript keeps text and request separate.
                    if !turn.text.is_empty() {
                        let text = std::mem::take(&mut turn.text);
                        transcript.push(Message::Assistant(text));
                    }
                    transcript.push(Message::ToolCall(call));
                }
                Event::ToolResult(result) => transcript.push(Message::ToolResult(result)),
                Event::Error(message) => turn.error = Some(message),
                Event::TurnEnd => {
                    finished = true;
                    break;
                }
            }
        }
        for message in transcript {
            self.messages.push(message);
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
            // The reply finished — fire the queued message, if any,
            // as the very next turn.
            if let Some(queued) = self.queued.take() {
                self.messages.push(Message::User(queued));
                self.start_turn();
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
            None,
            None,
            Arc::new(ToolRegistry::new()),
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
        assert!(
            completion
                .items
                .iter()
                .all(|(name, _)| name.starts_with("mo"))
        );
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

    #[test]
    fn menu_height_by_menu_kind() {
        let mut app = app();
        assert_eq!(app.menu_height(), 0);
        type_string(&mut app, "/");
        // The whole registry is listed, capped at the 6-row menu zone.
        let expected = (crate::commands::COMMANDS.len() as u16 + 2).clamp(3, 6);
        assert_eq!(app.menu_height(), expected);
        app.completion = None;
        app.open_picker(); // empty catalog clamps to the 5-row minimum
        assert_eq!(app.menu_height(), 5);
    }

    #[test]
    fn activity_label_appears_only_for_active_turns() {
        let mut app = app();
        assert!(app.activity_label().is_none());
        // A real (mock) turn: the provider thread streams in the
        // background while we inspect the title.
        app.start_turn();
        let label = app.activity_label().unwrap();
        assert!(label.contains("esc to cancel"), "{label}");
        assert!(label.contains('s'), "elapsed seconds: {label}");
    }

    #[test]
    fn exit_and_quit_commands_stop_the_session() {
        let mut first = app();
        type_string(&mut first, "/exit");
        first.handle_key(key(KeyCode::Enter));
        assert!(first.quit);

        let mut second = app();
        type_string(&mut second, "/quit");
        second.handle_key(key(KeyCode::Enter));
        assert!(second.quit);
    }

    #[test]
    fn messages_sent_mid_turn_group_into_one() {
        let mut app = app();
        app.start_turn();
        type_string(&mut app, "first part");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.queued.as_deref(), Some("first part"));
        assert!(app.activity_label().unwrap().contains("1 queued"));
        type_string(&mut app, "second part");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.queued.as_deref(), Some("first part\n\nsecond part"));
        // Both sends are acknowledged in the transcript.
        assert!(
            app.messages
                .iter()
                .any(|m| matches!(m, Message::System(s) if s.contains("first part")))
        );
        // And the activity title advertises the queue.
        assert!(app.activity_label().unwrap().contains("2 queued"));
    }

    #[test]
    fn queued_message_fires_when_the_turn_finishes() {
        let mut app = app();
        app.start_turn();
        type_string(&mut app, "follow up");
        app.handle_key(key(KeyCode::Enter));
        // Drain the mock turn in real time (its reply is short).
        for _ in 0..200 {
            app.pump();
            if app.turn.is_none() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // Both turns ran to completion inside the drain loop; the
        // proof of dispatch is the queued text becoming a real user
        // message in the transcript.
        assert!(app.turn.is_none());
        assert!(app.queued.is_none());
        assert!(
            app.messages
                .iter()
                .any(|m| matches!(m, Message::User(text) if text == "follow up"))
        );
    }

    #[test]
    fn esc_cancels_the_turn_and_restores_the_queue_to_the_input() {
        let mut app = app();
        app.start_turn();
        type_string(&mut app, "saved text");
        app.handle_key(key(KeyCode::Enter));
        app.handle_key(key(KeyCode::Esc));
        assert!(app.turn.is_none());
        assert!(app.queued.is_none());
        assert_eq!(app.input, "saved text");
    }
}
