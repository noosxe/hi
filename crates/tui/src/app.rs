use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::Instant;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use hi_core::{
    Event, Message, ModelsConfig, OpenAiCompatProvider, Provider, Session, Settings, ThinkingLevel,
    ToolRegistry, spawn_agent_turn,
};

use crate::catalog::Catalog;
/// All TUI state lives here.
///
/// The contract that keeps a TUI codebase sane as it grows:
/// event handling *mutates* this struct (`handle_key`, `pump`),
/// rendering only *reads* it. No widget, no terminal handle, no
/// drawing inside here.
use crate::editor;
use crate::status::{Status, location_label, usage_line};
use hi_core::event::Usage;

/// Prompt-history ceiling (AD-0014).
const HISTORY_CAP: usize = 100;

/// Undo ceiling (#65): snapshot-per-unit of a few-KB composer is a
/// few hundred KB worst case — bounded, like the history ring.
const UNDO_CAP: usize = 100;
/// Kills the ring remembers (#64) — emacs keeps 60; sixteen is
/// plenty for moving paragraphs around a prompt.
const KILL_CAP: usize = 16;

/// One undo unit: the composer state before the edit, plus the
/// post-cursor a continuing plain insert must land on to merge into
/// this unit — `usize::MAX` means the unit is closed.
struct UndoStep {
    input: String,
    cursor: usize,
    run_end: usize,
}

/// Derive the ring from a transcript's user prompts (#87) — the same
/// view whether the transcript came from `SessionInit` or a session
/// switch. Oldest-first, whitespace-only prompts invisible (the live
/// ring never took them), and the ceiling holds on seed: the disk may
/// hold more than the ring ever shows. Commands never replayed — they
/// ride the live ring only, never the transcript.
fn seed_history(messages: &[Message]) -> Vec<String> {
    let mut ring: Vec<String> = messages
        .iter()
        .filter_map(|message| match message {
            Message::User(text) if !text.trim().is_empty() => Some(text.clone()),
            _ => None,
        })
        .collect();
    if ring.len() > HISTORY_CAP {
        ring.drain(..ring.len() - HISTORY_CAP);
    }
    ring
}

/// The auto-trigger budget (AD-0022): how many turns a session may
/// spend on its own before it stops and says so. A safety bound, not
/// a preference — like the task cap in AD-0019, it is constant so the
/// failure mode is bounded by construction.
const AUTO_TRIGGER_BUDGET: u32 = 10;

/// The standing instruction the auto-trigger speaks with. Marked so
/// the session file is honest that the human did not type it, and so
/// the model knows the turn is machine-initiated.
const AUTO_TRIGGER_MANDATE: &str =
    "(auto-trigger) a background task finished — act on the notes above as warranted";

/// Pushed once, with the budget's last spend, so a disabled-by-
/// exhaustion trigger is a visible fact rather than a silent one.
const AUTO_TRIGGER_EXHAUSTED: &str =
    "auto-trigger budget spent — further completions ride your next turn";

/// The paste chord: Ctrl+V always; Cmd+V too where the kitty
/// keyboard protocol reports it (`KeyModifiers::SUPER` — the terminal
/// only forwards the chord in that mode, so this can never collide
/// with a terminal that still owns Cmd+V for its own text paste).
/// Compact wall-time phrasing for task notes: `54s`, `3m12s`.
fn format_task_wall(wall: std::time::Duration) -> String {
    let secs = wall.as_secs();
    if secs >= 60 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

/// The newline chords: Shift+Enter (the kitty encoding — crossterm
/// hands us Enter+SHIFT), a literal LF (a terminal binding
/// shift+enter to `text:\n`), and Ctrl+J (legacy, works everywhere).
fn is_newline_key(key: &KeyEvent) -> bool {
    (key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::SHIFT))
        || key.code == KeyCode::Char('\n')
        || (key.code == KeyCode::Char('j') && key.modifiers.contains(KeyModifiers::CONTROL))
}

fn paste_key(key: &KeyEvent) -> bool {
    key.code == KeyCode::Char('v')
        && (key.modifiers.contains(KeyModifiers::CONTROL)
            || key.modifiers.contains(KeyModifiers::SUPER))
}

