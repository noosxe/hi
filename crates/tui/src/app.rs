use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::Instant;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use hi_core::{
    DEFAULT_MAX_TOOL_ROUNDS, Event, Message, ModelsConfig, OpenAiCompatProvider, Provider, Session,
    Settings, ThinkingLevel, ToolRegistry, spawn_agent_turn,
};

use crate::catalog::Catalog;
/// All TUI state lives here.
///
/// The contract that keeps a TUI codebase sane as it grows:
/// event handling *mutates* this struct (`handle_key`, `pump`),
/// rendering only *reads* it. No widget, no terminal handle, no
/// drawing inside here.
use crate::editor;
use crate::status::{Status, location_label};

/// Prompt-history ceiling (AD-0014).
const HISTORY_CAP: usize = 100;

/// The paste chord: Ctrl+V always; Cmd+V too where the kitty
/// keyboard protocol reports it (`KeyModifiers::SUPER` — the terminal
/// only forwards the chord in that mode, so this can never collide
/// with a terminal that still owns Cmd+V for its own text paste).
fn paste_key(key: &KeyEvent) -> bool {
    key.code == KeyCode::Char('v')
        && (key.modifiers.contains(KeyModifiers::CONTROL)
            || key.modifiers.contains(KeyModifiers::SUPER))
}

#[cfg(test)]
pub(crate) fn test_app() -> App {
    App::new(
        Arc::new(hi_core::MockProvider::default()),
        Status {
            provider: "mock".into(),
            model: "mock".into(),
            tool_count: 0,
        },
        Catalog::default(),
        ModelsConfig::default(),
        crate::SessionInit {
            system_prompt: None,
            thinking: None,
            tools: Arc::new(ToolRegistry::new()),
            messages: Vec::new(),
            session: None,
            images: hi_core::ImageGate::new(false),
            images_setting: true,
        },
        false,
    )
}

pub struct App {
    /// Set when the user wants to exit; the event loop watches this.
    pub quit: bool,
    /// Text currently being typed in the input box.
    pub input: String,
    /// Byte offset into `input` where editing happens — always on a
    /// `char` boundary. The rules live in `editor` (AD-0014).
    pub cursor: usize,
    /// Submitted input, oldest first, for Up/Down recall. Everything
    /// non-empty that went through Enter — commands included, shell
    /// parity (AD-0014). Capped at [`HISTORY_CAP`].
    pub history: Vec<String>,
    /// Position while browsing history: `Some(index)` from the first
    /// Up until Down passes the newest entry.
    pub history_pos: Option<usize>,
    /// The in-progress input, parked when history browsing starts and
    /// restored when it ends.
    pub history_draft: Option<String>,
    /// The conversation transcript — finalized messages only.
    pub messages: Vec<Message>,
    /// The backend producing assistant turns. A trait object (`dyn`),
    /// so swapping mock → real LLM later is a one-line change here.
    pub provider: Arc<dyn Provider>,
    /// The user's attribution opt-in, resolved at startup — applied to
    /// every provider rebuild here (/model, /resume) so the stamp the
    /// CLI put on at launch is never dropped.
    telemetry: bool,
    /// Tools the model may call this session. Empty until built-ins
    /// land; the agent loop already runs whatever is registered here.
    pub tools: Arc<ToolRegistry>,
    /// The open session file, appended at every turn end. `None` when
    /// persistence is unavailable (no data dir, unwritable path) — chat
    /// must never depend on the disk.
    pub session: Option<Session>,
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
    /// The open `/resume` session picker. `Some` means it owns the
    /// keyboard and the whole live area above the status row.
    pub session_picker: Option<SessionPicker>,
    /// The flushed-prefix transcript window (AD-0016): cached
    /// display blocks, the fold pointer, and the flush cursor. The
    /// event loop syncs it against `messages` and drains flush chunks
    /// from it; a session switch refolds so the new history replays
    /// below the old conversation.
    pub(crate) scroll: crate::scroll::Scroll,
    /// An in-flight clipboard read (Ctrl-V). `Some` until the thread
    /// reports and `pump` applies the outcome — a second Ctrl-V while
    /// one is pending is ignored, not queued.
    pub clipboard_rx: Option<std::sync::mpsc::Receiver<crate::clipboard::ClipboardOutcome>>,
    /// A transient composer note (e.g. "clipboard unavailable"), shown
    /// in the input's title and cleared by the next keypress. A failed
    /// paste is not an event worth persisting — never enters the
    /// transcript or the session.
    pub note: Option<String>,
    /// The shared image-attachment gate (the same Arc inside `read`).
    /// Flipped on model switches — the registry outlives them, the
    /// policy follows (AD-0012).
    pub images: hi_core::ImageGate,
    /// The `settings.images` master switch, resolved at startup; the
    /// switch-time re-resolution must honor it.
    images_setting: bool,
    /// Session-storage root override — tests point this at a temp
    /// directory; production uses the real data dir (`None`).
    pub session_root: Option<std::path::PathBuf>,
}

