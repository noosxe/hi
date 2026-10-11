# `hi-tui` reference

Files this doc describes: `crates/tui/src/lib.rs`, `app.rs`, `commands.rs`,
`catalog.rs`, `status.rs`, `ui.rs`, `wrap.rs`.

Module map: `lib.rs` (terminal lifecycle + event loop), `app.rs` (all state +
input handling), `commands.rs` (slash registry), `catalog.rs` (picker menu
data), `status.rs` (status-bar facts), `ui.rs` (widgets/drawing), `wrap.rs`
(text wrapping). Public exports: `run`, `Catalog`, `Status`.

## lib.rs — terminal lifecycle and the frame clock

`run(provider, status, catalog, config, system_prompt, thinking) -> io::Result<()>`:

- **Inline viewport, no alternate screen**: `TerminalOptions { viewport:
  Viewport::Inline(12) }` — hi manages only the bottom 12 rows; the
  transcript is printed *above* the viewport and owned by the terminal's
  native scrollback from then on (native scroll/search/copy for free;
  same approach as pi's "regular mode").
- `ratatui::try_init_with_options` enables raw mode and installs a panic hook
  that restores the terminal; a welcome line is inserted before the viewport
  (`ui::welcome_into`).
- `event_loop` = the frame clock: wait up to **50ms** for a key, then draw
  and `pump` either way. Loop exits when `app.quit`.
- Teardown order: `terminal.clear()` (wipe managed region) →
  `ratatui::restore()` (raw mode off) → two newlines (park the shell prompt
  on a fresh row). Quitting mid-turn is safe: dropping `app` drops the
  turn's `Receiver`, which stops the provider thread (see
  [turn-lifecycle.md](turn-lifecycle.md)).

## app.rs — all state, one struct

Contract: event handling **mutates** `App` (`handle_key`, `pump`); rendering
only **reads** it. No widget, no terminal handle, no drawing in this file.

Key fields: `quit`, `input`, `messages: Vec<Message>` (finalized only),
`provider: Arc<dyn Provider>`, `status: Status`, `location: String`,
`catalog: Catalog`, `config: ModelsConfig` (needed to rebuild providers on
model switch), `picker: Option<Picker>` (open `/model` chooser; `Some` = owns
the keyboard), `tick: u8` (spinner frames), `completion: Option<Completion>`
(`/` menu; `Some` = consumes navigation keys), `queued: Option<String>`,
`system_prompt` + `thinking` (re-attached on provider rebuild),
`turn: Option<Turn>`.

`Turn { text, error: Option<String>, started_at: Instant, rx: Receiver<Event>
}` — `rx` is private on purpose: raw events are consumed only by `pump` and
never leak into UI code.

Behaviors (each pinned by a test in `app.rs`):

- Enter with an open completion menu executes the highlighted command;
  plain Enter sends.
- `/` opens the completion menu; typing filters case-insensitively by
  prefix; Tab completes without executing; Esc dismisses the menu and keeps
  the typed text; unknown commands report instead of sending.
- Enter mid-turn queues; Esc mid-turn cancels **and** restores the queue
  into the input box.
- `activity_label()` builds the input's top-rule title while busy:
  spinner frame + elapsed seconds + `· N queued` + `esc to cancel`. New
  busy states (compaction, tool runs) become arms of this method.

## commands.rs — slash registry

```rust
pub struct Command { pub name: &'static str, pub description: &'static str }
pub const COMMANDS: &[Command]  // sorted: exit, model, quit
pub fn matching(typed: &str) -> Vec<&'static Command>   // case-insensitive prefix
pub fn find_exact(name: &str) -> Option<&'static Command>
```

Adding a command = one entry in `COMMANDS` **plus** one arm in
`App::run_command`. The completion menu inherits both order and descriptions.
`/exit` and `/quit` (alias) stop the session; `/model` opens the picker.

## catalog.rs — what the `/model` picker offers

`Catalog { entries: Vec<(String, Vec<String>)> }` — provider name → model
ids in config order. `items()` flattens to `provider/model` strings. Built
once at startup in `crates/cli`: each provider is bounded by the same 10s
discovery timeout as `--list-models`; providers that fail or list nothing
simply don't appear (their errors are visible via `--list-models`).

Picker keys: arrows move, Enter switches the session's model (safe mid-turn;
running turn finishes on its original provider), **Ctrl-S saves** the choice
to `settings.json` as `defaultModel`, Esc closes.

## status.rs — status-bar facts (pure filesystem, no subprocesses)

- `Status { provider, model }` — right side; `label()` renders
  `(provider) model`. The CLI owns these names because the `dyn Provider`
  trait object can't name itself.
- `location_label()` — left side `~/path (branch)`; branch read from
  `.git/HEAD`, found by walking up from cwd (works for worktrees/submodules
  where `.git` is a *file* with `gitdir:`; detached HEAD → 7-char sha; not
  in a repo → no branch part). Computed once at startup; mid-session
  refresh is polish for later.
- `compose_status(left, right, width)` — one row, filler between, clips on
  narrow terminals, never panics.
- An effort (thinking level) indicator will join the right side later.

## ui.rs — drawing (the only file allowed to draw)

`draw(frame, app)` lays out the fixed 12-row viewport, bottom-up:

```
resting:     [stream 8] [input 3] [status 1]
menu open:   [stream 5] [menu 3]  [input 3] [status 1]
long input:  [stream 2] [input 8] [status 1]
```

- Status row pinned at the bottom; input grows upward with its wrapped text
  (`wrapped_row_count` clamped 1–6, +2 rule rows); the menu (completion or
  picker, whichever is open, height from `App::menu_height()`) takes rows
  above the input; the streaming area keeps the rest, never fewer than 2
  rows.
- `draw_input`: top/bottom rules only, no side walls (pi-style); the real
  terminal cursor parks right after the typed text; while busy, the top rule
  carries the right-aligned activity label in yellow.
- Transcript is NOT drawn here — finalized messages go to scrollback via
  `message_into`; the live area shows the in-flight `turn.text` dim.
- Other pieces: `draw_picker`, `draw_completion`, `draw_status`,
  `welcome_into`.

## wrap.rs — text wrapping helpers

Display-width aware (counts terminal columns, not chars — CJK-safe), used by
both input height estimation and transcript printing. Oversized words
(longer than the width) hard-split. Tests pin the width semantics and the
hard-split behavior.

## Extension-relevant seams (see decisions.md before touching)

- Status bar: `Status::label()` + `compose_status` are the current seam;
  extensions will contribute segments through a UI-state bridge (tui keeps
  drawing ownership; extension runtimes never draw).
- Busy states: `activity_label()` is the designated arm for new
  turn-adjacent activity display.
- Menu/picker: currently two arms (`picker` vs `completion`) in one layout
  slot — a pattern to follow for future modal surfaces.