/// Nothing in this suite may touch the developer's (or CI runner's)
/// real home: config (the persisting tests read-modify-write the real
/// `settings.json`, which raced into empty-file parse errors under
/// `cargo test`'s parallel processes) and data (`Session::start` with
/// no explicit root writes real session files). Armed once per
/// process, before the first `App` exists — every `config_dir()` and
/// `sessions_dir()` lookup afterwards lands in the scratch tree.
#[cfg(test)]
fn hermetic_config() {
    static ARMED: std::sync::Once = std::sync::Once::new();
    ARMED.call_once(|| {
        let config =
            std::env::temp_dir().join(format!("hi-tui-test-config-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&config);
        std::fs::create_dir_all(config.join("hi")).unwrap();
        let data = std::env::temp_dir().join(format!("hi-tui-test-data-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        std::fs::create_dir_all(data.join("hi")).unwrap();
        // Safety: armed once, serialized by `Once`, before any test
        // reads the environment through `config_dir`/`sessions_dir`.
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &config) };
        unsafe { std::env::set_var("XDG_DATA_HOME", &data) };
    });
}

#[cfg(test)]
pub(crate) fn test_app() -> App {
    app_with(crate::SessionInit {
        system_prompt: None,
        thinking: None,
        tools: Arc::new(ToolRegistry::new()),
        messages: Vec::new(),
        session: None,
        usage: Usage::default(),
        usage_last_prompt: 0,
        images: hi_core::ImageGate::new(false),
        images_setting: true,
        tasks: None,
        task_turns: false,
        max_tool_rounds: hi_core::DEFAULT_MAX_TOOL_ROUNDS,
        subagent: hi_core::subagent::SubagentRunner::new(
            Default::default(),
            std::env::temp_dir(),
            false,
            None,
        ),
    })
}

#[cfg(test)]
fn app_with(init: crate::SessionInit) -> App {
    hermetic_config();
    App::new(
        Arc::new(hi_core::MockProvider::default()),
        Status {
            provider: "mock".into(),
            model: "mock".into(),
            tool_count: 0,
        },
        Catalog::default(),
        ModelsConfig::default(),
        init,
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
    /// The composer's width from the last frame — the visual-row math
    /// behind Up/Down in a multi-line input needs it (AD-0018). A
    /// sane default until the first frame lands.
    pub width: u16,
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
    /// Undo/redo for the composer (#65): snapshot-per-unit states
    /// before each edit, capped at [`UNDO_CAP`]. Redo holds the states
    /// undo popped, and clears on any fresh edit. Only composer
    /// mutations land here — history browse and queue handback are
    /// navigation, not edits.
    undo: Vec<UndoStep>,
    redo: Vec<(String, usize)>,
    /// The kill ring (#64): newest last, capped at [`KILL_CAP`].
    /// Kills feed it, Ctrl+Y yanks the head, Alt+Y cycles.
    kill_ring: Vec<String>,
    /// The range the newest kill removed — a further kill joins that
    /// entry only while it abuts (the insert-run rule, again).
    last_kill: Option<(usize, usize)>,
    /// Where the in-flight yank sits and which ring entry it holds:
    /// `(start, end, index from the head)`. Alt+Y swaps it; any other
    /// key drops it.
    yank_site: Option<(usize, usize, usize)>,
    /// Armed jump-to-char (#67): `Ctrl+]` sets it, the next
    /// printable key is the target, anything else lets it lapse.
    jump_pending: Option<editor::Jump>,
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
    /// Session-cumulative token economics (AD-0026), merged from
    /// every `Event::Usage`. `usage_last_prompt` is the latest round's
    /// prompt size — the context fact the share needs.
    pub(crate) usage: Usage,
    pub(crate) usage_last_prompt: u64,
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
    /// The open `/settings` board (AD-0023), if any. `Some` means it
    /// owns the keyboard.
    pub settings_board: Option<SettingsBoard>,
    /// The open `/thinking` ladder picker, if any — same menu-slot
    /// contract as the settings board.
    pub thinking_picker: Option<ThinkingPicker>,
    /// The open `/context` report, if any — same menu slot, read-only.
    pub context_panel: Option<ContextPanel>,
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
    /// The background-task manager shared with the registry (#5). The
    /// app drains completions per frame; `None` when no task tools run.
    pub tasks: Option<hi_core::tasks::TaskManager>,
    /// The nested-agent runner (#4); restamped on provider rebuilds.
    pub subagent: hi_core::subagent::SubagentRunner,
    /// The open `/tasks` pane (AD-0021) — list or one task's output.
    /// `Some` = owns the keyboard, like the pickers.
    pub tasks_view: Option<TasksView>,
    /// The main-loop tool-round cap, resolved at startup
    /// (`settings.maxToolRounds` override or the built-in default).
    pub max_tool_rounds: usize,
    /// The `settings.taskTurns` opt-in (#72, AD-0022): a finished
    /// task may start a turn. Resolved once at startup; the app owns
    /// the trigger itself.
    pub task_turns: bool,
    /// Auto-trigger budget (AD-0022): how many turn-spends this
    /// session may still make on its own. Bounded, never refilled —
    /// completion → turn → spawn → completion chains must end.
    pub auto_trigger_budget: u32,
    /// A completion folded while a turn was in flight: the fact is
    /// armed. If the human's queued message fires first it consumes
    /// the arm; otherwise the mandate turn fires (budget permitting).
    auto_armed: bool,
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
    /// While `Some`, the picker is renaming the highlighted session:
    /// the buffer rides the prompt line and owns the keyboard.
    pub renaming: Option<String>,
}

/// Braille frames for the activity spinner, cycled once per frame.
const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// One assistant reply being streamed by a provider thread.
pub struct Turn {
    /// The reply accumulated so far as blocks (AD-0048) — prose and
    /// reasoning in stream order; rendered live, finalized into
    /// `messages` when the turn ends.
    pub blocks: Vec<hi_core::message::AssistantBlock>,
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

#[cfg(test)]
impl Turn {
    /// A turn mid-stream with fixed text — lets render tests place
    /// the live region deterministically instead of racing the mock's
    /// chunk timing.
    pub(crate) fn with_text(text: &str) -> Turn {
        Turn {
            blocks: vec![hi_core::message::AssistantBlock::Text(text.to_string())],
            error: None,
            started_at: Instant::now(),
            rx: std::sync::mpsc::channel().1,
        }
    }
}

/// The open `/model` chooser: flat `provider/model` items plus the
/// highlighted row.
pub struct Picker {
    pub items: Vec<String>,
    pub selected: usize,
}

/// One togglable session policy (#79, AD-0023). A row exists on the
/// board only when the running app can flip it safely mid-session;
/// startup-stamped settings stay off the board entirely.
#[derive(Clone, Copy, PartialEq)]
pub enum SettingKind {
    /// The #72 orchestrator opt-in: completions may start a turn.
    TaskTurns,
    /// The image-upload master switch (AD-0012 re-resolves the gate).
    Images,
}

/// The board's rows, in display order. Curated by hand — this table
/// is the whole contract between "a setting exists" and "the
/// `/settings` board offers it".
pub(crate) const SETTINGS_TOGGLES: &[(&str, SettingKind)] = &[
    ("task turns", SettingKind::TaskTurns),
    ("images", SettingKind::Images),
];

/// The open `/settings` board (AD-0023): which row is highlighted.
/// Enter flips and keeps the board open — a board, not a chooser;
/// Esc closes; Ctrl-S persists the highlighted row.
pub struct SettingsBoard {
    pub selected: usize,
}

/// The `/thinking` picker (AD-0044 addendum): the ladder as a modal,
/// rows computed fresh each frame from model + metadata — Enter
/// switches (and persists), Esc closes.
pub struct ThinkingPicker {
    pub selected: usize,
}

/// The `/context` report (AD-0047): the wire request bucketed by
/// class as a read-only modal — rows computed fresh each frame, any
/// key closes (nothing to select).
pub struct ContextPanel;

/// The `/tasks` pane's view (AD-0021): the list, or one task's
/// output watched live. The pane lives in the menu slot under the
/// input; draw reads the manager fresh every frame — the frame clock
/// is the poll, no cache, no thread.
pub enum TasksView {
    /// Every task, oldest first; `selected` is an index into
    /// `TaskManager::list()` re-read per frame.
    List { selected: usize },
    /// One task's output tail, newest rows at the bottom.
    Output { id: u64 },
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
            usage,
            usage_last_prompt,
            images,
            images_setting,
            tasks,
            task_turns,
            subagent,
            max_tool_rounds,
        } = init;
        // The ring replays from the transcript it rode in on (#87) —
        // a resume or fork hands its prompts back to ↑.
        let history = seed_history(&messages);
        Self {
            max_tool_rounds,
            quit: false,
            input: String::new(),
            cursor: 0,
            width: 80,
            history,
            history_pos: None,
            history_draft: None,
            undo: Vec::new(),
            redo: Vec::new(),
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
            settings_board: None,
            thinking_picker: None,
            context_panel: None,
            completion: None,
            tick: 0,
            queued: None,
            turn: None,
            kill_ring: Vec::new(),
            last_kill: None,
            yank_site: None,
            jump_pending: None,
            session_picker: None,
            clipboard_rx: None,
            note: None,
            images,
            images_setting,
            tasks,
            tasks_view: None,
            task_turns,
            subagent,
            auto_trigger_budget: AUTO_TRIGGER_BUDGET,
            auto_armed: false,
            usage,
            usage_last_prompt,
            scroll: crate::scroll::Scroll::new(),
            session_root: None,
        }
    }

    /// React to one key press by mutating state.
    pub fn handle_key(&mut self, key: KeyEvent) {
        // The composer note is transient by definition: any keypress
        // supersedes it.
        self.note = None;
        // So is an in-flight yank (#64): Alt+Y only cycles directly
        // after Ctrl+Y or another Alt+Y. The yank arms set it fresh —
        // it's captured first because the Alt+Y arm is the one key
        // that still needs it.
        let yank = self.yank_site.take();
        // A pending jump-to-char (#67) eats the next key: a
        // printable character is the target — the cursor moves, or
        // nothing at none. Enter cancels without submitting (a
        // stray chord pair must not send a half-written draft), the
        // jump chord itself cancels (pi's rule), and everything
        // else falls through with the pending mode consumed.
        if let Some(dir) = self.jump_pending.take() {
            match key.code {
                KeyCode::Char(c)
                    if !key.modifiers.intersects(
                        KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                    ) =>
                {
                    let landed = match dir {
                        editor::Jump::Forward => editor::jump_forward(&self.input, self.cursor, c),
                        editor::Jump::Backward => {
                            editor::jump_backward(&self.input, self.cursor, c)
                        }
                    };
                    if let Some(pos) = landed {
                        self.cursor = pos;
                    }
                    return;
                }
                KeyCode::Enter => return,
                KeyCode::Char(']') if key.modifiers.contains(KeyModifiers::CONTROL) => return,
                _ => {}
            }
        }
        // Ctrl-C is the line-killer, not the app-killer (#91): a
        // non-empty draft dies first (cursor home), an empty composer
        // is the user asking to leave. Menus own no text, so from a
        // picker or pane this still quits.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            if !self.input.is_empty() {
                // The line-kill is a composer mutation like any other:
                // one Ctrl-Z and the draft is back (#65).
                self.apply_edit(|input, _| {
                    input.clear();
                    0
                });
            } else {
                self.quit = true;
            }
            return;
        }

        // When the picker is open, it owns the keyboard.
        if self.picker.is_some() {
            self.handle_picker_key(key);
            return;
        }
        // The `/settings` board owns the keyboard like every menu
        // (AD-0023).
        if self.settings_board.is_some() {
            self.handle_settings_key(key);
            return;
        }
        // The `/thinking` ladder picker owns the keyboard like every
        // menu (AD-0044 addendum).
        if self.thinking_picker.is_some() {
            self.handle_thinking_picker_key(key);
            return;
        }
        // The `/context` report is read-only: any key dismisses it.
        if self.context_panel.is_some() {
            self.context_panel = None;
            return;
        }
        // The session picker likewise: it takes over everything above
        // the status row while open.
        if self.session_picker.is_some() {
            self.handle_session_picker_key(key);
            return;
        }
        // The `/tasks` pane owns the keyboard like every menu: Esc
        // backs out (output → list → closed), arrows walk, Enter
        // opens. Everything else is swallowed while it is open.
        if self.tasks_view.is_some() {
            self.handle_tasks_key(key);
            return;
        }
        // While the `/` completion menu is open it consumes the
        // navigation keys; everything else falls through to editing.
        // The newline chords outrank it (pi parity): a newline is an
        // edit, not a menu pick — the menu narrows on the text, and
        // an unmodified Enter still selects (AD-0018).
        if self.completion.is_some() && is_newline_key(&key) {
            self.insert_newline();
            return;
        }
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
                // Ctrl+A/E are line-scoped now: start/end of the
                // logical line the caret is on — identical to the
                // whole input while it is single-line (AD-0018).
                'a' => self.cursor = editor::line_start(&self.input, self.cursor),
                'e' => self.cursor = editor::line_end(&self.input, self.cursor),
                // Ctrl+J inserts a newline — the chord that works in
                // every terminal, no kitty protocol required (pi's
                // second binding; AD-0018).
                'j' => self.insert_newline(),
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
                        self.kill_range(self.cursor..next);
                    }
                }
                // Line-scoped with the A/E family (AD-0018).
                'k' => {
                    let end = editor::line_end(&self.input, self.cursor);
                    self.kill_range(self.cursor..end);
                }
                'u' => {
                    let start = editor::line_start(&self.input, self.cursor);
                    self.kill_range(start..self.cursor);
                }
                'w' => {
                    let range = editor::word_delete_range_back(&self.input, self.cursor);
                    self.kill_range(range);
                }
                // Jump-to-char, step one (#67): arm the pending
                // jump. Ctrl+Alt+] arms the backward one — pi's
                // chords. Legacy terminals send Ctrl+] as the ESC
                // byte, so this lives under the kitty protocol.
                ']' => {
                    self.jump_pending = Some(if key.modifiers.contains(KeyModifiers::ALT) {
                        editor::Jump::Backward
                    } else {
                        editor::Jump::Forward
                    });
                }
                // Ctrl+Y yanks the newest kill back (#64).
                'y' => self.yank(),
                // Ctrl+Z reverts the newest composer mutation (#65);
                // the run of plain inserts counts as one.
                'z' => self.undo_edit(),
                // Unbound control chords keep their old behavior: the
                // character lands in the input (ctrl+c quits and
                // ctrl+v/cmd+v paste, both handled above).
                _ => {
                    self.apply_insert(c);
                    self.sync_completion();
                }
            },
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::ALT) => {
                let range = editor::word_delete_range_fwd(&self.input, self.cursor);
                self.kill_range(range);
                self.sync_completion();
            }
            // Alt+Y cycles the in-flight yank through older kills (#64).
            KeyCode::Char('y') if key.modifiers.contains(KeyModifiers::ALT) => {
                self.yank_pop(yank);
            }
            // Alt+Z reapplies what Ctrl+Z reverted (#65) — emacs-style
            // redo, since shift+ctrl+z is indistinguishable on legacy
            // terminals.
            KeyCode::Char('z') if key.modifiers.contains(KeyModifiers::ALT) => {
                self.redo_edit();
            }
            // Newline chords (AD-0018): Shift+Enter rides the kitty
            // encoding (`CSI 13;2u` — crossterm hands us Enter+SHIFT,
            // the encoding is the capability, AD-0013's philosophy);
            // a literal LF arrives where a terminal binds shift+enter
            // to `text:\n` or a legacy raw-mode Ctrl+J slips through.
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.insert_newline();
            }
            KeyCode::Char('\n') => {
                self.insert_newline();
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
            // Home/End follow the A/E family: line-scoped (AD-0018).
            KeyCode::Home => self.cursor = editor::line_start(&self.input, self.cursor),
            KeyCode::End => self.cursor = editor::line_end(&self.input, self.cursor),
            // Prompt history: up into the past (draft parked on
            // entry), down toward it — past the newest, the draft
            // returns (AD-0014). In a multi-line input the arrows
            // first walk the visual rows, pi-style (AD-0018).
            KeyCode::Up => self.up_or_history(),
            KeyCode::Down => self.down_or_history(),
            KeyCode::Backspace => {
                if key.modifiers.contains(KeyModifiers::ALT) {
                    // The same unit the move lands on, so
                    // Alt+Backspace composes with Alt+Left.
                    let range = editor::word_delete_range_back(&self.input, self.cursor);
                    self.kill_range(range);
                } else {
                    self.apply_edit(|input, cursor| {
                        editor::prev_boundary(input, cursor)
                            .map_or(cursor, |prev| editor::delete_range(input, prev..cursor))
                    });
                }
                self.sync_completion();
            }
            KeyCode::Delete => {
                if key.modifiers.contains(KeyModifiers::ALT) {
                    let range = editor::word_delete_range_fwd(&self.input, self.cursor);
                    self.kill_range(range);
                } else {
                    self.apply_edit(|input, cursor| {
                        editor::next_boundary(input, cursor)
                            .map_or(cursor, |next| editor::delete_range(input, cursor..next))
                    });
                }
                self.sync_completion();
            }
            KeyCode::Char(c) => {
                self.apply_insert(c);
                self.sync_completion();
            }
            KeyCode::Enter => {
                // The escape hatch for terminals with neither newline
                // chord (pi parity): a backslash right before Enter
                // yields the newline instead of the submission.
                if self.cursor > 0 && self.input.as_bytes()[self.cursor - 1] == b'\\' {
                    self.cursor =
                        editor::delete_range(&mut self.input, self.cursor - 1..self.cursor);
                    self.insert_newline();
                    return;
                }
                // Enter always submits — a multi-line draft goes out
                // whole (AD-0018). Move the input out, leaving an
                // empty String behind — no clone needed since we own
                // the value anyway.
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
                    // completion speaks for the user. The name is the
                    // first word — everything after it rides to the
                    // handler ("/tasks 3").
                    let (name_part, args) =
                        command_text.split_once(' ').unwrap_or((command_text, ""));
                    let highlighted = self
                        .completion
                        .as_ref()
                        .and_then(|menu| menu.items.get(menu.selected))
                        .map(|(name, _)| *name);
                    let name = crate::commands::find_exact(name_part)
                        .map(|command| command.name)
                        .or(highlighted);
                    self.completion = None;
                    match name {
                        Some(name) => self.run_command(name, args.trim()),
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
                // Persist NOW: the frame pump's `added_from` marker is
                // captured after this push, so the user line would
                // otherwise never ride a later append (the recording
                // gap the lazy-session e2e exposed — headless recorded
                // it, the TUI silently did not).
                self.persist_last();
                self.start_turn();
            }
            KeyCode::Esc => {
                // Cancel the active turn: `take` drops the Turn, which
                // drops the event Receiver, so the provider thread's
                // next send fails and it returns — the same
                // drop-is-cancellation mechanism as quitting.
                // pi's behavior: partial output stays in the transcript.
                if let Some(turn) = self.turn.take()
                    && !turn.blocks.is_empty()
                {
                    self.messages.push(Message::Assistant(turn.blocks));
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
            self.max_tool_rounds,
        );
        self.turn = Some(Turn {
            blocks: Vec::new(),
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

    /// `/settings`: open the policy board (AD-0023). Takes no
    /// arguments — free-form editing is out of scope; the board offers
    /// exactly what the running session can flip.
    fn open_settings_board(&mut self) {
        self.settings_board = Some(SettingsBoard { selected: 0 });
    }

    /// The board's rows as rendered: `label: on|off`. Draw reads this
    /// live every frame, so a flip is visible without reopening.
    pub(crate) fn settings_rows(&self) -> Vec<String> {
        SETTINGS_TOGGLES
            .iter()
            .map(|(label, kind)| {
                let on = match kind {
                    SettingKind::TaskTurns => self.task_turns,
                    SettingKind::Images => self.images_setting,
                };
                format!("{label}: {}", if on { "on" } else { "off" })
            })
            .collect()
    }

    /// Keyboard owner while the board is open. The `/model` grammar
    /// with one divergence: Enter keeps the board open, so a flip is
    /// visible and more rows can follow (AD-0023).
    fn handle_settings_key(&mut self, key: KeyEvent) {
        let Some(board) = self.settings_board.as_mut() else {
            return;
        };
        let count = SETTINGS_TOGGLES.len();
        match key.code {
            KeyCode::Esc => self.settings_board = None,
            KeyCode::Up => {
                board.selected = if board.selected == 0 {
                    count.saturating_sub(1)
                } else {
                    board.selected - 1
                };
            }
            KeyCode::Down => {
                board.selected = (board.selected + 1) % count.max(1);
            }
            KeyCode::Enter => {
                let kind = SETTINGS_TOGGLES[board.selected].1;
                self.toggle_setting(kind);
            }
            // pi's contract, `/model`'s: Ctrl-S persists the choice;
            // Enter only changes this session.
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let (label, kind) = SETTINGS_TOGGLES[board.selected];
                let state = match kind {
                    SettingKind::TaskTurns => self.task_turns,
                    SettingKind::Images => self.images_setting,
                };
                let result = match kind {
                    SettingKind::TaskTurns => Settings::persist_task_turns(state),
                    SettingKind::Images => Settings::persist_images(state),
                };
                match result {
                    Ok(()) => self
                        .messages
                        .push(Message::System(format!("saved: {label} = {}", state as u8))),
                    Err(err) => self.messages.push(Message::Error(err)),
                }
            }
            _ => {}
        }
    }

    /// Flip one session policy. `task turns` is live on the next
    /// pump; `images` re-resolves the gate through the same one
    /// construction path as a model switch (AD-0012) so the flip
    /// applies now, not on the next switch. Mock providers have no
    /// config entry and are left alone — honest no-op.
    fn toggle_setting(&mut self, kind: SettingKind) {
        match kind {
            SettingKind::TaskTurns => {
                self.task_turns = !self.task_turns;
                let state = if self.task_turns { "on" } else { "off" };
                self.messages.push(Message::System(format!(
                    "task turns {state} for this session — ctrl-s saves it"
                )));
            }
            SettingKind::Images => {
                self.images_setting = !self.images_setting;
                let provider = self.status.provider.clone();
                let model = self.status.model.clone();
                let cache_key = self.session.as_ref().map(|s| s.id().to_string());
                self.rebuild_provider(&provider, &model, cache_key);
                let state = if self.images_setting { "on" } else { "off" };
                self.messages.push(Message::System(format!(
                    "images {state} for this session — ctrl-s saves it"
                )));
            }
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
        // The subagent runner follows the switch: inherited runs use
        // whatever identity is current, never a stale one.
        self.subagent.stamp(provider.to_string(), model.to_string());
        // The transcript lineage follows the live session (#116).
        self.subagent
            .stamp_session(self.session.as_ref().map(|s| s.id().to_string()));
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
        // AD-0046: the live header follows the switch, so the next
        // resume restores the model the session ended on. Same
        // best-effort surface as the thinking restamp.
        if let Some(session) = self.session.as_mut()
            && let Err(err) = session.set_model(provider, model)
        {
            self.messages.push(Message::Error(err));
        }
    }

    /// Height of the open menu, borders included: pickers are 4-5
    /// rows, completion menus 3-4; 0 when no menu is open. The menu
    /// takes its rows from the streaming area (elastic 8 → 2), so it
    /// lands directly above the input.
    /// The status board (AD-0025, amended; usage AD-0026): rows
    /// bottom-up, two permanent residents. Row 1 — the board's top —
    /// holds the session's usage line in the top-left corner and the
    /// model (+thinking) on the right, the auto-trigger arm appended
    /// after it (transient noise rides right). Row 0 is location on
    /// the left and the background tasks' counts on the right — under
    /// the model they run against. #4/#6 register here later; three
    /// rows is the ceiling anyone plans to spend.
    pub fn status_rows(&self) -> Vec<(String, String)> {
        // AD-0044 ruling 3: the level is always visible — `auto` when
        // unset, meaning the provider decides (no kwargs are sent).
        let thinking_label = self
            .thinking
            .as_ref()
            .map(|level| level.name())
            .unwrap_or("auto");
        let mut context_right = format!("{} · thinking:{}", self.status.label(), thinking_label);
        if self.auto_armed {
            context_right.push_str(" · auto: armed");
        }
        let mut task_counts = String::new();
        if let Some(tasks) = self.tasks.as_ref() {
            let total = tasks.list().len();
            if total > 0 {
                let running = tasks.running_count();
                let done = total - running;
                let parts: Vec<String> = [
                    (running > 0, format!("{running} running")),
                    (done > 0, format!("{done} done")),
                ]
                .into_iter()
                .filter_map(|(keep, part)| keep.then_some(part))
                .collect();
                task_counts = format!("tasks: {}", parts.join(" · "));
            }
        }
        let bill = usage_line(
            &self.usage,
            self.usage_last_prompt,
            self.context_window(),
            self.pricing(),
        );
        vec![(self.location.clone(), task_counts), (bill, context_right)]
    }

    /// The `/context` panel's rows (AD-0047): the wire request
    /// bucketed by class, anchored to the provider's last bill when
    /// one exists, plus the free remainder of a known window.
    pub(crate) fn context_panel_rows(&self) -> Vec<String> {
        let report = hi_core::context_report::context_report(
            hi_core::context_report::ContextInput {
                system_prompt: self.system_prompt.as_deref(),
                tools: &self.tools,
                history: &self.messages,
            },
            (self.usage_last_prompt > 0).then_some(self.usage_last_prompt),
        );
        let anchored = report.anchored();
        let total: u64 = anchored.iter().map(|(_, t)| t).sum();
        let provenance = if report.prompt_tokens.is_some() {
            "anchored to the provider's bill"
        } else {
            "estimated (bytes over four)"
        };
        let mut rows = vec![format!(
            "context \u{2014} {total} tokens \u{2014} {provenance}"
        )];
        for (class, tokens) in &anchored {
            let pct = (tokens * 100).checked_div(total).unwrap_or(0);
            let bar = "\u{2593}".repeat(((pct / 4) as usize).clamp(1, 25));
            rows.push(format!(
                "  {:<13} {:>7}  {:>3}%  {bar}",
                class.label(),
                tokens,
                pct
            ));
        }
        if let Some(window) = self.context_window()
            && window > total
        {
            let free = window - total;
            let pct = free * 100 / window;
            rows.push(format!(
                "  {:<13} {:>7}  {:>3}%  free before the window's edge",
                "free", free, pct
            ));
        }
        rows
    }

    /// The context window for the live model — the denominator of the
    /// usage line's context share. Tier chain (AD-0044): the config
    /// entry's `contextWindow` wins; then the shipped catalog's vendor
    /// metadata; then no share. Guessed means vendor-published, never
    /// invented (AD-0026 revised).
    fn context_window(&self) -> Option<u64> {
        if let Some(entry) = self.config.providers.get(&self.status.provider)
            && let Some(model) = entry.models.iter().find(|m| m.id == self.status.model)
            && let Some(window) = model.context_window
        {
            return Some(u64::from(window));
        }
        hi_core::catalog::lookup(&self.status.model).map(|info| info.context_window)?
    }

    /// Vendor pricing for the live model, from the shipped catalog —
    /// the bill's `~$` estimate. Config cannot set rates (pi-parity:
    /// models.json carries no pricing), so no tier above this.
    fn pricing(&self) -> Option<&'static hi_core::catalog::ModelInfo> {
        hi_core::catalog::lookup(&self.status.model)
    }

    /// The board's row count — the height `layout` and the flush pass
    /// both measure with, so they can never disagree (AD-0025).
    pub fn status_height(&self) -> u16 {
        self.status_rows().len() as u16
    }

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
        } else if self.thinking_picker.is_some() {
            // Same SelectList geometry as the model picker, plus any
            // muted vendor-note rows.
            let notes = self.thinking_menu().1.len() as u16;
            5 + 1 + 1 + notes
        } else if self.context_panel.is_some() {
            // The report shows every row — it is read-only, no
            // SelectList window — plus the key-hint footer.
            self.context_panel_rows().len() as u16 + 1
        } else if self.settings_board.is_some() {
            // The board is small and fixed: every row visible, plus
            // the key-hint footer.
            let count = SETTINGS_TOGGLES.len() as u16;
            count.min(5) + u16::from(count > 5) + 1
        } else if let Some(completion) = &self.completion {
            // SelectList geometry: five visible rows, plus the
            // `(n/m)` scroll row only when the matches overflow them.
            let visible = (completion.items.len() as u16).min(5);
            visible + u16::from(completion.items.len() > 5)
        } else if let Some(view) = &self.tasks_view {
            match view {
                TasksView::List { .. } => {
                    // The picker's geometry plus its key-hint footer.
                    let count = self
                        .tasks
                        .as_ref()
                        .map(|manager| manager.list().len())
                        .unwrap_or(0);
                    if count == 0 {
                        2 // "no background tasks" + footer
                    } else {
                        let visible = (count as u16).min(5);
                        visible + u16::from(count > 5) + 1
                    }
                }
                // Header + six content rows + key-hint footer.
                TasksView::Output { .. } => 8,
            }
        } else {
            0
        }
    }

    /// Replace the whole input, cursor parked at the end — the shape
    /// every whole-text assignment wants (completions, queue
    /// handback, history loads). Navigation, not an edit: nothing
    /// lands on the undo stack (#65).
    fn set_input(&mut self, text: String) {
        self.cursor = text.len();
        self.input = text;
    }

    /// Push the current composer state as one closed undo unit,
    /// capped at [`UNDO_CAP`] (#65).
    fn push_undo_unit(&mut self) {
        self.undo.push(UndoStep {
            input: self.input.clone(),
            cursor: self.cursor,
            run_end: usize::MAX,
        });
        if self.undo.len() > UNDO_CAP {
            self.undo.remove(0);
        }
    }

    /// Snapshot before a NEW edit: push the unit and drop any redo
    /// future — every composer edit flows through here (#65). Redo
    /// itself pushes without the clear, or one Alt+Z would strand
    /// the rest of the stack.
    fn snapshot(&mut self) {
        self.push_undo_unit();
        self.redo.clear();
    }

    /// One composer mutation: snapshot, then apply. The closure gets
    /// the input and the cursor, returns the new cursor.
    fn apply_edit(&mut self, edit: impl FnOnce(&mut String, usize) -> usize) {
        self.snapshot();
        // Any non-kill edit ends a kill run; kills re-record theirs
        // through `record_kill` after this clears.
        self.last_kill = None;
        let mut input = std::mem::take(&mut self.input);
        self.cursor = edit(&mut input, self.cursor);
        self.input = input;
    }

    /// Remove `range` as a kill (#64): the text joins the ring —
    /// merging into the previous entry while the kills abut — and
    /// the removal is one undo unit like any edit.
    fn kill_range(&mut self, range: std::ops::Range<usize>) {
        if range.start >= range.end {
            return;
        }
        let prev = self.last_kill;
        let text = self.input[range.clone()].to_string();
        let len = text.len();
        let start = range.start;
        self.apply_edit(move |input, _| {
            input.replace_range(range, "");
            start
        });
        if let Some((prev_start, prev_end)) = prev
            && let Some(entry) = self.kill_ring.last_mut()
        {
            if prev_end == start {
                // Forward-contiguous: the run reads on.
                entry.push_str(&text);
                self.last_kill = Some((prev_start, prev_end + len));
                return;
            }
            if prev_start == start + len {
                // Backward-contiguous: killed text lands before.
                *entry = format!("{text}{entry}");
                self.last_kill = Some((prev_start - len, prev_end));
                return;
            }
        }
        self.kill_ring.push(text);
        if self.kill_ring.len() > KILL_CAP {
            self.kill_ring.remove(0);
        }
        self.last_kill = Some((start, start + len));
    }

    /// Ctrl+Y: paste the newest kill at the cursor (#64). Silent when
    /// the ring is empty.
    fn yank(&mut self) {
        let Some(text) = self.kill_ring.last().cloned() else {
            return;
        };
        let start = self.cursor;
        self.apply_edit(move |input, cursor| {
            input.insert_str(cursor, &text);
            cursor + text.len()
        });
        self.yank_site = Some((start, self.cursor, 0));
    }

    /// Alt+Y: swap the in-flight yank for the previous ring entry,
    /// wrapping past the oldest (#64). Only a yank can be popped —
    /// `site` is the yank captured before this keypress, if any.
    fn yank_pop(&mut self, site: Option<(usize, usize, usize)>) {
        let Some((start, end, idx)) = site else {
            return;
        };
        if self.kill_ring.is_empty() {
            return;
        }
        let next = (idx + 1) % self.kill_ring.len();
        let text = self.kill_ring[self.kill_ring.len() - 1 - next].clone();
        let len = text.len();
        self.apply_edit(move |input, _| {
            input.replace_range(start..end, &text);
            start + len
        });
        self.yank_site = Some((start, start + len, next));
    }

    /// A plain character insert, coalescing into an open run —
    /// emacs' self-insert-command. The run continues only while the
    /// cursor sits exactly where the last insert ended; any other key
    /// or cursor move starts a fresh unit.
    fn apply_insert(&mut self, c: char) {
        let merges = self
            .undo
            .last()
            .is_some_and(|step| step.run_end == self.cursor);
        if !merges {
            self.snapshot();
        }
        let mut input = std::mem::take(&mut self.input);
        self.cursor = editor::insert_char(&mut input, self.cursor, c);
        self.input = input;
        if let Some(step) = self.undo.last_mut() {
            step.run_end = self.cursor;
        }
    }

    /// Ctrl+Z: revert the newest unit. The stack only ever holds
    /// composer mutations, so undo never drags navigation with it.
    fn undo_edit(&mut self) {
        let Some(step) = self.undo.pop() else {
            return;
        };
        self.redo
            .push((std::mem::take(&mut self.input), self.cursor));
        self.input = step.input;
        self.cursor = step.cursor;
        self.sync_completion();
    }

    /// Alt+Z: reapply the newest undone edit. The state undo came
    /// from rejoins the stack as a closed unit.
    fn redo_edit(&mut self) {
        let Some((input, cursor)) = self.redo.pop() else {
            return;
        };
        self.push_undo_unit();
        self.input = input;
        self.cursor = cursor;
        self.sync_completion();
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

    /// Insert a newline at the cursor — the multi-line composer
    /// (AD-0018). The input stops being a command draft the moment a
    /// newline lands in it, so the completion re-syncs.
    fn insert_newline(&mut self) {
        self.apply_insert('\n');
        self.sync_completion();
    }

    /// Up: the single-line contract is unchanged — history from
    /// anywhere (AD-0014). A multi-line input walks the visual rows
    /// first, column-preserving, pi-style; at the first visual row it
    /// recalls history when the caret sits at an edge (empty input,
    /// already browsing, or column 0) and otherwise clamps to the row
    /// start (AD-0018).
    fn up_or_history(&mut self) {
        let width = self.width.max(1) as usize;
        let (row, col) = crate::wrap::cursor_pos(&self.input, self.cursor, width);
        if row > 0 {
            self.cursor = crate::wrap::row_col_to_offset(&self.input, row - 1, col, width);
            return;
        }
        let single_row = crate::wrap::wrapped_row_count(&self.input, width) == 1;
        if single_row || self.input.is_empty() || self.history_pos.is_some() || col == 0 {
            self.history_up();
        } else {
            self.cursor = 0;
        }
    }

    /// Down: the mirror of [`Self::up_or_history`] — visual rows
    /// first, column-preserving; the last visual row recalls history
    /// only while already browsing (or on the single-line contract)
    /// and otherwise clamps to the row end (AD-0018).
    fn down_or_history(&mut self) {
        let width = self.width.max(1) as usize;
        let (row, col) = crate::wrap::cursor_pos(&self.input, self.cursor, width);
        let rows = crate::wrap::wrapped_row_count(&self.input, width);
        if row + 1 < rows {
            self.cursor = crate::wrap::row_col_to_offset(&self.input, row + 1, col, width);
            return;
        }
        let single_row = rows == 1;
        if single_row || self.history_pos.is_some() {
            self.history_down();
        } else {
            self.cursor = crate::wrap::row_col_to_offset(&self.input, row, usize::MAX, width);
        }
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
                    let name = *name;
                    self.apply_edit(|input, _| {
                        *input = format!("/{name}");
                        input.len()
                    });
                    self.sync_completion();
                }
                true
            }
            KeyCode::Enter => {
                self.completion = None;
                self.input.clear();
                self.cursor = 0;
                if let Some((name, _)) = completion.items.get(completion.selected) {
                    self.run_command(name, "");
                }
                true
            }
            _ => false,
        }
    }

    /// Execute a slash command by name. One arm per registry entry —
    /// a new registry entry without an arm lands here as an honest
    /// "unknown command" instead of a silent nothing. `args` carries
    /// everything after the name ("/tasks 3" → "3"); commands that
    /// take none ignore it.
    pub(crate) fn run_command(&mut self, name: &'static str, args: &str) {
        match name {
            "exit" | "quit" => self.quit = true,
            "model" => self.open_picker(),
            "resume" => self.open_session_picker(),
            "settings" => self.open_settings_board(),
            "tasks" => self.open_tasks_view(args),
            "tools" => self
                .messages
                .push(Message::System(Self::tools_listing(&self.tools))),
            "keybinds" => self
                .messages
                .push(Message::System(crate::keybinds::listing())),
            "context" => self.context_panel = Some(ContextPanel),
            "thinking" => self.command_thinking(args),
            "name" => self.command_name(args),
            "new" => self.command_new(args),
            other => self
                .messages
                .push(Message::System(format!("unknown command: /{other}"))),
        }
    }

    /// `/name <title>`: append the session's name record (#89) —
    /// names are appended, never rewritten, so the file keeps every
    /// choice. Bare `/name` shows the current one (pi parity). Same
    /// degrade-once contract as every session write.
    fn command_name(&mut self, args: &str) {
        let Some(session) = self.session.as_mut() else {
            self.messages
                .push(Message::System("not persisting — nothing to name".into()));
            return;
        };
        if args.is_empty() {
            let note = match session.name() {
                Some(name) => format!("session name: {name}"),
                None => "session has no name".into(),
            };
            self.messages.push(Message::System(note));
            return;
        }
        match session.rename(args) {
            Ok(()) => {
                let named = session.name().unwrap_or_default().to_string();
                if named.is_empty() {
                    self.messages
                        .push(Message::System("session name cleared".into()));
                } else {
                    self.messages
                        .push(Message::System(format!("session named: {named}")));
                }
            }
            Err(err) => {
                self.session = None;
                self.messages
                    .push(Message::System(format!("session not saved: {err}")));
            }
        }
    }

    /// Open the `/tasks` pane (AD-0021). Bare `/tasks` lists; an id
    /// (`/tasks 3`, or `/tasks #3` in the listing's own spelling)
    /// opens that task's output directly. Honesty rules: no manager
    /// (--mock sessions) and unknown ids say so in the transcript
    /// instead of opening an empty pane.
    fn open_tasks_view(&mut self, args: &str) {
        let Some(manager) = self.tasks.as_ref() else {
            self.messages.push(Message::System(
                "background tasks are unavailable in this session".into(),
            ));
            return;
        };
        let args = args.trim();
        if args.is_empty() {
            self.tasks_view = Some(TasksView::List { selected: 0 });
            return;
        }
        let number = args.strip_prefix('#').unwrap_or(args);
        match number.parse::<u64>() {
            Ok(id) if manager.status(id).is_some() => {
                self.tasks_view = Some(TasksView::Output { id });
            }
            Ok(id) => self
                .messages
                .push(Message::System(format!("no background task #{id}"))),
            Err(_) => self
                .messages
                .push(Message::System("usage: /tasks [id]".into())),
        }
    }

    /// Keys for the open `/tasks` pane: Esc backs out (output → list,
    /// list → closed), arrows walk the list with the pickers' wrap,
    /// Enter opens the selected task's output. Everything else is
    /// swallowed — the pane owns the keyboard while open.
    fn handle_tasks_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.tasks_view = match self.tasks_view {
                    Some(TasksView::Output { .. }) => Some(TasksView::List { selected: 0 }),
                    _ => None,
                };
            }
            KeyCode::Up | KeyCode::Down
                if matches!(self.tasks_view, Some(TasksView::List { .. })) =>
            {
                let Some(TasksView::List { selected }) = self.tasks_view else {
                    return;
                };
                let count = self
                    .tasks
                    .as_ref()
                    .map(|manager| manager.list().len())
                    .unwrap_or(0);
                if count == 0 {
                    return;
                }
                let next = if key.code == KeyCode::Up {
                    selected.checked_sub(1).unwrap_or(count - 1)
                } else {
                    (selected + 1) % count
                };
                self.tasks_view = Some(TasksView::List { selected: next });
            }
            KeyCode::Enter if matches!(self.tasks_view, Some(TasksView::List { .. })) => {
                let id = match self.tasks_view {
                    Some(TasksView::List { selected }) => self
                        .tasks
                        .as_ref()
                        .map(|manager| manager.list().get(selected).map(|summary| summary.id))
                        .unwrap_or(None),
                    _ => None,
                };
                if let Some(id) = id {
                    self.tasks_view = Some(TasksView::Output { id });
                }
            }
            _ => {}
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
            renaming: None,
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
        // A rename in flight owns the keyboard: chars build the name,
        // Enter saves, Esc cancels. Everything else waits.
        if self
            .session_picker
            .as_ref()
            .is_some_and(|picker| picker.renaming.is_some())
        {
            return self.handle_picker_rename_key(key);
        }
        match key.code {
            KeyCode::Esc => self.session_picker = None,
            // Ctrl-R: rename the highlighted session (#89). The record
            // appends to that file; the row updates in place.
            KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(picker) = self.session_picker.as_mut() {
                    picker.renaming = Some(String::new());
                }
            }
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

    /// Keyboard owner while a picker rename is in flight. Enter with
    /// an empty buffer cancels — renames are only written for real
    /// names; clearing is naming it something else.
    fn handle_picker_rename_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                if let Some(picker) = self.session_picker.as_mut() {
                    picker.renaming = None;
                }
            }
            KeyCode::Enter => self.commit_picker_rename(),
            KeyCode::Backspace => {
                if let Some(picker) = self.session_picker.as_mut() {
                    let _ = picker.renaming.as_mut().unwrap().pop();
                }
            }
            KeyCode::Char(c) => {
                if let Some(picker) = self.session_picker.as_mut() {
                    picker.renaming.as_mut().unwrap().push(c);
                }
            }
            _ => {}
        }
    }

    /// Save the rename: the record appends to the highlighted row's
    /// file and the summary updates in place. A failed write closes
    /// the picker with the usual loud note — degrade once.
    fn commit_picker_rename(&mut self) {
        let buffer = match self.session_picker.as_mut() {
            Some(picker) => picker.renaming.take().unwrap_or_default(),
            None => return,
        };
        if buffer.trim().is_empty() {
            return;
        }
        let selected = self.session_picker.as_ref().expect("open").selected;
        let visible = self.session_picker_visible();
        let Some(index) = visible.get(selected).copied() else {
            return;
        };
        let path = self.session_picker.as_ref().expect("open").summaries[index]
            .path
            .clone();
        match Session::resume(&path).and_then(|mut session| {
            let outcome = session.rename(&buffer);
            outcome.map(|_| session)
        }) {
            Ok(session) => {
                if let Some(picker) = self.session_picker.as_mut() {
                    picker.summaries[index].header.name = session.name().map(str::to_string);
                }
            }
            Err(err) => {
                self.session_picker = None;
                self.messages.push(Message::Error(err));
            }
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
        self.adopt(
            session,
            loaded,
            format!("resumed {count} messages from {}", path.display()),
        );
    }

    /// The second half of every session switch: adopt the transcript,
    /// replay the bill (#85), reseed the ring (#87), re-key the prompt
    /// cache. `/new` and `/resume` walk the same reset so nothing
    /// session-scoped leaks across the seam (AD-0034).
    fn adopt(&mut self, session: Session, loaded: hi_core::session::LoadedSession, intro: String) {
        let mut messages = loaded.messages;
        messages.insert(0, Message::System(intro));
        self.messages = messages;
        self.usage = loaded.usage;
        self.usage_last_prompt = loaded.usage_last_prompt;
        self.history = seed_history(&self.messages);
        self.history_pos = None;
        self.history_draft = None;
        // Cache routing follows the session: the new id adopts the
        // prompt-cache key before any further turn is spawned.
        let cache_key = session.id().to_string();
        self.session = Some(session);
        // AD-0045 ruling 1: the session's recorded level outranks the
        // settings default — resume puts you back where the session
        // was. No record (old files, auto) keeps the current value;
        // auto is not forced.
        if let Some(level) = loaded
            .header
            .thinking
            .as_deref()
            .and_then(hi_core::thinking::ThinkingLevel::parse)
        {
            self.thinking = Some(level);
        }
        // AD-0046: the recorded model outranks the startup selection
        // when it is still available; a vanished model falls back to
        // the selection with a warning. This runs after the boot
        // chain has discovered and selected a model — restoration is
        // a post-open correction, never the discovery itself.
        let (recorded_provider, recorded_model) = (
            loaded.header.provider.as_str(),
            loaded.header.model.as_str(),
        );
        if recorded_provider != self.status.provider || recorded_model != self.status.model {
            if hi_core::selection::model_available(&self.config, recorded_provider, recorded_model)
            {
                self.status = Status {
                    provider: recorded_provider.to_string(),
                    model: recorded_model.to_string(),
                    tool_count: self.status.tool_count,
                };
            } else {
                self.messages.push(Message::System(format!(
                    "session recorded model {recorded_provider}/{recorded_model} — not available; using {}",
                    self.status.model
                )));
            }
        }
        let (provider, model) = (self.status.provider.clone(), self.status.model.clone());
        self.rebuild_provider(&provider, &model, Some(cache_key));
        self.scroll.refold();
        self.queued = None;
    }

    /// `/new [name]`: a fresh session for the cwd we are in. Creation
    /// rides the startup seam (`Session::start`), the walk to it the
    /// resume seam's reset (`adopt`); the model and thinking level
    /// carry because they are status, not session state (AD-0034).
    /// `/thinking [level|auto]`: bare shows the ladder with per-model
    /// marks (AD-0044 ruling 2); a level switches mid-session (rebuilds
    /// the provider, ruling 2's persistence) and writes settings.json;
    /// `auto` clears back to provider-decides. No picker in v1 — the
    /// ladder listing is the marks surface.
    fn command_thinking(&mut self, args: &str) {
        let raw = args.trim().to_lowercase();
        if raw.is_empty() {
            // Bare /thinking is the picker: the ladder with per-model
            // marks, preselected at the current state (AD-0044
            // addendum — the listing grew up into a modal).
            let selected = match self.thinking.as_ref().map(|l| l.name()) {
                Some(name) => Self::THINKING_CHOICES
                    .iter()
                    .position(|c| c.as_ref().is_some_and(|l| l.name() == name))
                    .unwrap_or(Self::THINKING_CHOICES.len() - 1),
                None => Self::THINKING_CHOICES.len() - 1,
            };
            self.thinking_picker = Some(ThinkingPicker { selected });
            return;
        }
        let level = if raw == "auto" {
            None
        } else {
            match hi_core::thinking::ThinkingLevel::parse(&raw) {
                Some(level) => Some(level),
                None => {
                    self.messages.push(Message::System(format!(
                        "unknown thinking level: {raw} — try /thinking for the ladder"
                    )));
                    return;
                }
            }
        };
        self.apply_thinking_choice(level);
    }

    /// Row order of the thinking picker: the seven ladder levels,
    /// then auto (send nothing, provider decides).
    const THINKING_CHOICES: [Option<hi_core::thinking::ThinkingLevel>; 8] = {
        use hi_core::thinking::ThinkingLevel as T;
        [
            Some(T::Off),
            Some(T::Minimal),
            Some(T::Low),
            Some(T::Medium),
            Some(T::High),
            Some(T::Xhigh),
            Some(T::Max),
            None,
        ]
    };

    /// Switch the level, rebuild the provider, persist. The one
    /// commit path — the picker's Enter and the direct
    /// `/thinking <level>` argument both land here.
    fn apply_thinking_choice(&mut self, level: Option<hi_core::thinking::ThinkingLevel>) {
        self.thinking = level;
        self.rebuild_provider(
            &self.status.provider.clone(),
            &self.status.model.clone(),
            None,
        );
        match hi_core::Settings::persist_thinking(level) {
            Ok(()) => {
                let label = level
                    .as_ref()
                    .map(|l| l.name())
                    .unwrap_or("auto (provider decides)");
                self.messages
                    .push(Message::System(format!("thinking: {label} (saved)")));
            }
            Err(err) => self.messages.push(Message::System(format!(
                "thinking level switched for this session, but not saved: {err}"
            ))),
        }
        // AD-0045 ruling 2: the live header follows the switch, so
        // the next resume of this session restores the last level.
        // Best-effort surface: an IO failure is reported but never
        // rolls the switch back.
        if let Some(session) = self.session.as_mut()
            && let Err(err) = session.set_thinking(level.as_ref().map(|l| l.name()))
        {
            self.messages.push(Message::Error(err));
        }
    }

    /// Keyboard owner while the thinking picker is open.
    fn handle_thinking_picker_key(&mut self, key: KeyEvent) {
        let Some(picker) = self.thinking_picker.as_mut() else {
            return;
        };
        let last = Self::THINKING_CHOICES.len() - 1;
        match key.code {
            KeyCode::Esc => self.thinking_picker = None,
            KeyCode::Up => {
                picker.selected = if picker.selected == 0 {
                    last
                } else {
                    picker.selected - 1
                };
            }
            KeyCode::Down => {
                picker.selected = (picker.selected + 1) % (last + 1);
            }
            KeyCode::Enter => {
                let choice = Self::THINKING_CHOICES
                    .get(picker.selected)
                    .cloned()
                    .flatten();
                self.thinking_picker = None;
                self.apply_thinking_choice(choice);
            }
            _ => {}
        }
    }

    /// The picker's ladder rows plus any vendor note lines (drawn
    /// muted, not selectable). Marks fresh each frame: `✓` where the
    /// level reaches the wire (a `thinkingLevelMap` entry, a
    /// URL-family default, or vendor-published efforts; auto always),
    /// `–` where it would send nothing. Honest marks only — no
    /// guessing (AD-0044).
    pub(crate) fn thinking_menu(&self) -> (Vec<String>, Vec<String>) {
        let config = &self.config;
        let status = &self.status;
        let current = self.thinking;
        let entry = config.providers.get(&status.provider);
        let model = entry.and_then(|e| e.models.iter().find(|m| m.id == status.model));
        let map = model.and_then(|m| m.thinking_level_map.as_ref());
        let base_url = entry.map(|e| e.base_url.as_str()).unwrap_or("");
        let vendor = hi_core::catalog::lookup(&status.model);
        let mut rows = Vec::new();
        for choice in Self::THINKING_CHOICES {
            let (label, mark) = match choice {
                Some(level) => {
                    let mapped = map.is_some_and(|m| m.contains_key(level.name()))
                        || hi_core::thinking::family_kwargs(base_url, level).is_some()
                        || vendor
                            .and_then(|v| v.efforts.as_ref())
                            .is_some_and(|efforts| efforts.iter().any(|e| e == level.name()));
                    (
                        level.name().to_string(),
                        if mapped { "\u{2713}" } else { "\u{2013}" },
                    )
                }
                None => ("auto".to_string(), "\u{2713}"),
            };
            let default_mark = if choice.as_ref().is_some_and(|level| {
                vendor
                    .and_then(|v| v.default_effort.as_deref())
                    .is_some_and(|d| d == level.name())
            }) {
                " (vendor default)"
            } else {
                ""
            };
            let current_mark = if choice.as_ref() == current.as_ref() {
                " \u{2190} current"
            } else {
                ""
            };
            rows.push(format!("  {label:<8} {mark}{default_mark}{current_mark}"));
        }
        let mut notes = Vec::new();
        if vendor.is_some_and(|v| v.reasoning_mandatory) {
            notes.push("this model cannot disable reasoning".to_string());
        }
        (rows, notes)
    }

    fn command_new(&mut self, args: &str) {
        if self.turn.is_some() {
            self.messages.push(Message::System(
                "cannot switch sessions while a turn is running — esc first".into(),
            ));
            return;
        }
        let trimmed = args.trim();
        let name = if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        };
        let (provider, model) = (self.status.provider.clone(), self.status.model.clone());
        let thinking = self.thinking.as_ref().map(|t| t.name().to_string());
        let started = match self.session_root.as_deref() {
            Some(root) => Session::start_in(
                root,
                &std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
                &provider,
                &model,
                thinking.as_deref(),
                name.as_deref(),
            ),
            None => Session::start(&provider, &model, thinking.as_deref(), name.as_deref()),
        };
        let session = match started {
            Ok(session) => session,
            Err(err) => {
                self.messages.push(Message::Error(err));
                return;
            }
        };
        // Nothing on disk yet: the file materializes on the first
        // recorded record (lazy session creation), so adoption reads
        // the in-memory snapshot instead of a file that does not exist.
        let intro = format!("new session {}", session.path().display());
        let loaded = session.fresh_snapshot();
        self.adopt(session, loaded, intro);
    }

    /// What the input's right-side title shows while work is in
    /// flight; `None` renders a plain rule. New activity states
    /// (context compaction, tool runs) adopt arms here.
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
    /// input; failures adopt the transient composer note. A failed
    /// paste never touches the transcript or the session.
    pub fn handle_clipboard_outcome(&mut self, outcome: crate::clipboard::ClipboardOutcome) {
        use crate::clipboard::ClipboardOutcome;
        match outcome {
            ClipboardOutcome::Image { path } => {
                self.snapshot();
                let (before, after) = self.split_at_cursor();
                self.input = crate::clipboard::insert_pasted(&before, &path, &after);
                self.cursor = self.input.len() - after.len();
            }
            ClipboardOutcome::Text(text) if !text.is_empty() => {
                self.snapshot();
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
        // Task completions next (#5): each adopts a dim System note in
        // the transcript and rides the next turn to the model. The
        // append range starts BEFORE the drain so finalize persists the
        // notes; an idle session has no finalize coming, so the notes
        // persist right here instead.
        let added_from = self.messages.len();
        let completions = self.drain_task_completions();
        let Some(turn) = self.turn.as_mut() else {
            // Idle: a fresh completion may earn an auto turn (AD-0022)
            // — opt-in, budget-bounded, and keyed on completions so a
            // command notice never spends a turn.
            self.maybe_auto_trigger(completions);
            self.persist_new(added_from);
            return;
        };
        // Mid-turn the same fact only arms: the one-turn invariant
        // holds, and the decision waits for the turn to finish.
        if completions > 0 {
            self.auto_armed = true;
        }
        let mut finished = false;
        // Messages produced by tool events queue here and land in the
        // transcript after the loop — `self.messages` can't be touched
        // while `turn` holds a borrow of `self`.
        let mut transcript: Vec<Message> = Vec::new();
        // The round's bill (AD-0026): merged across rounds within one
        // pump, applied to `self` after the borrow ends.
        let mut round_bill: Option<Usage> = None;
        while let Ok(event) = turn.rx.try_recv() {
            match event {
                Event::TurnStart => {} // nothing to do yet — tokens follow
                Event::Token(token) => {
                    hi_core::message::AssistantBlock::push_text(&mut turn.blocks, &token);
                }
                Event::Thinking { delta, field } => {
                    hi_core::message::AssistantBlock::push_thinking(
                        &mut turn.blocks,
                        &delta,
                        field,
                    );
                }
                Event::ToolCall(call) => {
                    // Narration before a call is its own assistant
                    // message: flush the accumulated blocks so the
                    // transcript keeps text and request separate.
                    if !turn.blocks.is_empty() {
                        let blocks = std::mem::take(&mut turn.blocks);
                        transcript.push(Message::Assistant(blocks));
                    }
                    transcript.push(Message::ToolCall(call));
                }
                Event::ToolResult(result) => transcript.push(Message::ToolResult(result)),
                Event::Usage(usage) => {
                    round_bill = Some(match round_bill {
                        Some(mut total) => {
                            total.merge(usage);
                            total
                        }
                        None => usage,
                    });
                }
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
        if let Some(bill) = round_bill {
            self.usage.merge(bill);
            // The latest round's prompt IS the context size: every
            // turn re-sends the whole conversation.
            self.usage_last_prompt = bill.prompt_tokens;
            // And the bill rides the session file (#85) — the front
            // end that resumes shows the totals.
            self.persist_usage(bill);
        }
        if finished {
            // `Option::take` removes the turn and leaves `None` behind.
            // The mutable borrow above has ended by now, so taking
            // ownership to move the text out is allowed.
            if let Some(turn) = self.turn.take() {
                // Partial output is still worth keeping; the error
                // entry goes below it so the failure is visible.
                if !turn.blocks.is_empty() {
                    self.messages.push(Message::Assistant(turn.blocks));
                }
                if let Some(err) = turn.error {
                    self.messages.push(Message::Error(err));
                }
            }
            // The reply finished — fire the queued message, if any,
            // as the very next turn. A queued human message carries
            // the armed completion notes to the model already, so it
            // consumes the arm: the human's instruction dominates and
            // no budget is spent (AD-0022).
            if let Some(queued) = self.queued.take() {
                self.auto_armed = false;
                self.messages.push(Message::User(queued));
                self.start_turn();
            } else if self.auto_armed {
                self.auto_armed = false;
                self.fire_auto_trigger();
            }
        }
        // Persist everything finalized this frame.
        self.persist_new(added_from);
    }

    /// Append the message just pushed by an event handler. Event
    /// handling runs before the frame's `added_from` marker is
    /// captured, so anything pushed here is invisible to the frame's
    /// own range and must ride its own append.
    fn persist_last(&mut self) {
        let last = self.messages.len().saturating_sub(1);
        self.persist_new(last);
    }

    /// Append everything since `added_from` to the session file. An
    /// append failure degrades once, loudly — the session is dropped so
    /// the note is not spammed every frame, and chat continues without
    /// disk.
    fn persist_new(&mut self, added_from: usize) {
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

    /// Append the round's bill to the session file — the `usage`
    /// record (#85). Same degrade-once contract as `persist_new`: one
    /// loud note, then chat continues without disk.
    fn persist_usage(&mut self, bill: Usage) {
        let append_error = self
            .session
            .as_mut()
            .and_then(|session| session.append_usage(&bill).err());
        if let Some(err) = append_error {
            self.session = None;
            self.messages
                .push(Message::System(format!("session not saved: {err}")));
        }
    }

    /// Fold finished background tasks into the transcript as System
    /// notes (#5). The fact rides the next turn's message payload; the
    /// status row's running count clears itself on the next draw.
    /// Returns how many completions folded — the auto-trigger keys on
    /// this, not on generic transcript growth.
    fn drain_task_completions(&mut self) -> usize {
        let Some(tasks) = self.tasks.as_ref() else {
            return 0;
        };
        let mut completions = 0;
        while let Some(completion) = tasks.try_recv_completion() {
            let wall = format_task_wall(completion.wall);
            self.messages.push(Message::System(format!(
                "background task #{} '{}' {} after {}",
                completion.id,
                completion.name,
                completion.state.label(),
                wall
            )));
            completions += 1;
        }
        completions
    }

    /// Idle-path auto trigger (AD-0022): spend one budget unit, push
    /// the marked mandate, and start the turn. The completion notes
    /// are already in the transcript — `start_turn` snapshots it — so
    /// the model sees the facts and the mandate together.
    fn maybe_auto_trigger(&mut self, completions: usize) {
        if completions == 0 || !self.task_turns || self.auto_trigger_budget == 0 {
            return;
        }
        self.fire_auto_trigger();
    }

    /// Spend one budget unit and start the mandate turn. The caller
    /// has already checked the opt-in and the budget.
    fn fire_auto_trigger(&mut self) {
        self.auto_trigger_budget -= 1;
        if self.auto_trigger_budget == 0 {
            // The silence after this must be explained: one dim note,
            // riding the same final auto turn.
            self.messages
                .push(Message::System(AUTO_TRIGGER_EXHAUSTED.into()));
        }
        self.messages
            .push(Message::User(AUTO_TRIGGER_MANDATE.into()));
        self.start_turn();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hi_core::message::AssistantBlock;
    use ratatui::crossterm::event::KeyEvent;
    use std::path::PathBuf;
    use std::sync::Mutex;

    /// The two tests that persist through `apply_thinking_choice`
    /// still serialize: their read-modify-write on the one scratch
    /// `settings.json` must not interleave in-process.
    static SETTINGS_LOCK: Mutex<()> = Mutex::new(());

    fn app() -> App {
        test_app()
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    /// An app with a live task manager, for the `/tasks` pane tests.
    #[cfg(test)]
    fn app_with_tasks(tag: &str) -> (App, hi_core::tasks::TaskManager) {
        let mut app = test_app();
        let tasks = hi_core::tasks::TaskManager::new(
            std::env::temp_dir().join(format!("hi-tui-tasks-{tag}")),
        );
        app.tasks = Some(tasks.clone());
        (app, tasks)
    }

    /// Block until a spawned task has finished (no fixed sleeps).
    fn wait_task_finished(tasks: &hi_core::tasks::TaskManager, id: u64) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !tasks
            .status(id)
            .is_some_and(|status| status.state.finished())
        {
            assert!(
                std::time::Instant::now() < deadline,
                "task #{id} never finished"
            );
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    }

    /// Pump until the turn loop reports idle (mock replies are short;
    /// auto-fired follow-ups ride the same loop).
    fn drain_turns(app: &mut App) {
        for _ in 0..400 {
            app.pump();
            if app.turn.is_none() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("turn never finished");
    }

    /// Pump until the predicate holds. Completion notes travel
    /// through the reaper's channel: the state flag can flip a hair
    /// before the note is sent, so a single pump after
    /// `wait_task_finished` can race it (the #82 CI flake). Predicate
    /// waits only — no fixed sleeps.

    #[test]
    fn a_sent_turn_records_the_user_line_then_the_reply() {
        let root = std::env::temp_dir().join(format!("hi-tui-record-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut app = test_app();
        app.session_root = Some(root.clone());
        app.run_command("new", "");
        // Lazy: nothing on disk yet.
        assert!(!app.session.as_ref().unwrap().path().exists());
        app.set_input("record me".to_string());
        app.handle_key(key(KeyCode::Enter));
        pump_until(&mut app, "the turn", |a| a.turn.is_none());
        let path = app.session.as_ref().unwrap().path().to_path_buf();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains("record me"),
            "the user line must be recorded: {raw:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    fn pump_until(app: &mut App, what: &str, done: impl Fn(&App) -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            app.pump();
            if done(app) {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "never: {what}");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// How many machine-initiated mandate turns exist so far.
    #[test]
    fn the_board_is_two_rows_and_the_model_lives_on_the_second() {
        let app = test_app();
        let rows = app.status_rows();
        assert_eq!(rows.len(), 2, "the model made row 1 a resident");
        assert!(!rows[0].0.is_empty(), "location owns row 0's left");
        assert!(rows[0].1.is_empty(), "row 0's right waits for residents");
        assert!(rows[1].0.is_empty());
        assert_eq!(rows[1].1, "(mock) mock · thinking:auto");
    }

    #[test]
    fn tasks_join_the_model_row_and_the_arm_appends() {
        let (mut app, tasks) = app_with_tasks("status-board");
        let id = tasks
            .spawn(hi_core::tasks::TaskSpec::process(
                "sleep 2",
                std::env::temp_dir(),
            ))
            .unwrap();
        app.tasks = Some(tasks);
        let rows = app.status_rows();
        // Tasks count on the bottom row's right — under the model.
        assert!(rows[0].1.starts_with("tasks: 1 running"), "{:?}", rows[0]);
        assert_eq!(
            rows[1].1, "(mock) mock · thinking:auto",
            "the model stays right"
        );
        wait_task_finished(app.tasks.as_ref().unwrap(), id);
        // The arm appends after the model — transient noise rides
        // right (#72's visible feedback).
        app.tasks = None;
        app.auto_armed = true;
        let rows = app.status_rows();
        assert!(rows[0].1.is_empty());
        assert_eq!(rows[1].1, "(mock) mock · thinking:auto · auto: armed");
    }

    #[test]
    fn usage_fills_row_0s_right_and_the_share_needs_a_window() {
        let mut app = test_app();
        app.usage = Usage {
            prompt_tokens: 1000,
            completion_tokens: 250,
            cached_tokens: Some(700),
            reasoning_tokens: Some(100),
        };
        app.usage_last_prompt = 1000;
        // The mock's bill sits top-left: net ↓300 of 1000, no window
        // configured.
        assert_eq!(app.status_rows()[1].0, "↑250 ↓300 R100 CH70.0%");
        // A configured window joins the share — nothing is guessed.
        let mut providers = std::collections::BTreeMap::new();
        providers.insert(
            "mock".to_string(),
            hi_core::ProviderConfig {
                base_url: "http://x/v1".into(),
                api_key: None,
                models: vec![hi_core::ModelConfig {
                    id: "mock".into(),
                    context_window: Some(1_000_000),
                    ..Default::default()
                }],
            },
        );
        app.config = ModelsConfig { providers };
        assert_eq!(app.status_rows()[1].0, "↑250 ↓300 R100 CH70.0% 0.1%/1M");
    }

    #[test]
    fn startup_seeds_the_bill_from_the_session_init() {
        // The startup-resume arm of `App::new`: the replayed bill is
        // on the board before anything live happens.
        let init = crate::SessionInit {
            system_prompt: None,
            thinking: None,
            tools: Arc::new(ToolRegistry::new()),
            messages: Vec::new(),
            session: None,
            usage: Usage {
                prompt_tokens: 800,
                completion_tokens: 60,
                cached_tokens: Some(300),
                reasoning_tokens: None,
            },
            usage_last_prompt: 800,
            images: hi_core::ImageGate::new(false),
            images_setting: true,
            tasks: None,
            task_turns: false,
            max_tool_rounds: hi_core::DEFAULT_MAX_TOOL_ROUNDS,
            subagent: hi_core::subagent::SubagentRunner::new(
                Default::default(),
                std::env::temp_dir(),
                false,
                None,
            ),
        };
        let app = app_with(init);
        assert_eq!(app.usage.completion_tokens, 60);
        assert_eq!(app.usage_last_prompt, 800);
        let bill = app.status_rows()[1].0.clone();
        assert!(bill.contains("↑60 ↓500"), "{bill}");
    }

    #[test]
    fn startup_replays_the_ring_from_the_init_transcript() {
        // The ring is a view of the transcript (#87): user prompts
        // seed it oldest-first, everything else is invisible to it.
        let init = crate::SessionInit {
            system_prompt: None,
            thinking: None,
            tools: Arc::new(ToolRegistry::new()),
            messages: vec![
                Message::System("resumed 3 messages".into()),
                Message::User("first".into()),
                Message::assistant("hi"),
                Message::User("second".into()),
            ],
            session: None,
            usage: Usage::default(),
            usage_last_prompt: 0,
            images: hi_core::ImageGate::new(false),
            images_setting: true,
            tasks: None,
            task_turns: false,
            max_tool_rounds: hi_core::DEFAULT_MAX_TOOL_ROUNDS,
            subagent: hi_core::subagent::SubagentRunner::new(
                Default::default(),
                std::env::temp_dir(),
                false,
                None,
            ),
        };
        let app = app_with(init);
        assert_eq!(app.history, vec!["first".to_string(), "second".to_string()]);
    }

    #[test]
    fn model_switch_restamps_the_subagent_runner() {
        let mut app = test_app();
        assert_eq!(app.subagent.current(), None);
        app.config =
            serde_json::from_str(r#"{"providers":{"mprov":{"baseUrl":"http://127.0.0.1:1/v1"}}}"#)
                .unwrap();
        app.apply_model_choice("mprov/m".into());
        // Inherited subagent runs follow the switch, never a stale
        // identity.
        assert_eq!(
            app.subagent.current(),
            Some(("mprov".to_string(), "m".to_string()))
        );
        assert_eq!(app.status.provider, "mprov");
    }

    #[test]
    fn a_new_command_starts_a_fresh_session_for_this_directory() {
        // /new creates a session file whose header names the cwd we
        // are in, then walks the same reset resume walks — fresh
        // transcript, fresh bill, fresh ring — under a new id.
        let root = std::env::temp_dir().join(format!("hi-new-session-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut app = test_app();
        app.session_root = Some(root.clone());
        let cwd = std::env::current_dir().unwrap();
        let mut first =
            hi_core::Session::start_in(&root, &cwd, "mock", "mock", None, None).unwrap();
        first.materialize().unwrap();
        app.switch_session(first.path());
        // A live bill and transcript from the session we are leaving.
        app.usage = Usage {
            prompt_tokens: 800,
            completion_tokens: 60,
            cached_tokens: None,
            reasoning_tokens: None,
        };
        app.messages
            .push(Message::User("work from the old session".into()));

        let old_id = app.session.as_ref().unwrap().id().to_string();
        app.run_command("new", "");

        let note = match app.messages.as_slice() {
            [Message::System(note)] => note.clone(),
            other => panic!("expected one fresh-system note, got {other:?}"),
        };
        assert!(note.starts_with("new session "), "{note}");
        assert_eq!(app.usage.completion_tokens, 0, "the bill resets");
        assert!(app.history.is_empty(), "the ring resets");
        let new_id = app.session.as_ref().unwrap().id().to_string();
        assert_ne!(new_id, old_id, "a new session id took over");
        // Lazy creation: /new alone writes nothing — the file appears
        // on the first recorded record, not at open time.
        assert!(!app.session.as_ref().unwrap().path().exists());
        app.session
            .as_mut()
            .unwrap()
            .materialize()
            .expect("materialize");
        // Now it exists, headed by the cwd we are in.
        let loaded = hi_core::session::load(app.session.as_ref().unwrap().path()).unwrap();
        assert_eq!(loaded.header.cwd, cwd.to_string_lossy());
        // An optional argument names the session it creates.
        app.run_command("new", "scratch");
        app.session
            .as_mut()
            .unwrap()
            .materialize()
            .expect("materialize");
        let second = hi_core::session::load(app.session.as_ref().unwrap().path()).unwrap();
        assert_eq!(second.header.name.as_deref(), Some("scratch"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_resumed_session_replays_the_ring_and_keeps_the_ceiling() {
        // A session file whose transcript outgrew the ring: only the
        // newest HISTORY_CAP prompts replay, drop-oldest, same rule
        // the live ring follows (AD-0014).
        let root = std::env::temp_dir().join(format!("hi-replay-ring-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut session = hi_core::Session::start_in(
            &root,
            std::path::Path::new("/p"),
            "mock",
            "mock",
            None,
            None,
        )
        .unwrap();
        let recorded: Vec<Message> = (0..105)
            .map(|i| Message::User(format!("prompt {i}")))
            .collect();
        session.append(&recorded).unwrap();
        let path = session.path().to_path_buf();

        let mut app = test_app();
        app.switch_session(&path);
        assert_eq!(app.history.len(), 100);
        assert_eq!(app.history.first().unwrap(), "prompt 5");
        assert_eq!(app.history.last().unwrap(), "prompt 104");

        // And the ring stays live: a submission after the replay
        // pushes on top of it.
        app.input = "after the resume".into();
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.history.last().unwrap(), "after the resume");
        assert_eq!(app.history.len(), 100, "the ceiling holds across the seam");
        assert_eq!(app.history.first().unwrap(), "prompt 6", "oldest dropped");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_resumed_session_restores_the_bill_and_the_next_turn_merges() {
        // A session file with a recorded bill, written the way
        // turn-end writes it.
        let root = std::env::temp_dir().join(format!("hi-resume-bill-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut session = hi_core::Session::start_in(
            &root,
            std::path::Path::new("/p"),
            "mock",
            "mock",
            None,
            None,
        )
        .unwrap();
        session.append(&[Message::User("recorded".into())]).unwrap();
        session
            .append_usage(&Usage {
                prompt_tokens: 500,
                completion_tokens: 40,
                cached_tokens: Some(200),
                reasoning_tokens: None,
            })
            .unwrap();
        let path = session.path().to_path_buf();

        let mut app = test_app();
        app.switch_session(&path);
        // The recorded bill replays: merged, the last recorded prompt
        // the context fact.
        assert_eq!(app.usage.completion_tokens, 40);
        assert_eq!(app.usage_last_prompt, 500);
        let bill = app.status_rows()[1].0.clone();
        assert!(bill.contains("↑40"), "{bill}");

        // The next live turn merges on top — the mock's fixed bill
        // joins the recorded one.
        app.start_turn();
        drain_turns(&mut app);
        assert_eq!(app.usage.completion_tokens, 40 + 250);
        assert_eq!(app.usage.cached_tokens, Some(200 + 700));
        assert_eq!(
            app.usage_last_prompt, 1000,
            "the live turn is the latest context"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_settled_mock_turn_leaves_the_bill_on_the_board() {
        let mut app = test_app();
        app.start_turn();
        drain_turns(&mut app);
        // The mock's fixed bill rode the settled turn end to end.
        assert_eq!(app.usage.completion_tokens, 250);
        assert_eq!(app.usage.cached_tokens, Some(700));
        assert_eq!(app.usage_last_prompt, 1000);
        let bill = app.status_rows()[1].0.clone();
        assert!(bill.contains("↑250 ↓300"), "{bill}");
        assert!(bill.contains("CH70.0%"), "{bill}");
        assert!(app.status_rows()[0].1.is_empty(), "no tasks, no counts");
    }

    fn mandate_count(app: &App) -> usize {
        app.messages
            .iter()
            .filter(|m| matches!(m, Message::User(text) if text.contains("(auto-trigger)")))
            .count()
    }

    #[test]
    fn auto_trigger_fires_an_idle_turn_on_completion() {
        let (mut app, tasks) = app_with_tasks("auto-idle");
        app.task_turns = true;
        let id = tasks
            .spawn(hi_core::tasks::TaskSpec::process(
                "echo done",
                std::env::temp_dir(),
            ))
            .unwrap();
        wait_task_finished(&tasks, id);
        // An idle pump folds the note and fires the mandate turn —
        // pump until it does (the note races the state flag).
        pump_until(&mut app, "the idle trigger", |a| a.turn.is_some());
        assert!(app.turn.is_some(), "no auto turn started");
        assert_eq!(
            mandate_count(&app),
            1,
            "the mandate speaks once, as a User message"
        );
        assert_eq!(app.auto_trigger_budget, AUTO_TRIGGER_BUDGET - 1);
        drain_turns(&mut app);
        // The note rode the auto turn; the model's answer landed.
        assert!(app.messages.iter().any(|m| matches!(m,
            Message::System(text) if text.contains("finished (exit 0)"))));
    }

    #[test]
    fn default_off_leaves_the_note_for_the_human() {
        let (mut app, tasks) = app_with_tasks("auto-off");
        let id = tasks
            .spawn(hi_core::tasks::TaskSpec::process(
                "echo done",
                std::env::temp_dir(),
            ))
            .unwrap();
        wait_task_finished(&tasks, id);
        for _ in 0..5 {
            app.pump();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            app.turn.is_none(),
            "the default session must not spend turns"
        );
        assert_eq!(mandate_count(&app), 0);
        // The fact still reached the transcript for the human's next turn.
        assert!(app.messages.iter().any(|m| matches!(m,
            Message::System(text) if text.contains("finished (exit 0)"))));
    }

    #[test]
    fn mid_turn_completion_waits_for_the_turn_then_fires() {
        let (mut app, tasks) = app_with_tasks("auto-mid");
        app.task_turns = true;
        app.start_turn();
        let id = tasks
            .spawn(hi_core::tasks::TaskSpec::process(
                "echo done",
                std::env::temp_dir(),
            ))
            .unwrap();
        wait_task_finished(&tasks, id);
        // The completion folds while the turn runs: armed, not fired.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !app.auto_armed {
            assert!(std::time::Instant::now() < deadline, "never armed");
            app.pump();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(app.turn.is_some(), "a second turn must never run");
        assert_eq!(mandate_count(&app), 0);
        // The original turn ends; the arm turns into the mandate turn.
        drain_turns(&mut app);
        assert_eq!(mandate_count(&app), 1);
        assert_eq!(app.auto_trigger_budget, AUTO_TRIGGER_BUDGET - 1);
    }

    #[test]
    fn queued_human_message_consumes_the_armed_completion() {
        let (mut app, tasks) = app_with_tasks("auto-queued");
        app.task_turns = true;
        app.start_turn();
        type_string(&mut app, "follow up");
        app.handle_key(key(KeyCode::Enter));
        let id = tasks
            .spawn(hi_core::tasks::TaskSpec::process(
                "echo done",
                std::env::temp_dir(),
            ))
            .unwrap();
        wait_task_finished(&tasks, id);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !app.auto_armed {
            assert!(std::time::Instant::now() < deadline, "never armed");
            app.pump();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        drain_turns(&mut app);
        // The human's queued turn fired; it already carries the notes,
        // so no mandate, no spend.
        assert!(
            app.messages
                .iter()
                .any(|m| matches!(m, Message::User(text) if text == "follow up"))
        );
        assert_eq!(mandate_count(&app), 0);
        assert_eq!(app.auto_trigger_budget, AUTO_TRIGGER_BUDGET);
        assert!(!app.auto_armed);
    }

    #[test]
    fn auto_trigger_budget_bounds_the_chain() {
        let (mut app, tasks) = app_with_tasks("auto-budget");
        app.task_turns = true;
        app.auto_trigger_budget = 1;
        // First completion: fires and spends the last unit.
        let first = tasks
            .spawn(hi_core::tasks::TaskSpec::process(
                "echo one",
                std::env::temp_dir(),
            ))
            .unwrap();
        wait_task_finished(&tasks, first);
        pump_until(&mut app, "the first trigger", |a| a.turn.is_some());
        assert!(app.turn.is_some());
        drain_turns(&mut app);
        assert_eq!(app.auto_trigger_budget, 0);
        // Second completion: the silence is explained, exactly once.
        let second = tasks
            .spawn(hi_core::tasks::TaskSpec::process(
                "echo two",
                std::env::temp_dir(),
            ))
            .unwrap();
        wait_task_finished(&tasks, second);
        pump_until(&mut app, "the exhaustion note", |a| {
            a.messages.iter().any(|m| {
                matches!(m, Message::System(text) if text.contains("auto-trigger budget spent"))
            })
        });
        assert!(app.turn.is_none(), "budget spent — no more auto turns");
        assert_eq!(mandate_count(&app), 1);
        assert_eq!(
            app.messages
                .iter()
                .filter(|m| matches!(m,
            Message::System(text) if text.contains("auto-trigger budget spent")))
                .count(),
            1
        );
    }

    #[test]
    fn context_window_falls_back_to_the_catalog() {
        // AD-0044 tier chain: no config entry → the shipped vendor
        // table supplies the window (claude-opus-4.7 → 1M).
        let mut app = test_app();
        app.status.model = "claude-opus-4.7".into();
        assert_eq!(app.context_window(), Some(1_000_000));
        app.status.model = "totally-unknown-model".into();
        assert_eq!(app.context_window(), None);
    }

    #[test]
    fn context_command_opens_the_report_panel() {
        let mut app = test_app();
        app.run_command("context", "");
        assert!(app.context_panel.is_some(), "bare /context opens the panel");
        // Esc closes the read-only panel — and so does any other key:
        // there is nothing to select.
        app.handle_key(key(KeyCode::Esc));
        assert!(app.context_panel.is_none());
        app.run_command("context", "");
        app.handle_key(key(KeyCode::Char('x')));
        assert!(app.context_panel.is_none());
    }

    #[test]
    fn context_panel_rows_anchor_to_the_providers_bill() {
        let mut app = test_app();
        app.messages = vec![
            Message::User("hello there".into()),
            Message::assistant("hi"),
        ];
        app.usage_last_prompt = 1_000;
        let rows = app.context_panel_rows();
        let joined = rows.join("\n");
        assert!(
            rows[0].contains("1000"),
            "the header carries the exact bill: {joined}"
        );
        assert!(joined.contains("user"), "{joined}");
        assert!(joined.contains("assistant"), "{joined}");
        // No bill yet — the raw estimate stands alone.
        app.usage_last_prompt = 0;
        let rows = app.context_panel_rows();
        assert!(rows[0].contains("estimated"), "{:?}", rows[0]);
    }

    #[test]
    fn context_panel_buckets_thinking() {
        let mut app = test_app();
        app.messages = vec![
            Message::User("hello there".into()),
            Message::Assistant(vec![
                AssistantBlock::Thinking {
                    thinking: "pondering".into(),
                    signature: Some("reasoning_content".into()),
                    redacted: false,
                },
                AssistantBlock::Text("hi".into()),
            ]),
        ];
        let rows = app.context_panel_rows();
        let joined = rows.join("\n");
        assert!(
            joined.contains("thinking"),
            "reasoning is real context: {joined}"
        );
    }

    #[test]
    fn context_panel_shows_free_space_when_the_window_is_known() {
        let mut app = test_app();
        app.messages = vec![Message::User("hello".into())];
        app.usage_last_prompt = 100;
        app.status.model = "claude-opus-4.7".into();
        let rows = app.context_panel_rows();
        let joined = rows.join("\n");
        assert!(
            joined.contains("free") && joined.contains("999900"),
            "window minus the anchored total: {joined}"
        );
    }

    #[test]
    fn thinking_command_switches_level_directly() {
        let _guard = SETTINGS_LOCK.lock().unwrap();
        let mut app = test_app();
        // The argument form never opens a modal — power path.
        app.run_command("thinking", "high");
        assert!(app.thinking_picker.is_none());
        assert_eq!(app.thinking, Some(hi_core::thinking::ThinkingLevel::High));
        assert!(
            app.messages
                .iter()
                .any(|m| matches!(m, Message::System(t) if t.contains("thinking: high (saved)"))),
            "{:?}",
            app.messages
        );
        // Back to auto clears the level (provider decides again).
        app.run_command("thinking", "auto");
        assert_eq!(app.thinking, None);
        // Unknown levels are refused with guidance, not silently.
        let before = app.messages.len();
        app.run_command("thinking", "ultra");
        assert_eq!(app.thinking, None, "a refused level changes nothing");
        assert!(
            matches!(app.messages.last(), Some(Message::System(t)) if t.contains("unknown thinking level"))
        );
        assert_eq!(app.messages.len(), before + 1);
        assert!(app.thinking_picker.is_none());
    }

    #[test]
    fn thinking_bare_opens_the_ladder_picker() {
        let mut app = test_app();
        app.run_command("thinking", "");
        assert!(
            app.thinking_picker.is_some(),
            "bare /thinking opens the picker"
        );
        // Eight rows: the seven levels, then auto.
        let rows = app.thinking_menu().0;
        assert_eq!(rows.len(), 8);
        assert!(rows[0].trim().starts_with("off"), "{rows:?}");
        assert!(rows[7].trim().starts_with("auto"), "{rows:?}");
        assert!(
            rows.iter().any(|r| r.contains("\u{2190} current")),
            "{rows:?}"
        );
        // Esc closes without changing anything.
        app.handle_key(key(KeyCode::Esc));
        assert!(app.thinking_picker.is_none());
        assert_eq!(app.thinking, None);
    }

    #[test]
    fn thinking_picker_preselects_the_current_level() {
        let mut app = test_app();
        app.thinking = Some(hi_core::thinking::ThinkingLevel::High);
        app.run_command("thinking", "");
        assert_eq!(app.thinking_picker.as_ref().unwrap().selected, 4);
        app.thinking = None;
        app.run_command("thinking", "");
        assert_eq!(app.thinking_picker.as_ref().unwrap().selected, 7);
    }

    #[test]
    fn thinking_picker_enter_switches_wraps_and_persists() {
        let _guard = SETTINGS_LOCK.lock().unwrap();
        let mut app = test_app();
        app.run_command("thinking", "");
        // Wrap up from auto (preselected: nothing set) lands on max.
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.thinking_picker.as_ref().unwrap().selected, 6);
        app.handle_key(key(KeyCode::Enter));
        assert!(app.thinking_picker.is_none());
        assert_eq!(app.thinking, Some(hi_core::thinking::ThinkingLevel::Max));
        assert!(
            app.messages
                .iter()
                .any(|m| matches!(m, Message::System(t) if t.contains("thinking: max (saved)"))),
            "{:?}",
            app.messages
        );
        // Reopen: down from max wraps to auto; Enter clears.
        app.run_command("thinking", "");
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.thinking_picker.as_ref().unwrap().selected, 7);
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.thinking, None);
        assert!(
            app.messages
                .iter()
                .any(|m| matches!(m, Message::System(t) if t.contains("thinking: auto"))),
            "{:?}",
            app.messages
        );
    }

    #[test]
    fn settings_command_opens_the_board_and_esc_closes_it() {
        let mut app = test_app();
        app.run_command("settings", "");
        assert!(app.settings_board.is_some());
        // Two rows plus the key-hint footer.
        assert_eq!(app.menu_height(), 3);
        let rows = app.settings_rows();
        assert_eq!(rows, vec!["task turns: off", "images: on"]);
        app.handle_key(key(KeyCode::Esc));
        assert!(app.settings_board.is_none());
        assert_eq!(app.menu_height(), 0);
    }

    #[test]
    fn settings_toggle_enables_task_turns_mid_session() {
        // The acceptance test (#79): consent costs two keystrokes and
        // needs no restart — flip the board row, then a completion
        // fires the auto trigger in the same session.
        let (mut app, tasks) = app_with_tasks("settings-accept");
        assert!(!app.task_turns, "default is off");
        app.run_command("settings", "");
        app.handle_key(key(KeyCode::Enter));
        assert!(app.task_turns);
        assert!(app.settings_board.is_some(), "Enter keeps the board open");
        let id = tasks
            .spawn(hi_core::tasks::TaskSpec::process(
                "echo done",
                std::env::temp_dir(),
            ))
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !tasks.status(id).is_some_and(|s| s.state.finished()) {
            assert!(std::time::Instant::now() < deadline, "task never finished");
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        pump_until(&mut app, "the flipped flag's trigger", |a| a.turn.is_some());
        assert!(app.turn.is_some(), "the flipped flag fired with no restart");
        drain_turns(&mut app);
        assert_eq!(mandate_count(&app), 1);
    }

    #[test]
    fn settings_toggle_images_flips_the_master_switch() {
        let mut app = test_app();
        app.run_command("settings", "");
        // Arrow wrap: Up from the first row lands on the last.
        app.handle_key(key(KeyCode::Up));
        app.handle_key(key(KeyCode::Enter));
        assert!(!app.images_setting);
        // The mock provider has no config entry: the AD-0012 rebuild
        // is an honest no-op, and the flip still lands.
        assert!(app.messages.iter().any(|m| matches!(m,
            Message::System(text) if text.contains("images off for this session"))));
        // And back down through the wrap: the other row flips, this
        // one stays where Enter left it.
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Enter));
        assert!(app.task_turns);
        assert!(!app.images_setting);
    }

    #[test]
    fn tasks_command_opens_the_list_and_esc_closes_it() {
        let (mut app, tasks) = app_with_tasks("list");
        let _id = tasks
            .spawn(hi_core::tasks::TaskSpec::process(
                "echo hi",
                std::env::temp_dir(),
            ))
            .unwrap();
        app.run_command("tasks", "");
        assert!(matches!(
            app.tasks_view,
            Some(crate::app::TasksView::List { selected: 0 })
        ));
        // The pane is a real menu: it takes rows from the stream.
        assert!(app.menu_height() >= 2);
        app.handle_key(key(KeyCode::Esc));
        assert!(app.tasks_view.is_none());
        assert_eq!(app.menu_height(), 0);
    }

    #[test]
    fn tasks_command_with_an_id_opens_the_output_and_rejects_junk() {
        let (mut app, tasks) = app_with_tasks("byid");
        let _id = tasks
            .spawn(hi_core::tasks::TaskSpec::process(
                "echo hi",
                std::env::temp_dir(),
            ))
            .unwrap();
        app.run_command("tasks", "1");
        assert!(matches!(
            app.tasks_view,
            Some(crate::app::TasksView::Output { id: 1 })
        ));
        assert_eq!(app.menu_height(), 8); // header + 6 rows + footer
        // The listing's own spelling works too.
        app.tasks_view = None;
        app.run_command("tasks", "#1");
        assert!(matches!(
            app.tasks_view,
            Some(crate::app::TasksView::Output { id: 1 })
        ));
        // Unknown ids and non-numeric args earn transcript notices,
        // never a pane.
        app.tasks_view = None;
        app.run_command("tasks", "9");
        assert!(app.tasks_view.is_none());
        assert!(app.messages.iter().any(|m| matches!(&m,
            Message::System(text) if text.contains("no background task #9"))));
        app.run_command("tasks", "soon");
        assert!(app.messages.iter().any(|m| matches!(&m,
            Message::System(text) if text.contains("usage: /tasks"))));
    }

    #[test]
    fn tasks_list_keys_walk_enter_opens_output_and_esc_backs_out() {
        let (mut app, tasks) = app_with_tasks("walk");
        // Oldest first: #1 lingers, #2 finishes — the list order is
        // deterministic.
        let _ = tasks
            .spawn(hi_core::tasks::TaskSpec::process(
                "sleep 30",
                std::env::temp_dir(),
            ))
            .unwrap();
        let _ = tasks
            .spawn(hi_core::tasks::TaskSpec::process(
                "echo two",
                std::env::temp_dir(),
            ))
            .unwrap();
        app.run_command("tasks", "");
        app.handle_key(key(KeyCode::Down));
        let Some(crate::app::TasksView::List { selected }) = app.tasks_view else {
            panic!("list closed by a walk key");
        };
        assert_eq!(selected, 1);
        app.handle_key(key(KeyCode::Up));
        let Some(crate::app::TasksView::List { selected }) = app.tasks_view else {
            panic!("list closed by a walk key");
        };
        assert_eq!(selected, 0);
        // Enter opens the selected row's output; Esc returns to the
        // list, not to nothing.
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(
            app.tasks_view,
            Some(crate::app::TasksView::Output { id: 2 })
        ));
        app.handle_key(key(KeyCode::Esc));
        assert!(matches!(
            app.tasks_view,
            Some(crate::app::TasksView::List { selected: 0 })
        ));
    }

    #[test]
    fn tasks_without_a_manager_pushes_the_honest_notice() {
        let mut app = test_app(); // --mock shape: no manager
        app.run_command("tasks", "");
        assert!(app.tasks_view.is_none());
        assert!(app.messages.iter().any(|m| matches!(&m,
            Message::System(text) if text.contains("unavailable in this session"))));
    }

    #[test]
    fn submit_parses_command_arguments() {
        let (mut app, tasks) = app_with_tasks("args");
        let _ = tasks
            .spawn(hi_core::tasks::TaskSpec::process(
                "echo hi",
                std::env::temp_dir(),
            ))
            .unwrap();
        // The arg seam through the real submit path: "/tasks 1".
        app.input = "/tasks 1".into();
        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(
            app.tasks_view,
            Some(crate::app::TasksView::Output { id: 1 })
        ));
        assert!(app.input.is_empty());
    }

    #[test]
    fn finished_background_tasks_adopt_system_notes_exactly_once() {
        let mut app = test_app();
        let tasks = hi_core::tasks::TaskManager::new(std::env::temp_dir().join("hi-app-tasks"));
        app.tasks = Some(tasks.clone());
        let id = tasks
            .spawn(hi_core::tasks::TaskSpec::process(
                "echo note-me",
                std::env::temp_dir(),
            ))
            .unwrap();
        // Idle pump: no turn runs, so the note must land and persist
        // semantics belong to this path — keep pumping until the fact
        // shows up (no fixed sleeps).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            app.pump();
            let notes: Vec<&str> = app
                .messages
                .iter()
                .filter_map(|m| match m {
                    Message::System(text) => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            if let Some(note) = notes.iter().find(|n| n.contains("note-me")) {
                assert!(note.contains(&format!("#{id}")), "{note}");
                assert!(note.contains("finished (exit 0)"), "{note}");
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "completion note never landed"
            );
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        // One completion, one note: further idle pumps add nothing.
        let count = app.messages.len();
        app.pump();
        app.pump();
        assert_eq!(app.messages.len(), count);
        assert_eq!(tasks.running_count(), 0);
    }

    #[test]
    fn dropping_the_last_task_handle_kills_the_process_group() {
        // The real subject of this test since #71: the drop is the
        // kill (the old `· N bg` comment rode along until AD-0025).
        let tasks = hi_core::tasks::TaskManager::new(std::env::temp_dir().join("hi-app-tasks-2"));
        let _id = tasks
            .spawn(hi_core::tasks::TaskSpec::process(
                "sleep 30",
                std::env::temp_dir(),
            ))
            .unwrap();
        assert_eq!(tasks.running_count(), 1);
        // Dropping the last handle kills the group — no leaked sleep.
        let pid = tasks.status(1).unwrap().pid;
        drop(tasks);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while unsafe { libc::kill(-(pid as i32), 0) == 0 } {
            assert!(
                std::time::Instant::now() < deadline,
                "task group survived the drop"
            );
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
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
    fn a_pending_jump_lands_on_the_next_occurrence_or_dies_at_none() {
        let mut app = app();
        type_string(&mut app, "abca");
        app.cursor = 0;
        // Ctrl+] arms the jump; `a` is the target. The first `a`
        // is behind the cursor, so the landing is the second.
        app.handle_key(ctrl(KeyCode::Char(']')));
        app.handle_key(key(KeyCode::Char('a')));
        assert_eq!(app.cursor, 3);
        // Standing on it, the same pair finds none and moves
        // nothing.
        app.handle_key(ctrl(KeyCode::Char(']')));
        app.handle_key(key(KeyCode::Char('a')));
        assert_eq!(app.cursor, 3);
        // Ctrl+Alt+] jumps back — and the character immediately
        // behind the cursor counts.
        app.handle_key(ctrl_alt(KeyCode::Char(']')));
        app.handle_key(key(KeyCode::Char('a')));
        assert_eq!(app.cursor, 0);
    }

    #[test]
    fn a_pending_jump_cancels_on_enter_without_submitting_the_draft() {
        let mut app = app();
        type_string(&mut app, "draft");
        app.cursor = 0;
        app.handle_key(ctrl(KeyCode::Char(']')));
        // Enter is the cancel, not a submission: the draft stays.
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.input, "draft");
        assert!(!app.quit);
        assert!(app.messages.is_empty(), "nothing was sent");
        // The pending mode is gone, so plain typing types again.
        app.handle_key(key(KeyCode::Char('x')));
        assert_eq!(app.input, "xdraft");
    }

    #[test]
    fn a_pending_jump_cancels_on_the_chord_itself_and_lets_other_keys_through() {
        let mut app = app();
        type_string(&mut app, "ab");
        app.cursor = 0;
        // Re-pressing the chord cancels rather than re-arming (pi's
        // rule), so the next `a` types instead of jumping.
        app.handle_key(ctrl(KeyCode::Char(']')));
        app.handle_key(ctrl(KeyCode::Char(']')));
        app.handle_key(key(KeyCode::Char('a')));
        assert_eq!(app.input, "aab");
        // Any other chord acts normally, pending consumed: a motion
        // after an armed jump still moves the cursor.
        app.handle_key(ctrl(KeyCode::Char(']')));
        app.handle_key(key(KeyCode::Right));
        assert_eq!(app.cursor, 2, "the chord fell through to the motion");
        // And a target carrying control is not a target: the pending
        // mode is consumed and the motion chord still ran.
        app.handle_key(ctrl(KeyCode::Char(']')));
        app.handle_key(ctrl(KeyCode::Char('a')));
        assert_eq!(app.cursor, 0, "ctrl+a still ran, pending consumed");
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
        // The /context panel: every row visible, plus the footer.
        app.picker = None;
        app.run_command("context", "");
        assert_eq!(app.menu_height(), app.context_panel_rows().len() as u16 + 1);
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
    fn ctrl_z_reverts_in_order_coalesces_runs_and_alt_z_reapplies() {
        let mut app = test_app();
        // "ab" is one run, backspace its own step, "c" starts fresh.
        app.handle_key(key(KeyCode::Char('a')));
        app.handle_key(key(KeyCode::Char('b')));
        app.handle_key(key(KeyCode::Backspace));
        app.handle_key(key(KeyCode::Char('c')));
        assert_eq!(app.input, "ac");

        app.handle_key(ctrl(KeyCode::Char('z')));
        assert_eq!(app.input, "a");
        app.handle_key(ctrl(KeyCode::Char('z')));
        assert_eq!(app.input, "ab");
        app.handle_key(ctrl(KeyCode::Char('z')));
        // The whole typed run reverts as one unit.
        assert_eq!(app.input, "");

        // Alt+Z walks the redo stack back up.
        app.handle_key(alt(KeyCode::Char('z')));
        assert_eq!(app.input, "ab");
        app.handle_key(alt(KeyCode::Char('z')));
        assert_eq!(app.input, "a");
        app.handle_key(alt(KeyCode::Char('z')));
        assert_eq!(app.input, "ac");

        // A fresh edit clears the redo future.
        app.handle_key(key(KeyCode::Char('x')));
        assert_eq!(app.input, "acx");
        app.handle_key(ctrl(KeyCode::Char('z')));
        assert_eq!(app.input, "ac");
    }

    #[test]
    fn paste_is_its_own_undo_unit() {
        let mut app = test_app();
        app.handle_key(key(KeyCode::Char('h')));
        app.handle_key(key(KeyCode::Char('i')));
        app.handle_clipboard_outcome(crate::clipboard::ClipboardOutcome::Text("world".into()));
        assert_eq!(app.input, "hi world");
        app.handle_key(ctrl(KeyCode::Char('z')));
        assert_eq!(app.input, "hi");
        app.handle_key(key(KeyCode::Char('z')));
        app.handle_key(key(KeyCode::Char('a')));
        app.handle_key(key(KeyCode::Char('r')));
        assert_eq!(app.input, "hizar");
    }

    #[test]
    fn kills_feed_the_ring_and_ctrl_y_yanks_the_head() {
        let mut app = test_app();
        for c in "hello world".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        // Consecutive kills accumulate as one entry, in reading order.
        app.handle_key(ctrl(KeyCode::Char('w')));
        assert_eq!(app.input, "hello ");
        app.handle_key(ctrl(KeyCode::Char('w')));
        assert_eq!(app.input, "");
        assert_eq!(app.kill_ring, ["hello world"]);
        app.handle_key(ctrl(KeyCode::Char('y')));
        assert_eq!(app.input, "hello world");
        // The yank is an undo unit of its own.
        app.handle_key(ctrl(KeyCode::Char('z')));
        assert_eq!(app.input, "");
    }

    #[test]
    fn alt_y_cycles_back_through_earlier_kills() {
        let mut app = test_app();
        for c in "alpha".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(ctrl(KeyCode::Char('u')));
        for c in "beta".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(ctrl(KeyCode::Char('u')));
        // Typing between the kills kept them separate entries.
        assert_eq!(app.kill_ring, ["alpha", "beta"]);
        app.handle_key(ctrl(KeyCode::Char('y')));
        assert_eq!(app.input, "beta");
        // Alt+Y swaps in the previous kill; cycling past the oldest
        // wraps to the newest.
        app.handle_key(alt(KeyCode::Char('y')));
        assert_eq!(app.input, "alpha");
        app.handle_key(alt(KeyCode::Char('y')));
        assert_eq!(app.input, "beta");
    }

    #[test]
    fn yank_and_yank_pop_stay_silent_when_there_is_nothing_to_do() {
        let mut app = test_app();
        app.handle_key(ctrl(KeyCode::Char('y')));
        assert_eq!(app.input, "");
        for c in "one".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(ctrl(KeyCode::Char('u')));
        // Alt+Y with no yank in flight does nothing.
        app.handle_key(alt(KeyCode::Char('y')));
        assert_eq!(app.input, "");
        // A lone ring entry cycles to itself.
        app.handle_key(ctrl(KeyCode::Char('y')));
        app.handle_key(alt(KeyCode::Char('y')));
        assert_eq!(app.input, "one");
    }

    #[test]
    fn the_kill_ring_is_bounded() {
        let mut app = test_app();
        for c in 'a'..='q' {
            app.handle_key(key(KeyCode::Char(c)));
            app.handle_key(ctrl(KeyCode::Char('u')));
        }
        assert_eq!(app.kill_ring.len(), 16);
        // The newest kill survived; the oldest fell off the far end.
        app.handle_key(ctrl(KeyCode::Char('y')));
        assert_eq!(app.input, "q");
    }

    #[test]
    fn a_cleared_draft_comes_back_from_undo() {
        // The line-kill is a composer mutation like any other (AD-0030
        // meets #65): typed text after the clear steps back through
        // undo, and one more Ctrl-Z restores the draft.
        let mut app = test_app();
        app.handle_key(key(KeyCode::Char('d')));
        app.handle_key(key(KeyCode::Char('r')));
        app.handle_key(key(KeyCode::Char('a')));
        app.handle_key(key(KeyCode::Char('f')));
        app.handle_key(ctrl(KeyCode::Char('c')));
        assert_eq!(app.input, "");
        app.handle_key(key(KeyCode::Char('x')));
        app.handle_key(ctrl(KeyCode::Char('z')));
        assert_eq!(app.input, "");
        app.handle_key(ctrl(KeyCode::Char('z')));
        assert_eq!(app.input, "draf");
    }

    #[test]
    fn the_undo_ceiling_holds_at_100_units() {
        let mut app = test_app();
        // Insert + cursor-left per loop: every insert is its own unit.
        for _ in 0..120 {
            app.handle_key(key(KeyCode::Char('x')));
            app.handle_key(key(KeyCode::Left));
        }
        assert_eq!(app.undo.len(), 100);
        for _ in 0..100 {
            app.handle_key(ctrl(KeyCode::Char('z')));
        }
        assert!(app.undo.is_empty());
        // The 101st is a silent noop.
        app.handle_key(ctrl(KeyCode::Char('z')));
    }

    #[test]
    fn ctrl_c_clears_a_draft_and_quits_an_empty_composer() {
        // The line-killer rule: a draft dies first, the app dies only
        // when there is nothing left to kill.
        let mut app = test_app();
        app.input = "precious draft".into();
        app.cursor = 8;
        app.handle_key(ctrl(KeyCode::Char('c')));
        assert_eq!(app.input, "");
        assert_eq!(app.cursor, 0);
        assert!(!app.quit);

        app.handle_key(ctrl(KeyCode::Char('c')));
        assert!(app.quit);
    }

    #[test]
    fn name_names_the_session_and_the_file_records_it() {
        let root = std::env::temp_dir().join(format!("hi-name-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut session = hi_core::Session::start_in(
            &root,
            std::path::Path::new("/p"),
            "mock",
            "mock",
            None,
            None,
        )
        .unwrap();
        session.materialize().unwrap();
        let path = session.path().to_path_buf();
        drop(session);

        let mut app = test_app();
        app.switch_session(&path);
        app.run_command("name", "the scroll fix");
        let loaded = hi_core::session::load(&path).unwrap();
        assert_eq!(loaded.header.name.as_deref(), Some("the scroll fix"));
        assert!(
            app.messages
                .iter()
                .any(|m| matches!(m, Message::System(text) if text.contains("the scroll fix")))
        );

        // Renaming again wins; the words ride the transcript note.
        app.run_command("name", "the real fix");
        let loaded = hi_core::session::load(&path).unwrap();
        assert_eq!(loaded.header.name.as_deref(), Some("the real fix"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn bare_name_shows_the_current_name() {
        let root = std::env::temp_dir().join(format!("hi-bare-name-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut session = hi_core::Session::start_in(
            &root,
            std::path::Path::new("/p"),
            "mock",
            "mock",
            None,
            None,
        )
        .unwrap();
        session.materialize().unwrap();
        let path = session.path().to_path_buf();

        let mut app = test_app();
        app.switch_session(&path);
        app.run_command("name", "");
        assert!(
            app.messages
                .iter()
                .any(|m| matches!(m, Message::System(text) if text.contains("no name")))
        );
        app.run_command("name", "titled");
        app.messages.clear();
        app.run_command("name", "");
        assert!(
            app.messages.iter().any(
                |m| matches!(m, Message::System(text) if text.contains("session name: titled"))
            )
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn name_without_a_session_says_so() {
        let mut app = test_app();
        app.run_command("name", "wishes");
        assert!(
            app.messages
                .iter()
                .any(|m| matches!(m, Message::System(text) if text.contains("not persisting")))
        );
    }

    #[test]
    fn ctrl_r_renames_the_highlighted_session() {
        let mut app = app();
        app.session_root = Some(picker_root("rename", "33333333-aaaa", None, "hello"));
        app.run_command("resume", "");
        app.handle_key(ctrl(KeyCode::Char('r')));
        let picker = app.session_picker.as_ref().expect("renaming open");
        assert!(picker.renaming.is_some());

        for c in "named!".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Enter));
        // The record rides the file; the row updates in place; the
        // picker stays open for the next decision.
        let path = app
            .session_picker
            .as_ref()
            .expect("picker still open")
            .summaries[0]
            .path
            .clone();
        let loaded = hi_core::session::load(&path).unwrap();
        assert_eq!(loaded.header.name.as_deref(), Some("named!"));
        assert_eq!(
            app.session_picker.as_ref().unwrap().summaries[0]
                .header
                .name
                .as_deref(),
            Some("named!")
        );
        assert!(app.session_picker.as_ref().unwrap().renaming.is_none());
    }

    #[test]
    fn esc_cancels_the_picker_rename() {
        let mut app = app();
        app.session_root = Some(picker_root("renameesc", "44444444-aaaa", None, "hello"));
        app.run_command("resume", "");
        app.handle_key(ctrl(KeyCode::Char('r')));
        app.handle_key(key(KeyCode::Char('x')));
        app.handle_key(key(KeyCode::Esc));
        let picker = app.session_picker.as_ref().expect("picker stays open");
        assert!(picker.renaming.is_none());
        let loaded = hi_core::session::load(&picker.summaries[0].path).unwrap();
        assert_eq!(loaded.header.name, None);
    }

    #[test]
    fn resume_restores_the_recorded_thinking_level() {
        // AD-0045 ruling 1: the session's recorded level outranks the
        // settings default — resume puts you back where the session
        // was, exactly.
        let root = std::env::temp_dir().join(format!("hi-think-resume-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let cwd = std::env::current_dir().unwrap();
        let mut session =
            hi_core::Session::start_in(&root, &cwd, "mock", "mock", Some("high"), None).unwrap();
        session
            .append(&[Message::User("recorded at high".into())])
            .unwrap();
        let mut app = test_app();
        app.session_root = Some(root);
        app.thinking = Some(hi_core::thinking::ThinkingLevel::Low);
        app.switch_session(session.path());
        assert_eq!(app.thinking, Some(hi_core::thinking::ThinkingLevel::High));
    }

    #[test]
    fn resume_without_a_recorded_level_keeps_the_current_one() {
        // No record (old files, auto) = no override: the settings
        // value stands, auto is not forced.
        let root = std::env::temp_dir().join(format!("hi-think-resume2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let cwd = std::env::current_dir().unwrap();
        let mut session =
            hi_core::Session::start_in(&root, &cwd, "mock", "mock", None, None).unwrap();
        session.materialize().unwrap();
        let mut app = test_app();
        app.session_root = Some(root);
        app.thinking = Some(hi_core::thinking::ThinkingLevel::Medium);
        app.switch_session(session.path());
        assert_eq!(app.thinking, Some(hi_core::thinking::ThinkingLevel::Medium));
    }

    #[test]
    fn thinking_switch_restamps_the_live_session_header() {
        // AD-0045 ruling 2: the live header follows the switch, so
        // the next resume of this session restores the last level.
        let root = std::env::temp_dir().join(format!("hi-think-restamp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut app = test_app();
        app.session_root = Some(root);
        app.run_command("new", "");
        app.session
            .as_mut()
            .unwrap()
            .materialize()
            .expect("materialize");
        app.run_command("thinking", "high");
        let path = app.session.as_ref().unwrap().path().to_path_buf();
        let loaded = hi_core::session::load(&path).unwrap();
        assert_eq!(loaded.header.thinking.as_deref(), Some("high"));
        app.run_command("thinking", "auto");
        let loaded = hi_core::session::load(&path).unwrap();
        assert_eq!(loaded.header.thinking, None);
    }

    #[test]
    fn resume_restores_the_recorded_model() {
        // AD-0046: the header's provider/model outrank the startup
        // selection when they are still available.
        let root = std::env::temp_dir().join(format!("hi-model-resume-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let cwd = std::env::current_dir().unwrap();
        let mut session =
            hi_core::Session::start_in(&root, &cwd, "mock", "mock-2", None, None).unwrap();
        session
            .append(&[Message::User("recorded on mock-2".into())])
            .unwrap();
        let mut app = test_app();
        // Make mock-2 selectable: a config entry plus the catalog row.
        let mut config = hi_core::config::ModelsConfig::default();
        let provider = hi_core::config::ProviderConfig {
            base_url: "http://localhost:1/v1".into(),
            api_key: None,
            models: vec![hi_core::config::ModelConfig {
                id: "mock-2".into(),
                name: None,
                reasoning: true,
                vision: true,
                context_window: None,
                max_tokens: None,
                thinking_level_map: None,
            }],
        };
        config.providers.insert("mock".into(), provider);
        app.config = config;
        app.switch_session(session.path());
        assert_eq!(app.status.provider, "mock");
        assert_eq!(app.status.model, "mock-2");
    }

    #[test]
    fn resume_falls_back_with_a_warning_when_the_recorded_model_is_gone() {
        let root = std::env::temp_dir().join(format!("hi-model-gone-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let cwd = std::env::current_dir().unwrap();
        let mut session =
            hi_core::Session::start_in(&root, &cwd, "mock", "ghost-model", None, None).unwrap();
        session.materialize().unwrap();
        let mut app = test_app();
        app.session_root = Some(root);
        app.switch_session(session.path());
        // The selection stands and the user is told why.
        assert_eq!(app.status.model, "mock");
        assert!(
            app.messages
                .iter()
                .any(|m| matches!(m, Message::System(t) if t.contains("ghost-model") && t.contains("not available"))),
            "{:?}",
            app.messages
        );
    }

    #[test]
    fn model_switch_restamps_the_live_session_identity() {
        // AD-0046: a mid-session /model switch rewrites the header's
        // provider/model, so the next resume restores the model the
        // session ended on.
        let root = std::env::temp_dir().join(format!("hi-model-restamp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut app = test_app();
        app.session_root = Some(root);
        app.run_command("new", "");
        app.session
            .as_mut()
            .unwrap()
            .materialize()
            .expect("materialize");
        app.apply_model_choice("mock/mock-2".into());
        let path = app.session.as_ref().unwrap().path().to_path_buf();
        let loaded = hi_core::session::load(&path).unwrap();
        assert_eq!(loaded.header.provider, "mock");
        assert_eq!(loaded.header.model, "mock-2");
    }

    #[test]
    fn resume_opens_the_picker_and_esc_closes_it() {
        let mut app = app();
        app.session_root = Some(picker_root("open", "11111111-aaaa", Some("work"), "hello"));
        app.run_command("resume", "");
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
        app.run_command("resume", "");
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
            blocks: Vec::new(),
            error: None,
            started_at: std::time::Instant::now(),
            rx,
        });
        app.run_command("resume", "");
        assert!(app.session_picker.is_none());
        assert!(
            matches!(app.messages.last(), Some(Message::System(note)) if note.contains("esc first"))
        );
    }

    fn alt(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::ALT)
    }

    fn ctrl_alt(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL | KeyModifiers::ALT)
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

    fn shift(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::SHIFT)
    }

    #[test]
    fn newline_chords_insert_newlines() {
        // Shift+Enter — what the kitty encoding (`CSI 13;2u`) decodes
        // to, no handshake gating needed.
        let mut app = app();
        for c in "a".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(shift(KeyCode::Enter));
        assert_eq!(app.input, "a\n");
        assert_eq!(app.cursor, 2);
        app.handle_key(key(KeyCode::Char('b')));
        assert_eq!(app.input, "a\nb");
        // Ctrl+J — the chord that needs no kitty protocol at all.
        app.handle_key(ctrl(KeyCode::Char('j')));
        assert_eq!(app.input, "a\nb\n");
        // A literal LF (a terminal binding shift+enter to text:\n).
        app.handle_key(key(KeyCode::Char('\n')));
        assert_eq!(app.input, "a\nb\n\n");
        // A newline leaves command space: no menu for "/x\n...".
        app.input.clear();
        app.cursor = 0;
        for c in "/e".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        assert!(app.completion.is_some());
        app.handle_key(shift(KeyCode::Enter));
        assert_eq!(app.input, "/e\n");
        assert!(app.completion.is_none());
    }

    #[test]
    fn enter_submits_a_multiline_draft_whole_and_backslash_escapes() {
        let mut app = app();
        for c in "two\nlines".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Enter));
        assert!(app.input.is_empty(), "Enter submits even multi-line");
        assert_eq!(app.messages.len(), 1);
        let crate::Message::User(sent) = &app.messages[0] else {
            panic!("expected a user message, got {:?}", app.messages[0]);
        };
        assert_eq!(sent, "two\nlines");

        // The escape hatch: backslash right before Enter yields the
        // newline instead of the submission.
        for c in "one\\".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.input, "one\n");
        assert_eq!(app.messages.len(), 1, "no submission happened");
    }

    #[test]
    fn arrows_walk_visual_rows_in_a_multiline_input() {
        let mut app = app();
        for c in "alpha\nbeta".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(app.cursor, 10);
        // Up from the last row: column preserved (after "beta" -> after "alph").
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.cursor, 4);
        // Up at the first row's interior clamps to the row start (pi).
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.cursor, 0);
        // Up at column 0 of the first row: history fallthrough — the
        // draft parks, history is empty here, nothing moves.
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.cursor, 0);
        // Down walks back out: start of "beta", then the row end clamp.
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.cursor, 6);
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.cursor, 10);
        // Down on the last row without history browsing: row end clamp.
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.cursor, 10);
    }

    #[test]
    fn line_scoped_chords_match_the_multiline_contract() {
        let mut app = app();
        for c in "one\ntwo".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        // Cursor at the end (7): Home/Ctrl+A to the line's start.
        app.handle_key(key(KeyCode::Home));
        assert_eq!(app.cursor, 4);
        app.handle_key(ctrl(KeyCode::Char('a')));
        assert_eq!(app.cursor, 4);
        // End/Ctrl+E to the line's end (the whole input's end here).
        app.handle_key(key(KeyCode::End));
        assert_eq!(app.cursor, 7);
        // Ctrl+U kills the current line only.
        app.handle_key(ctrl(KeyCode::Char('u')));
        assert_eq!(app.input, "one\n");
        assert_eq!(app.cursor, 4);
        // Ctrl+K kills to the line's end only.
        app.cursor = 0;
        app.handle_key(ctrl(KeyCode::Char('k')));
        assert_eq!(app.input, "\n");
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
        app.run_command("tools", "");
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