/// The open `/resume` chooser: this project's sessions, their loaded
/// first-message previews, the filter text, and the highlighted row —
/// an index into `summaries`; the visible subset is derived per frame.
pub struct SessionPicker {
    pub summaries: Vec<hi_core::session::SessionSummary>,
    pub previews: Vec<Option<String>>,
    pub filter: String,
    pub selected: usize,
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
        init: crate::SessionInit,
        telemetry: bool,
    ) -> Self {
        let crate::SessionInit {
            system_prompt,
            thinking,
            tools,
            messages,
            session,
            images,
            images_setting,
        } = init;
        Self {
            quit: false,
            input: String::new(),
            cursor: 0,
            history: Vec::new(),
            history_pos: None,
            history_draft: None,
            messages,
            provider,
            status,
            location: location_label(),
            catalog,
            config,
            system_prompt,
            thinking,
            telemetry,
            tools,
            session,
            picker: None,
            completion: None,
            tick: 0,
            queued: None,
            turn: None,
            session_picker: None,
            clipboard_rx: None,
            note: None,
            images,
            images_setting,
            scroll: crate::scroll::Scroll::new(),
            session_root: None,
        }
    }

    /// React to one key press by mutating state.
    pub fn handle_key(&mut self, key: KeyEvent) {
        // The composer note is transient by definition: any keypress
        // supersedes it.
        self.note = None;
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
        // The session picker likewise: it takes over everything above
        // the status row while open.
        if self.session_picker.is_some() {
            self.handle_session_picker_key(key);
            return;
        }
        // While the `/` completion menu is open it consumes the
        // navigation keys; everything else falls through to editing.
        if self.completion.is_some() && self.handle_completion_key(key) {
            return;
        }
        // Ctrl-V pastes from the system clipboard off-thread. The
        // outcome stages a *reference* — text, or a temp-file path —
        // into the input; nothing sends until Enter (AD-0011). Cmd+V
        // aliases it where the kitty keyboard protocol lets the event
        // through (AD-0013); elsewhere the terminal pastes text only
        // and this arm never fires for Cmd.
        if paste_key(&key) {
            self.request_paste();
            return;
        }
        match key.code {
            // --- editing chords (AD-0014). The editor helpers take
            // the input as a plain &str so every rule is testable
            // without an App; the App shuttles only the cursor field.
            KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) => match c {
                'a' => self.cursor = 0,
                'e' => self.cursor = self.input.len(),
                'b' => {
                    self.cursor =
                        editor::prev_boundary(&self.input, self.cursor).unwrap_or(self.cursor);
                }
                'f' => {
                    self.cursor =
                        editor::next_boundary(&self.input, self.cursor).unwrap_or(self.cursor);
                }
                // Ctrl+D and Delete forward-delete the same unit.
                'd' => {
                    if let Some(next) = editor::next_boundary(&self.input, self.cursor) {
                        self.cursor = editor::delete_range(&mut self.input, self.cursor..next);
                    }
                }
                'k' => {
                    let end = self.input.len();
                    self.cursor = editor::delete_range(&mut self.input, self.cursor..end);
                }
                'u' => {
                    self.cursor = editor::delete_range(&mut self.input, 0..self.cursor);
                }
                'w' => {
                    let range = editor::word_delete_range_back(&self.input, self.cursor);
                    self.cursor = editor::delete_range(&mut self.input, range);
                }
                // Unbound control chords keep their old behavior: the
                // character lands in the input (ctrl+c quits and
                // ctrl+v/cmd+v paste, both handled above).
                _ => {
                    self.cursor = editor::insert_char(&mut self.input, self.cursor, c);
                    self.sync_completion();
                }
            },
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::ALT) => {
                let range = editor::word_delete_range_fwd(&self.input, self.cursor);
                self.cursor = editor::delete_range(&mut self.input, range);
                self.sync_completion();
            }
            // Readline word motions: ESC b / ESC f arrive decoded as
            // Alt+B / Alt+F — what Ghostty's macOS defaults send for
            // option+arrows (#63). Alt+arrows themselves stay bound
            // below for terminals that report the chord.
            KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::ALT) => {
                self.cursor = editor::word_left(&self.input, self.cursor);
            }
            KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::ALT) => {
                self.cursor = editor::word_right(&self.input, self.cursor);
            }
            KeyCode::Left => {
                self.cursor = if key.modifiers.contains(KeyModifiers::SUPER) {
                    // Cmd+Left: start of input — the macOS text
                    // convention, reportable under the kitty keyboard
                    // protocol (AD-0013).
                    0
                } else if key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                {
                    editor::word_left(&self.input, self.cursor)
                } else {
                    editor::prev_boundary(&self.input, self.cursor).unwrap_or(self.cursor)
                };
            }
            KeyCode::Right => {
                self.cursor = if key.modifiers.contains(KeyModifiers::SUPER) {
                    self.input.len()
                } else if key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                {
                    editor::word_right(&self.input, self.cursor)
                } else {
                    editor::next_boundary(&self.input, self.cursor).unwrap_or(self.cursor)
                };
            }
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.input.len(),
            // Prompt history: up into the past (draft parked on
            // entry), down toward it — past the newest, the draft
            // returns (AD-0014).
            KeyCode::Up => self.history_up(),
            KeyCode::Down => self.history_down(),
            KeyCode::Backspace => {
                if key.modifiers.contains(KeyModifiers::ALT) {
                    // The same unit the move lands on, so
                    // Alt+Backspace composes with Alt+Left.
                    let range = editor::word_delete_range_back(&self.input, self.cursor);
                    self.cursor = editor::delete_range(&mut self.input, range);
                } else if let Some(prev) = editor::prev_boundary(&self.input, self.cursor) {
                    self.cursor = editor::delete_range(&mut self.input, prev..self.cursor);
                }
                self.sync_completion();
            }
            KeyCode::Delete => {
                if key.modifiers.contains(KeyModifiers::ALT) {
                    let range = editor::word_delete_range_fwd(&self.input, self.cursor);
                    self.cursor = editor::delete_range(&mut self.input, range);
                } else if let Some(next) = editor::next_boundary(&self.input, self.cursor) {
                    self.cursor = editor::delete_range(&mut self.input, self.cursor..next);
                }
                self.sync_completion();
            }
            KeyCode::Char(c) => {
                self.cursor = editor::insert_char(&mut self.input, self.cursor, c);
                self.sync_completion();
            }
            KeyCode::Enter => {
                // Move the input out, leaving an empty String behind —
                // no clone needed since we own the value anyway.
                let msg = std::mem::take(&mut self.input);
                self.cursor = 0;
                let trimmed = msg.trim().to_string();
                // A leading `/` is a command only when the rest could
                // be one — command names never contain `/`, but staged
                // paste paths always start with one (`/tmp/hi-clipboard-…`,
                // AD-0011). `/tmp` alone must send as a message, and a
                // typo'd `/modle` still earns its teaching error.
                // Every non-empty submission lands in prompt
                // history — commands included, shell parity (AD-0014).
                if !trimmed.is_empty() {
                    if self.history.len() >= HISTORY_CAP {
                        self.history.remove(0);
                    }
                    self.history.push(msg.clone());
                }
                self.history_pos = None;
                self.history_draft = None;
                let command_text = trimmed.strip_prefix('/').filter(|rest| !rest.contains('/'));
                if let Some(command_text) = command_text {
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
                        self.set_input(queued);
                    } else {
                        self.set_input(format!("{queued}\n\n{}", self.input));
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
            KeyCode::Up => {
                // pi's SelectList wraps at both ends.
                picker.selected = if picker.selected == 0 {
                    picker.items.len().saturating_sub(1)
                } else {
                    picker.selected - 1
                };
            }
            KeyCode::Down => {
                picker.selected = (picker.selected + 1) % picker.items.len().max(1);
            }
            // pi's contract: Ctrl-S persists the choice as the startup
            // default; Enter only switches this session.
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(item) = picker.items.get(picker.selected) {
                    // Load-modify-save: only the model choice is written;
                    // every other user setting survives (#45).
                    match Settings::persist_default_model(item) {
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

    /// Rebuild the OpenAI-compat provider from the live session facts
    /// — system prompt, thinking, tools — plus the wire-identity
    /// stamps: the telemetry opt-in and the prompt-cache routing key.
    /// One construction path for every post-startup rebuild (/model,
    /// /resume) so a switch can never drop a stamp startup applied.
    /// Mock providers (no config entry) are left alone.
    fn rebuild_provider(&mut self, provider: &str, model: &str, cache_key: Option<String>) {
        let Some(entry) = self.config.providers.get(provider) else {
            return;
        };
        // The image policy follows the model (AD-0012): same gate the
        // registry's `read` holds, re-resolved per switch — configured
        // models honor `vision: false`, discovered ones read as
        // vision-capable, and the settings master switch still wins.
        self.images
            .set(entry.images_allowed(model, self.images_setting));
        self.provider = Arc::new(
            OpenAiCompatProvider::from_config(entry, model.to_string())
                .with_system_prompt(self.system_prompt.clone())
                .with_thinking_level(self.thinking)
                // The switch must not drop the tool registry from the
                // wire — re-attach exactly what startup attached.
                .with_tools(Arc::clone(&self.tools))
                .with_telemetry(self.telemetry)
                .with_cache_key(cache_key),
        );
    }

    /// Switch this session to `provider/model`. Safe even mid-turn:
    /// the running turn thread holds its own `Arc` to the old provider
    /// and finishes on it; only new turns pick up the new one.
    fn apply_model_choice(&mut self, item: String) {
        let Some((provider, model)) = item.split_once('/') else {
            return;
        };
        // Cache routing follows the live session across model switches.
        let cache_key = self.session.as_ref().map(|s| s.id().to_string());
        self.rebuild_provider(provider, model, cache_key);
        self.status = Status {
            provider: provider.to_string(),
            model: model.to_string(),
            tool_count: self.status.tool_count,
        };
    }

    /// Height of the open menu, borders included: pickers are 4-5
    /// rows, completion menus 3-4; 0 when no menu is open. The menu
    /// takes its rows from the streaming area (elastic 8 → 2), so it
    /// lands directly above the input.
    pub fn menu_height(&self) -> u16 {
        if let Some(picker) = &self.picker {
            // SelectList geometry: five visible rows, the `(n/m)`
            // count row only on overflow, and the key-hint footer.
            if picker.items.is_empty() {
                2 // "no models available" + hint
            } else {
                let visible = (picker.items.len() as u16).min(5);
                visible + u16::from(picker.items.len() > 5) + 1
            }
        } else if let Some(completion) = &self.completion {
            // SelectList geometry: five visible rows, plus the
            // `(n/m)` scroll row only when the matches overflow them.
            let visible = (completion.items.len() as u16).min(5);
            visible + u16::from(completion.items.len() > 5)
        } else {
            0
        }
    }

    /// Replace the whole input, cursor parked at the end — the shape
    /// every whole-text assignment wants (completions, queue
    /// handback, history loads).
    fn set_input(&mut self, text: String) {
        self.cursor = text.len();
        self.input = text;
    }

    /// The input split at the cursor — the two halves paste insertion
    /// works between. Owned copies so the caller's borrow ends before
    /// it reassigns.
    fn split_at_cursor(&self) -> (String, String) {
        (
            self.input[..self.cursor].to_string(),
            self.input[self.cursor..].to_string(),
        )
    }

    /// Step back through prompt history. Entering from fresh input
    /// parks the draft; a loaded entry puts the cursor at the start
    /// so the first keystrokes fix up its beginning (pi's rule,
    /// AD-0014).
    fn history_up(&mut self) {
        if self.history.is_empty() {
            return;
        }
        match self.history_pos {
            None => {
                let draft = std::mem::take(&mut self.input);
                self.history_draft = Some(draft);
                self.history_pos = Some(self.history.len() - 1);
                self.input = self.history[self.history.len() - 1].clone();
                self.cursor = 0;
            }
            Some(i) if i > 0 => {
                self.history_pos = Some(i - 1);
                self.input = self.history[i - 1].clone();
                self.cursor = 0;
            }
            Some(_) => {} // already at the oldest entry
        }
    }

    /// Step forward through prompt history; past the newest entry the
    /// parked draft returns, cursor at the end — as if you'd never
    /// left.
    fn history_down(&mut self) {
        let Some(i) = self.history_pos else {
            return;
        };
        if i + 1 >= self.history.len() {
            self.history_pos = None;
            self.input = self.history_draft.take().unwrap_or_default();
            self.cursor = self.input.len();
        } else {
            self.history_pos = Some(i + 1);
            self.input = self.history[i + 1].clone();
            self.cursor = 0;
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
                    // pi wraps: up from the top lands on the last row.
                    menu.selected = if menu.selected == 0 {
                        menu.items.len() - 1
                    } else {
                        menu.selected - 1
                    };
                }
                true
            }
            KeyCode::Down => {
                if let Some(menu) = self.completion.as_mut() {
                    menu.selected = (menu.selected + 1) % menu.items.len();
                }
                true
            }
            KeyCode::Tab => {
                if let Some((name, _)) = completion.items.get(completion.selected) {
                    self.set_input(format!("/{name}"));
                    self.sync_completion();
                }
                true
            }
            KeyCode::Enter => {
                self.completion = None;
                self.input.clear();
                self.cursor = 0;
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
            "resume" => self.open_session_picker(),
            "tools" => self
                .messages
                .push(Message::System(Self::tools_listing(&self.tools))),
            other => self
                .messages
                .push(Message::System(format!("unknown command: /{other}"))),
        }
    }

    /// The `/tools` listing: one line per registered tool — name, its
    /// annotation badges when the tool claims any (the derived default is
    /// deliberately unlabeled), and the first sentence of its description
    /// (the whole paragraph is model-facing and too chatty for the
    /// transcript). Sorted — the registry's BTreeMap order. Rendered as a
    /// dim `Message::System` notice: local-only, never sent to the
    /// provider.
    fn tools_listing(tools: &ToolRegistry) -> String {
        let info = tools.info();
        if info.is_empty() {
            return "no tools registered".into();
        }
        let mut lines = vec![format!("registered tools ({})", info.len())];
        for tool in info {
            let mut badges: Vec<&str> = Vec::new();
            if tool.annotations.read_only {
                badges.push("read-only");
            }
            if tool.annotations.destructive {
                badges.push("destructive");
            }
            let first_sentence = tool
                .description
                .split_once(". ")
                .map_or(tool.description.as_str(), |(first, _)| first);
            let badge = if badges.is_empty() {
                String::new()
            } else {
                format!(" ({})", badges.join(", "))
            };
            lines.push(format!("  {}{} — {first_sentence}", tool.name, badge));
        }
        lines.join("\n")
    }

    /// Open the `/resume` picker over this project's sessions. Refused
    /// mid-turn: switching under a streaming reply would interleave
    /// two sessions' provider events.
    fn open_session_picker(&mut self) {
        if self.turn.is_some() {
            self.messages.push(Message::System(
                "cannot switch sessions while a turn is running — esc first".into(),
            ));
            return;
        }
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let listed = match self.session_root.as_deref() {
            Some(root) => hi_core::session::sessions_in_root(root, &cwd),
            None => hi_core::session::sessions_in(&cwd),
        };
        let summaries = match listed {
            Ok(list) if !list.is_empty() => list,
            Ok(_) => {
                self.messages.push(Message::System(
                    "no previous sessions in this directory".into(),
                ));
                return;
            }
            Err(err) => {
                self.messages.push(Message::Error(err));
                return;
            }
        };
        let previews = summaries
            .iter()
            .map(|summary| crate::picker::preview_of(&summary.path))
            .collect();
        self.session_picker = Some(SessionPicker {
            summaries,
            previews,
            filter: String::new(),
            selected: 0,
        });
    }

    /// The summaries passing the current filter, in listing order.
    pub fn session_picker_visible(&self) -> Vec<usize> {
        let Some(picker) = &self.session_picker else {
            return Vec::new();
        };
        (0..picker.summaries.len())
            .filter(|index| {
                crate::picker::filter_matches(
                    &picker.filter,
                    &picker.summaries[*index],
                    picker.previews[*index].as_deref(),
                )
            })
            .collect()
    }

    /// Keyboard owner while the session picker is open.
    fn handle_session_picker_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.session_picker = None,
            KeyCode::Up => {
                // pi's SelectList wraps at both ends.
                let len = self.session_picker_visible().len();
                if let Some(picker) = self.session_picker.as_mut() {
                    picker.selected = if picker.selected == 0 {
                        len.saturating_sub(1)
                    } else {
                        picker.selected - 1
                    };
                }
            }
            KeyCode::Down => {
                let len = self.session_picker_visible().len();
                if let Some(picker) = self.session_picker.as_mut() {
                    picker.selected = (picker.selected + 1) % len.max(1);
                }
            }
            KeyCode::Enter => {
                let visible = self.session_picker_visible();
                let Some(picker) = self.session_picker.take() else {
                    return;
                };
                if let Some(index) = visible.get(picker.selected) {
                    let path = picker.summaries[*index].path.clone();
                    self.switch_session(&path);
                }
            }
            KeyCode::Backspace => {
                if let Some(picker) = self.session_picker.as_mut() {
                    let _ = picker.filter.pop();
                }
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(picker) = self.session_picker.as_mut() {
                    picker.filter.push(c);
                }
            }
            _ => {}
        }
    }

    /// Swap the live session for another file: reopen it for
    /// appending, replace the transcript (with the same resumed-from
    /// note the startup path uses), and reset the print cursor so the
    /// event loop renders the new history below the old scrollback.
    fn switch_session(&mut self, path: &std::path::Path) {
        let opened = Session::resume(path).and_then(|session| {
            let loaded = hi_core::session::load(path)?;
            Ok((session, loaded))
        });
        let (session, loaded) = match opened {
            Ok(opened) => opened,
            Err(err) => {
                self.messages.push(Message::Error(err));
                return;
            }
        };
        let count = loaded.messages.len();
        let mut messages = loaded.messages;
        messages.insert(
            0,
            Message::System(format!("resumed {count} messages from {}", path.display())),
        );
        self.messages = messages;
        // Cache routing follows the session: the new id becomes the
        // prompt-cache key before any further turn is spawned.
        let cache_key = session.id().to_string();
        self.session = Some(session);
        let (provider, model) = (self.status.provider.clone(), self.status.model.clone());
        self.rebuild_provider(&provider, &model, Some(cache_key));
        self.scroll.refold();
        self.queued = None;
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
    /// Stage a clipboard read, unless one is already in flight.
    pub fn request_paste(&mut self) {
        if self.clipboard_rx.is_none() {
            self.begin_clipboard_request(crate::clipboard::spawn_clipboard_read());
        }
    }

    /// Test seam: stage a clipboard read whose outcome is controlled by
    /// the caller instead of the real system clipboard.
    pub fn begin_clipboard_request(
        &mut self,
        rx: std::sync::mpsc::Receiver<crate::clipboard::ClipboardOutcome>,
    ) {
        self.clipboard_rx = Some(rx);
    }

    /// A bracketed-paste event: insert verbatim. This is text paste —
    /// images never arrive this way (they have no keystroke encoding);
    /// Ctrl-V reads the clipboard for those.
    pub fn handle_paste(&mut self, text: &str) {
        let (before, after) = self.split_at_cursor();
        self.input = crate::clipboard::insert_pasted(&before, text, &after);
        self.cursor = self.input.len() - after.len();
    }

    /// Apply one clipboard outcome: a staged path or text joins the
    /// input; failures become the transient composer note. A failed
    /// paste never touches the transcript or the session.
    pub fn handle_clipboard_outcome(&mut self, outcome: crate::clipboard::ClipboardOutcome) {
        use crate::clipboard::ClipboardOutcome;
        match outcome {
            ClipboardOutcome::Image { path } => {
                let (before, after) = self.split_at_cursor();
                self.input = crate::clipboard::insert_pasted(&before, &path, &after);
                self.cursor = self.input.len() - after.len();
            }
            ClipboardOutcome::Text(text) if !text.is_empty() => {
                let (before, after) = self.split_at_cursor();
                self.input = crate::clipboard::insert_pasted(&before, &text, &after);
                self.cursor = self.input.len() - after.len();
            }
            // Empty text inserts nothing — a no-op, not an event.
            ClipboardOutcome::Text(_) => {}
            ClipboardOutcome::Empty => {
                self.note = Some("clipboard holds no image or text".into());
            }
            ClipboardOutcome::Unavailable(e) => {
                self.note = Some(format!("clipboard unavailable: {e}"));
            }
        }
    }

    pub fn pump(&mut self) {
        // One frame, one tick — this drives the activity spinner.
        self.tick = self.tick.wrapping_add(1);
        // Clipboard outcomes first: a staged path should be in the
        // input by the time the next frame renders.
        if let Some(rx) = self.clipboard_rx.take() {
            let mut received = false;
            while let Ok(outcome) = rx.try_recv() {
                self.handle_clipboard_outcome(outcome);
                received = true;
            }
            if !received {
                // The thread is still reading; keep listening.
                self.clipboard_rx = Some(rx);
            }
        }
        let Some(turn) = self.turn.as_mut() else {
            return;
        };
        let mut finished = false;
        let added_from = self.messages.len();
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
        // Persist everything finalized this frame. An append failure
        // degrades once, loudly — the session is dropped so the note is
        // not spammed every frame, and chat continues without disk.
        let append_error = self
            .session
            .as_mut()
            .and_then(|session| session.append(&self.messages[added_from..]).err());
        if let Some(err) = append_error {
            self.session = None;
            self.messages
                .push(Message::System(format!("session not saved: {err}")));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyEvent;
    use std::path::PathBuf;

    fn app() -> App {
        test_app()
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    #[test]
    fn ctrl_v_stages_and_pump_applies_the_staged_path() {
        let mut app = app();
        let (tx, rx) = std::sync::mpsc::channel();
        app.begin_clipboard_request(rx);
        app.handle_key(ctrl(KeyCode::Char('v')));
        // The staged reference is not the send — nothing leaves the
        // composer until Enter, and the pending read is not replaced.
        assert!(app.clipboard_rx.is_some());
        assert_eq!(app.input, "");
        tx.send(crate::clipboard::ClipboardOutcome::Image {
            path: "/tmp/hi-clipboard-1.png".into(),
        })
        .unwrap();
        app.pump();
        assert!(app.clipboard_rx.is_none());
        assert_eq!(app.input, "/tmp/hi-clipboard-1.png");
        // And the staged path sends as plain text: Enter → Message::User.
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            app.messages[0],
            Message::User("/tmp/hi-clipboard-1.png".into())
        );
    }

    #[test]
    fn paste_chord_covers_ctrl_v_and_cmd_v_only() {
        use KeyModifiers as M;
        assert!(paste_key(&KeyEvent::new(KeyCode::Char('v'), M::CONTROL)));
        assert!(paste_key(&KeyEvent::new(KeyCode::Char('v'), M::SUPER)));
        // A terminal that reports both (protocol edge) still pastes.
        assert!(paste_key(&KeyEvent::new(
            KeyCode::Char('v'),
            M::CONTROL | M::SUPER
        )));
        // Everything else is typing.
        assert!(!paste_key(&KeyEvent::new(KeyCode::Char('v'), M::NONE)));
        assert!(!paste_key(&KeyEvent::new(KeyCode::Char('v'), M::SHIFT)));
        assert!(!paste_key(&KeyEvent::new(KeyCode::Char('c'), M::SUPER)));
        assert!(!paste_key(&KeyEvent::new(KeyCode::Char('x'), M::CONTROL)));
    }

    #[test]
    fn a_second_ctrl_v_while_pending_is_ignored() {
        let mut app = app();
        let (_tx, rx) = std::sync::mpsc::channel();
        app.begin_clipboard_request(rx);
        app.handle_key(ctrl(KeyCode::Char('v')));
        assert!(app.clipboard_rx.is_some());
    }

    #[test]
    fn clipboard_failure_is_a_transient_note_not_an_event() {
        let mut app = app();
        let (tx, rx) = std::sync::mpsc::channel();
        app.begin_clipboard_request(rx);
        tx.send(crate::clipboard::ClipboardOutcome::Unavailable(
            "no clipboard".into(),
        ))
        .unwrap();
        app.pump();
        assert_eq!(
            app.note.as_deref(),
            Some("clipboard unavailable: no clipboard")
        );
        // A failed paste never enters the transcript or the session.
        assert!(app.messages.is_empty());
        // The next keypress supersedes the note.
        app.handle_key(key(KeyCode::Char('h')));
        assert_eq!(app.note, None);
        assert_eq!(app.input, "h");
    }

    #[test]
    fn model_switches_re_resolve_the_image_policy() {
        let mut app = app();
        let mut providers = std::collections::BTreeMap::new();
        providers.insert(
            "p".to_string(),
            hi_core::ProviderConfig {
                base_url: "http://x/v1".into(),
                api_key: None,
                models: vec![
                    hi_core::ModelConfig {
                        id: "blind".into(),
                        vision: false,
                        ..Default::default()
                    },
                    hi_core::ModelConfig {
                        id: "seer".into(),
                        ..Default::default()
                    },
                ],
            },
        );
        app.config = ModelsConfig { providers };
        app.images = hi_core::ImageGate::new(true);
        // Discovered model (no entry): vision-capable — pi's `!model` arm.
        app.apply_model_choice("p/unknown".into());
        assert!(app.images.allowed());
        // A configured opt-out is honored, on the same Arc `read` holds.
        app.apply_model_choice("p/blind".into());
        assert!(!app.images.allowed());
        // The master switch pins the policy off regardless of model.
        app.images_setting = false;
        app.apply_model_choice("p/seer".into());
        assert!(!app.images.allowed());
    }

    #[test]
    fn a_staged_path_sends_as_a_message_not_an_unknown_command() {
        // The dispatch rule: `/name` is a command only while the rest
        // contains no `/` — paths are messages, typos still teach.
        let mut staged = app();
        staged.input = "/tmp/hi-clipboard-1.png".into();
        staged.handle_key(key(KeyCode::Enter));
        assert_eq!(
            staged.messages[0],
            Message::User("/tmp/hi-clipboard-1.png".into())
        );
        // A typo'd command keeps its teaching error.
        let mut other = app();
        other.input = "/modle".into();
        other.handle_key(key(KeyCode::Enter));
        assert!(matches!(other.messages[0], Message::System(_)));
    }

    #[test]
    fn empty_clipboard_notes_and_text_paste_appends_normalized() {
        let mut app = app();
        let (tx, rx) = std::sync::mpsc::channel();
        app.begin_clipboard_request(rx);
        tx.send(crate::clipboard::ClipboardOutcome::Empty).unwrap();
        app.pump();
        assert_eq!(
            app.note.as_deref(),
            Some("clipboard holds no image or text")
        );

        app.note = None;
        app.input.clear();
        app.handle_paste("two\r\nlines\r");
        assert_eq!(app.input, "two\nlines\n");
        // Paste is insertion, not submission — even with newlines.
        assert!(app.messages.is_empty());
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
        // Five visible slots plus the scroll row only when the
        // registry overflows them.
        let total = crate::commands::COMMANDS.len();
        let expected = (total as u16).min(5) + u16::from(total > 5);
        assert_eq!(app.menu_height(), expected);
        // And the menu wraps: up from the first row lands on the last.
        assert_eq!(app.completion.as_ref().unwrap().selected, 0);
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.completion.as_ref().unwrap().selected, total - 1);
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.completion.as_ref().unwrap().selected, 0);
        app.completion = None;
        app.open_picker(); // empty catalog: "no models" row + hint row
        assert_eq!(app.menu_height(), 2);
    }

    #[test]
    fn a_filtered_menu_shrinks_with_its_matches() {
        let mut app = app();
        type_string(&mut app, "/m");
        let matches = crate::commands::matching("m");
        assert_eq!(app.menu_height(), matches.len() as u16);
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

    // ---- /resume: the in-session session picker ----

    /// A temp session root with one recorded session for the process
    /// cwd (the picker lists the *current* project only).
    fn picker_root(tag: &str, id: &str, name: Option<&str>, first_user: &str) -> PathBuf {
        use hi_core::session;
        let root = std::env::temp_dir().join(format!("hi-picker-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let cwd = std::env::current_dir().unwrap();
        let dir = root.join(session::project_key(&cwd));
        std::fs::create_dir_all(&dir).unwrap();
        let mut header = serde_json::json!({
            "type": "session",
            "version": 1,
            "id": id,
            "cwd": cwd.to_string_lossy(),
            "createdAt": 1,
            "provider": "mock",
            "model": "mock",
        });
        if let Some(name) = name {
            header["name"] = serde_json::json!(name);
        }
        let message = serde_json::json!({
            "type": "message",
            "message": { "user": first_user },
        });
        std::fs::write(
            dir.join(format!("0001_{id}.jsonl")),
            format!("{header}\n{message}\n"),
        )
        .unwrap();
        root
    }

    #[test]
    fn resume_opens_the_picker_and_esc_closes_it() {
        let mut app = app();
        app.session_root = Some(picker_root("open", "11111111-aaaa", Some("work"), "hello"));
        app.run_command("resume");
        let picker = app.session_picker.as_ref().expect("picker open");
        assert_eq!(picker.summaries.len(), 1);
        assert_eq!(picker.summaries[0].header.name.as_deref(), Some("work"));
        assert_eq!(picker.previews[0].as_deref(), Some("hello"));
        assert_eq!(app.session_picker_visible().len(), 1);

        app.handle_key(key(KeyCode::Esc));
        assert!(app.session_picker.is_none());
        // Closing changes nothing about the live session.
        assert!(app.session.is_none());
        assert!(app.messages.is_empty());
    }

    #[test]
    fn resume_filters_by_name_and_enter_switches_sessions() {
        let mut app = app();
        let root = picker_root("switch", "22222222-aaaa", Some("keeper"), "keep me");
        let cwd = std::env::current_dir().unwrap();
        // A second session the filter must hide.
        let project_dir = hi_core::session::project_key(&cwd);
        std::fs::write(
            root.join(project_dir).join("0002_33333333-bbbb.jsonl"),
            format!(
                "{}\n{}\n",
                serde_json::json!({
                    "type": "session", "version": 1, "id": "33333333-bbbb",
                    "cwd": cwd.to_string_lossy(), "createdAt": 2,
                    "provider": "mock", "model": "mock",
                }),
                serde_json::json!({ "type": "message", "message": { "user": "unrelated notes" } }),
            ),
        )
        .unwrap();

        app.session_root = Some(root);
        app.run_command("resume");
        app.handle_key(key(KeyCode::Char('k'))); // "keeper" only — the other session's id, name, and preview have no k
        assert_eq!(app.session_picker_visible().len(), 1);

        app.handle_key(key(KeyCode::Enter));
        assert!(app.session_picker.is_none());
        // Transcript replaced: the resumed-from note, then the file's history.
        assert_eq!(app.messages.len(), 2);
        assert!(
            matches!(&app.messages[0], Message::System(note) if note.contains("resumed 1 messages"))
        );
        assert_eq!(app.messages[1], Message::User("keep me".into()));
        // The window refolds so the event loop replays the new
        // history below the old conversation.
        assert_eq!(app.scroll.seen(), 0);
        // And the live append handle now points at the chosen file.
        let resumed = app.session.as_ref().expect("session handle").path();
        assert!(resumed.ends_with("0001_22222222-aaaa.jsonl"));
    }

    #[test]
    fn resume_is_refused_mid_turn() {
        let mut app = app();
        let (_tx, rx) = std::sync::mpsc::channel();
        app.turn = Some(Turn {
            text: String::new(),
            error: None,
            started_at: std::time::Instant::now(),
            rx,
        });
        app.run_command("resume");
        assert!(app.session_picker.is_none());
        assert!(
            matches!(app.messages.last(), Some(Message::System(note)) if note.contains("esc first"))
        );
    }

    fn alt(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::ALT)
    }

    #[test]
    fn arrows_move_the_cursor_and_clamp_at_the_edges() {
        let mut app = app();
        for c in "hi there".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(app.cursor, 8);
        app.handle_key(key(KeyCode::Left));
        assert_eq!(app.cursor, 7);
        app.handle_key(key(KeyCode::Right));
        assert_eq!(app.cursor, 8);
        // Past the end: clamped, not wrapped, not panicking.
        app.handle_key(key(KeyCode::Right));
        assert_eq!(app.cursor, 8);
        for _ in 0..20 {
            app.handle_key(key(KeyCode::Left));
        }
        assert_eq!(app.cursor, 0);
        app.handle_key(key(KeyCode::Left));
        assert_eq!(app.cursor, 0);
    }

    #[test]
    fn typing_and_backspace_work_at_the_cursor_not_the_tail() {
        let mut app = app();
        for c in "hello world".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        for _ in 0..6 {
            app.handle_key(key(KeyCode::Left));
        }
        assert_eq!(app.cursor, 5);
        // Typing lands mid-string.
        app.handle_key(key(KeyCode::Char('X')));
        assert_eq!(app.input, "helloX world");
        // Backspace removes before the cursor.
        app.handle_key(key(KeyCode::Backspace));
        assert_eq!(app.input, "hello world");
        assert_eq!(app.cursor, 5);
        // An old friend: backspace at the tail.
        app.handle_key(key(KeyCode::Right));
        app.handle_key(key(KeyCode::Right));
        app.handle_key(key(KeyCode::Backspace));
        assert_eq!(app.input, "hello orld"); // cursor 7, the 'w' goes
    }

    #[test]
    fn home_end_and_ctrl_a_e_jump_to_the_ends() {
        let mut app = app();
        for c in "hello".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Home));
        assert_eq!(app.cursor, 0);
        app.handle_key(key(KeyCode::End));
        assert_eq!(app.cursor, 5);
        app.handle_key(ctrl(KeyCode::Char('a')));
        assert_eq!(app.cursor, 0);
        app.handle_key(ctrl(KeyCode::Char('e')));
        assert_eq!(app.cursor, 5);
    }

    #[test]
    fn word_chords_jump_by_words() {
        let mut app = app();
        for c in "one two three".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(alt(KeyCode::Left));
        assert_eq!(app.cursor, 8); // "three" starts at 8
        app.handle_key(ctrl(KeyCode::Left));
        assert_eq!(app.cursor, 4);
        app.handle_key(alt(KeyCode::Right));
        assert_eq!(app.cursor, 7);
        app.handle_key(ctrl(KeyCode::Right));
        assert_eq!(app.cursor, 13);
    }

    #[test]
    fn cmd_left_right_jump_to_start_and_end() {
        let mut app = app();
        for c in "some words here".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        let cmd = KeyEvent::new(KeyCode::Left, KeyModifiers::SUPER);
        app.handle_key(cmd);
        assert_eq!(app.cursor, 0);
        app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::SUPER));
        assert_eq!(app.cursor, 15);
    }

    #[test]
    fn alt_b_and_alt_f_are_the_readline_word_motions() {
        let mut app = app();
        for c in "one two three".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        // What Ghostty's option+arrows decode to (ESC b / ESC f).
        app.handle_key(alt(KeyCode::Char('b')));
        assert_eq!(app.cursor, 8);
        app.handle_key(alt(KeyCode::Char('b')));
        assert_eq!(app.cursor, 4);
        app.handle_key(alt(KeyCode::Char('b')));
        assert_eq!(app.cursor, 0);
        app.handle_key(alt(KeyCode::Char('f')));
        assert_eq!(app.cursor, 3);
        app.handle_key(alt(KeyCode::Char('f')));
        assert_eq!(app.cursor, 7);
        app.handle_key(alt(KeyCode::Char('f')));
        assert_eq!(app.cursor, 13);
        // Unmodified b still types.
        app.handle_key(key(KeyCode::Char('b')));
        assert_eq!(app.input, "one two threeb");
        // alt+b lands at the start of the word the b is glued to —
        // "threeb" is one word.
        app.handle_key(alt(KeyCode::Char('b')));
        assert_eq!(app.cursor, 8);
        // From there alt+d kills the whole glued word.
        app.handle_key(alt(KeyCode::Char('d')));
        assert_eq!(app.input, "one two ");
    }

    #[test]
    fn kill_chords_delete_their_ranges() {
        let mut app = app();
        for c in "keep kill me".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(ctrl(KeyCode::Char('w'))); // word back: "me"
        assert_eq!(app.input, "keep kill ");
        // The cursor now parks after the space — Alt+Backspace skips
        // it and removes "kill ", composing exactly like a shell.
        app.handle_key(alt(KeyCode::Backspace));
        assert_eq!(app.input, "keep ");
        app.handle_key(ctrl(KeyCode::Char('u'))); // to start
        assert_eq!(app.input, "");
        for c in "keep kill me".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(ctrl(KeyCode::Char('a')));
        app.handle_key(ctrl(KeyCode::Char('f'))); // past 'k'
        app.handle_key(ctrl(KeyCode::Char('k'))); // to end
        assert_eq!(app.input, "k");
        // Forward delete: Ctrl+D at the cursor, Delete key the same.
        app.handle_key(ctrl(KeyCode::Char('e')));
        app.handle_key(ctrl(KeyCode::Char('d')));
        assert_eq!(app.input, "k");
        app.handle_key(ctrl(KeyCode::Char('a')));
        app.handle_key(key(KeyCode::Delete));
        assert_eq!(app.input, "");
    }

    #[test]
    fn tools_listing_names_badges_and_first_sentences() {
        let mut registry = ToolRegistry::new();
        registry
            .register(Arc::new(hi_core::tools::BashTool::new(".")))
            .unwrap();
        registry
            .register(Arc::new(hi_core::tools::ReadTool::new(".")))
            .unwrap();

        let listing = App::tools_listing(&registry);
        let lines: Vec<&str> = listing.split('\n').collect();
        assert_eq!(lines[0], "registered tools (2)");
        // Sorted by name — the registry's order, not insertion order.
        assert!(lines[1].starts_with("  bash (destructive) — Execute a bash command"));
        assert!(lines[2].starts_with("  read (read-only) — Read the contents of a text file"));
        // The rest of the description paragraph stays out.
        assert!(!listing.contains("line-numbered"));
    }

    #[test]
    fn empty_registry_lists_nothing() {
        let registry = ToolRegistry::new();
        assert_eq!(App::tools_listing(&registry), "no tools registered");
    }

    #[test]
    fn tools_command_pushes_a_local_notice() {
        let mut app = app();
        app.run_command("tools");
        let last = app.messages.last().unwrap();
        match last {
            Message::System(text) => assert_eq!(text, "no tools registered"),
            other => panic!("expected a System notice, got {other:?}"),
        }
    }

    #[test]
    fn alt_d_deletes_the_word_ahead() {
        let mut app = app();
        for c in "go far now".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(ctrl(KeyCode::Char('a')));
        app.handle_key(alt(KeyCode::Char('d')));
        assert_eq!(app.input, " far now"); // "go" goes, the space stays
    }

    #[test]
    fn history_walks_newest_first_and_restores_the_draft() {
        let mut app = app();
        for msg in ["first message", "second message"] {
            for c in msg.chars() {
                app.handle_key(key(KeyCode::Char(c)));
            }
            app.handle_key(key(KeyCode::Enter));
            app.pump();
        }
        assert_eq!(app.history, ["first message", "second message"]);
        // Fresh draft parked on the first Up.
        for c in "draft text".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.input, "second message");
        assert_eq!(app.cursor, 0);
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.input, "first message");
        app.handle_key(key(KeyCode::Up)); // oldest — clamps
        assert_eq!(app.input, "first message");
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.input, "second message");
        app.handle_key(key(KeyCode::Down)); // past newest — draft returns
        assert_eq!(app.input, "draft text");
        assert_eq!(app.cursor, 10);
        assert_eq!(app.history_pos, None);
    }

    #[test]
    fn a_cancelled_message_can_be_recalled_edited_and_resent() {
        let mut app = app();
        for c in "deploy to prod".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Enter));
        app.pump();
        // Esc cancels the turn; the sent text stays in history.
        app.handle_key(key(KeyCode::Esc));
        assert!(app.turn.is_none());
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.input, "deploy to prod");
        // Cursor parked at the start: the first edits fix the beginning.
        app.handle_key(key(KeyCode::Char('X')));
        assert_eq!(app.input, "Xdeploy to prod");
        app.handle_key(key(KeyCode::Enter));
        app.pump();
        assert!(matches!(app.messages.last(), Some(Message::User(m)) if m == "Xdeploy to prod"));
        // History now holds both, in order.
        assert_eq!(app.history, ["deploy to prod", "Xdeploy to prod"]);
    }

    #[test]
    fn history_records_commands_and_queue_lines_but_not_empties() {
        let mut app = app();
        app.handle_key(key(KeyCode::Enter)); // empty — nothing
        assert!(app.history.is_empty());
        for c in "/theme".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.history, ["/theme"]);
        // A mid-turn submission queues — and lands in history too.
        for c in "hello".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Enter));
        app.pump();
        for c in "also this".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Enter));
        app.pump();
        assert_eq!(app.history, ["/theme", "hello", "also this"]);
    }

    #[test]
    fn paste_inserts_at_the_cursor_with_pads_on_both_sides() {
        let mut app = app();
        for c in "seepaste now".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        for _ in 0..9 {
            app.handle_key(key(KeyCode::Left));
        }
        assert_eq!(app.cursor, 3);
        app.handle_paste("/tmp/x.png");
        assert_eq!(app.input, "see /tmp/x.png paste now");
        assert_eq!(app.cursor, 15);
    }
}
