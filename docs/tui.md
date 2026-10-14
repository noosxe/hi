# `hi-tui` reference

Files this doc describes: `crates/tui/src/lib.rs`, `app.rs`, `commands.rs`,
`catalog.rs`, `status.rs`, `ui.rs`, `markdown.rs`, `theme.rs`, `picker.rs`,
`wrap.rs`.

Module map: `lib.rs` (terminal lifecycle + event loop), `app.rs` (all state +
input handling), `commands.rs` (slash registry), `catalog.rs` (picker menu
data), `status.rs` (status-bar facts), `ui.rs` (widgets/drawing),
`markdown.rs` (markdown → themed rows), `theme.rs` (role-indirected palette),
`picker.rs` (session picker), `wrap.rs` (text wrapping). Public exports:
`run`, `pick_session`, `Catalog`, `Status`.

## lib.rs — terminal lifecycle and the frame clock

`run(provider, status, catalog, config, system_prompt, thinking) -> io::Result<()>`:

- **Inline viewport, no alternate screen**: `Viewport::Inline(rows at
  startup)` — the live region is the whole screen (the inline height is
  frozen at construction, so a mid-run resize keeps the startup height,
  clamped). The scrollback above is the flushed prefix of the transcript
  (AD-0016, `scroll.rs`); the viewport shows its live tail down to the
  input. Native scroll/search/copy for free; same approach as pi's
  "regular mode".
- `ratatui::try_init_with_options` enables raw mode and installs a panic hook
  that restores the terminal; the welcome line seeds the transcript window
  (`Scroll::new`) and flushes into scrollback as the conversation grows.
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
`turn: Option<Turn>`, `settings_board: Option<SettingsBoard>` (the
`/settings` board — AD-0023), `tasks: Option<hi_core::tasks::TaskManager>` (drained
per frame in `pump` — completions become transcript notes; the running
count feeds the status row), `tasks_view: Option<TasksView>` (the `/tasks`
pane: list, or one task's output watched live — AD-0021),
`task_turns: bool` plus the auto-trigger budget and arm (AD-0022: an
opted-in idle session may start a turn on a completion).

`Turn { text, error: Option<String>, started_at: Instant, rx: Receiver<Event>
}` — `rx` is private on purpose: raw events are consumed only by `pump` and
never leak into UI code.

Behaviors (each pinned by a test in `app.rs`):

- Enter with an open completion menu executes the highlighted command;
  plain Enter sends.
- `/` opens the completion menu; typing filters case-insensitively by
  prefix; Tab completes without executing; Esc dismisses the menu and keeps
  the text. The menu renders pi's SelectList look (grounded on
  `select-list.js`): no box, plain rows under the input, `→ ` on the
  selected row, the command in a primary column sized to the widest match
  (clamped 12–32) with the description muted in a second column, the whole
  selected row in `Fg::Accent`. The visible window is five rows, centered
  on the selection, with a muted `  (n/m)` row when the matches overflow —
  and Up/Down wrap. Descriptions only render when the terminal is wider
  than 40 columns with room left, pi's exact rule. The SelectList is the
  one renderer for every menu (`ui::select_list_rows`): the `/model`
  picker rides the same rows with a muted key-hint footer, the `/resume`
  chooser marks its rows the same way with a count row on overflow, and
  both pickers wrap their navigation.
  the typed text; unknown commands report instead of sending.
- Enter mid-turn queues; Esc mid-turn cancels **and** restores the queue
  into the input box.
- `activity_label()` builds the input's top-rule title while busy:
  spinner frame + elapsed seconds + `· N queued` + `esc to cancel`. New
  busy states (compaction, tool runs) become arms of this method.

## commands.rs — slash registry

```rust
pub struct Command { pub name: &'static str, pub description: &'static str }
pub const COMMANDS: &[Command]  // sorted: exit, model, name, quit, resume, settings, tasks, tools
pub fn matching(typed: &str) -> Vec<&'static Command>   // case-insensitive prefix
pub fn find_exact(name: &str) -> Option<&'static Command>
```

Adding a command = one entry in `COMMANDS` **plus** one arm in
`App::run_command`. The completion menu inherits both order and descriptions.
`/exit` and `/quit` (alias) stop the session; `/model` opens the picker;
`/name <title>` names the session — the record appends, last-wins on
replay, and the picker row leads with it (see
[picker.rs](#pickerrs--session-selection)); bare `/name` shows the current
name;
`/resume` opens the session picker (see [picker.rs](#pickerrs--session-selection));
`/settings` opens the policy board (AD-0023) — a curated list of what
the running session can flip (`task turns`, `images`), each row shown
with its live `on|off` state. Enter toggles and **keeps the board
open** (a board, not a chooser — the flip is visible and more rows can
follow); Ctrl-S persists the highlighted row through the #45
load-modify-save shape (`Settings::persist_task_turns` /
`persist_images`); Esc closes. Startup-stamped settings
(`defaultTools`, `telemetry`) stay off the board until they declare a
setter path. `App.settings_board: Option<SettingsBoard>` owns the
keyboard while open; the row table is `SETTINGS_TOGGLES`, and draw
reads `App::settings_rows()` live each frame.
`/tools` pushes a dim local-only notice listing the registry — name,
annotation badges (`read-only` / `destructive`, only when the tool claims
them), and the first sentence of each description (`ToolRegistry::info()`
in core snapshots the facts; the TUI formats). `/tasks` opens the tasks
pane (AD-0021) — bare to list, `/tasks <id>` for one task's live output.
`/new` starts a fresh session for the directory hi runs in —
optionally named (`/new scratch`) — and walks the same reset resume
walks, so transcript, bill, and ring all begin again while the model
and thinking level carry (#98). `/keybinds` pushes the same kind of notice listing every registered
chord from `keybinds::KEYBINDS` — chord and one-line description,
grouped movement / kills / yank / undo / session-level (#95). The
table cannot drift from the bindings: a test scans `app.rs` for
modified-chord arms and fails when a bound chord has no row or a row
names an unbound chord. On macOS the chord column renders the
menu-bar icons — `⌃ S`, `⌥ D`, `⌫`/`⌦` for the named editing keys —
through `icon_label(chord, mac)`; every other platform keeps the
table's words. The mapping is pure, so tests pin both platforms on
any host.
Like unknown-command reports, notices persist in the session file but
never reach the provider. Commands may carry arguments: the dispatch
splits on the first space, name first, the rest rides to the handler.

## catalog.rs — what the `/model` picker offers

`Catalog { entries: Vec<(String, Vec<String>)> }` — provider name → model
ids in config order. `items()` flattens to `provider/model` strings. Built
once at startup in `crates/cli`: each provider is bounded by the same 10s
discovery timeout as `--list-models`; providers that fail or list nothing
simply don't appear (their errors are visible via `--list-models`).

Picker keys: arrows move, Enter switches the session's model (safe mid-turn;
running turn finishes on its original provider), **Ctrl-S saves** the choice
to `settings.json` as `defaultModel` — load-modify-save: your other settings
(thinking, tools, telemetry, images) survive untouched; a file that won't
parse is reported, not overwritten — Esc closes.

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
- **The status board (AD-0025, amended; usage AD-0026)**:
  `App::status_rows()` returns rows bottom-up, two permanent
  residents. Row 1 — the board's top — holds the session's
  **usage line** in the top-left corner:
  `↑8.6M ↓1.9M R250M CH99.2% 10.7%/1M` from `status::usage_line`:
  cumulative output / input-net-of-cache / reasoning / cache-hit
  share, plus the *latest* round's prompt against the model's
  configured `contextWindow` (absent ⇒ no share; every segment hides
  when its input is absent; empty until the first bill — recorded in
  the session file, so a resume restores the totals (#85)).
  The model (+thinking) owns row 1's right — the indicator you read
  all day — with the auto-trigger arm appended (`auto: armed` —
  #72's visible feedback; transient noise rides right). Row 0 is
  location on the left and the background tasks' counts on the right
  (`tasks: 2 running · 1 done`) — under the model they run against.
  `App::status_height()` is the one height `layout` and the flush
  pass both measure with, so draw and flush can never disagree; a
  menu still outranks the whole board (the ruled yield, generalized
  from row to board). Three rows is the planned ceiling; #4/#6
  register here later.
- Thinking display: when a level is set, `draw_status` appends
  `· thinking:<level>` to the right label (e.g.
  `(zai) glm-4.6 · thinking:high`); `Status::label()` itself stays
  provider/model only.
- Background tasks (#5, AD-0019): while tasks exist, the status
  board's earned row shows `tasks: N running · M done` (AD-0025); on
  completion the app's frame pump folds a dim
  `Message::System` note into the transcript (`background task #3
  'cargo test' finished (exit 0) after 54s`) and the fact rides the
  next turn's payload to the model.

## ui.rs — drawing (the only file allowed to draw)

The `/tasks` pane draws in the menu slot like every menu, and reads the
manager fresh each frame — the frame clock is the poll, nothing caches
(AD-0021).

`draw(frame, app)` lays out the live viewport — **the whole terminal**
(AD-0015; the first sketch fixed it at 12 rows, which capped every reply
at ~8 lines on any screen), bottom-up via the pure `layout()` helper:

```
50 rows, resting:  [stream 44] [input 4] [status 2]
50 rows, menu:     [stream 41] [menu 4]  [input 4] [status 1]
(status grows upward when rows are earned — AD-0025 — and yields
entirely to an open menu)
```

- Status row pinned at the bottom; input grows upward with its wrapped text
  (`wrapped_row_count` clamped 1–6, +2 rule rows, +1 breathing row above the
  box — AD-0020, the input's own margin: owned and cleared each frame, so
  scrolled content never lingers in it); the menu (completion or
  picker, whichever is open, natural height from `App::menu_height()`) takes
  rows above the input; the streaming area keeps every row that remains —
  a reply fills the window, pi's live viewport is likewise the full
  terminal. Scrollback above the viewport is the transcript's flushed
  prefix; the stream area is its window — the conversation flows down
  to the input, never a blank band (AD-0016). Saturating arithmetic: on
  absurdly short terminals the stream shrinks first, to nothing;
  nothing panics.
- `draw_input`: top/bottom rules only, no side walls (pi-style); the real
  terminal cursor parks **at the logical cursor** (`wrap::cursor_pos` maps the
  byte offset into wrapped row/column — mid-text while editing, AD-0014);
  while busy, the top rule carries the right-aligned activity label in yellow.
- Transcript is NOT drawn here — finalized messages go to scrollback via
  `block_rows` + `rows_into` (pi-style role blocks, no text labels; see the
  markdown.rs and theme.rs sections below). The insert height is the
  wrapped row count — never one row per message. Spacing is each block's
  own padding, not a separator: every block carries one blank row above
  and below its content (pi's `paddingY`) and one blank column on its
  left and right (paddingX — content wraps at width−2; the fill of a
  filled block still runs the full width behind it), so two blank rows
  separate any
  adjacent content rows — tinted with the fill for blocks that have a
  background (user, tool), plain for blocks that don't (assistant, system,
  error). While a tool runs, the
  streaming area shows its **live pending block** (pending fill); when the
  result lands, one merged block (bold tool name + collapsed args over the
  output preview) prints to scrollback with a success- or error-tinted
  fill. Blocks print once, at final state, and are then terminal-owned —
  frozen colors, frozen width on resize, exactly like pi's inline mode.
- `draw_stream` shows the in-flight `turn.text` as **markdown, undimmed**
  (pi parity; it re-wraps per frame), or a turn error in the error
  color. The live reply carries the **same block padding and wrap
  width its flushed self will get** (AD-0024: live == flushed) — the
  completion is visually a no-op, not a reflow jump.
- Other pieces: `draw_picker`, `draw_completion`, `draw_status`,
  `welcome_into`.

## kitty.rs — image rendering into scrollback (AD-0010)

On kitty-protocol terminals (kitty, Ghostty, WezTerm, Warp — pi's exact
env-sniffed matrix, no terminal queries; `HI_IMAGE_PROTOCOL=kitty|none`
overrides), `read`-attached images render as real graphics in the
transcript:

- **Emission model**: the block builder returns `Segment`s — text rows
  or `{ sequence, rows }` images. `lib.rs` computes the height across
  segments and `insert_before`s once; `segments_into` writes text via
  the ordinary Paragraph path and drops the whole kitty APC sequence
  into **one buffer cell** at the block's side padding. The `rows`
  cells below stay untouched — the image overlays them (z-order draws
  images over default-background cells), and because history prints
  exactly once, the terminal keeps it the way it keeps text. No image
  registry, no cropping, no delete-on-exit — the machinery pi needs
  for re-renderable viewports is structurally unnecessary here.
- **Encoder**: `a=T,f=100,q=2,C=1,c=<cols>,r=<rows>,i=<id>` — PNG data
  only (f=100; non-PNG re-encodes via `core::image_util::to_png`),
  base64 chunked at the protocol's 4096 bytes with `m=1`/`m=0` flags,
  only the first chunk carries control keys. `C=1` is mandatory: by
  default kitty moves the cursor after placing, and ratatui's backend
  skips cursor moves between contiguous cells — the next row's text
  would land where kitty left the cursor.
- **Fit**: pixel dims sniffed from headers (`core::image_util::
  dimensions`), scaled into a cell rectangle via the cell pixel size
  (crossterm's `window_size()` when the OS reports real pixels, pi's
  9×18 fallback), capped at 80 columns × 24 rows, aspect-preserved
  with pi's distortion chooser. Small images upscale to the budget,
  like pi.
- **Fallback**: an image that fails any step (undecodable base64,
  corrupt bytes) renders as its text note — `· image: <mime>, <size>`
  — the same note every terminal shows when no image protocol is
  detected. Images render as words, never as nothing.

## clipboard.rs — paste stages a reference (AD-0011)

Ctrl-V reads the system clipboard on a one-shot thread (arboard;
image first, then text) and reports through a channel `pump` drains —
the UI never blocks on a possibly-stalling X11 clipboard. An image is
encoded to PNG (via `core::image_util::from_rgba` — arboard only ever
hands back raw pixels) and staged at
`temp_dir()/hi-clipboard-*.png`; the **path is inserted into the input
as inert text**. Nothing sends until Enter, and the image reaches the
model only when `read` runs on the path — gates, budget, honest notes,
kitty rendering all inherited. A failed read is a transient note on
the composer's title rule, gone at the next keypress; it never enters
the transcript or the session. Text paste rides bracketed paste
(`Event::Paste` → verbatim insert, newlines normalized) — without the
mode enabled, pasted newlines would submit mid-paste. Command
dispatch disambiguation: a leading `/` is a command only while the
rest contains no `/`, so staged paths send as messages. The chord is
Ctrl+V everywhere, and **Cmd+V aliases it** where the kitty keyboard
protocol reports the chord (AD-0013: `DISAMBIGUATE_ESCAPE_CODES` is
pushed at startup — supporting terminals forward Cmd+V as
`Char('v') + SUPER`; the rest ignore the enable and keep their own
text paste).

## editor.rs — the composer's text-editing core (AD-0014)

Pure functions over the flat input `String` plus a byte-offset cursor
(always on a `char` boundary). Owned, not a crate: ratatui ships no
input widget, tui-textarea is a self-rendering multi-line widget whose
"history" is undo history, tui-input covers only this thin slice, and
the app-shaped parts — prompt history with a parked draft, chords
including SUPER under the kitty keyboard protocol — cannot come from a
crate at all. pi reached the same conclusion with its own editor.

Word unit everywhere: maximal runs of `is_alphanumeric() || '_'`,
other characters separators — so paths like `/tmp/hi-clipboard-x.png`
jump at `-`, `/`, `.`.

| Chord | Action |
| --- | --- |
| ← / → , Ctrl+B / F | move one character (clamped) |
| Home / End, Ctrl+A / E | start / end of the line (AD-0018) |
| Alt+B / F, Alt+← / →, Ctrl+← / → | word jump (separator-skip, then word-skip; the newline is a separator) |
| Ctrl+] , Ctrl+Alt+] | jump to the next / previous occurrence of a character (#67) — pi's two-step: the chord arms, the next printable key is the target; none moves nothing, Enter cancels rather than submit; needs the kitty protocol (legacy Ctrl+] *is* ESC) |
| Shift+Enter, Ctrl+J, literal LF | newline — the encoding is the capability (AD-0018) |
| Cmd+← / → (SUPER) | start / end — kitty-protocol reporting (AD-0013) |
| Backspace | remove the character before the cursor |
| Delete, Ctrl+D | remove the character at the cursor |
| Ctrl+C | clear the draft (cursor home); quit when it's already empty |
| Ctrl+Z / Alt+Z | undo / redo the last composer edit (#65) — insert runs coalesce into one step, kills and pastes are their own; 100-unit ceiling |
| Ctrl+Y / Alt+Y | yank the newest kill / cycle it to older kills (#64) — kills (Ctrl+K/U/W/D, Alt+D, Alt+Backspace/Delete) feed a 16-entry ring; consecutive kills merge into one entry |
| Ctrl+W, Alt+Backspace | remove the word before the cursor |
| Alt+D, Alt+Delete | remove the word after the cursor |
| Ctrl+U / Ctrl+K | remove to line start / line end (AD-0018) |
| ↑ / ↓ | prompt history (below) |

Alt+B / F are the readline word motions — what terminals that intercept
option+arrows actually send on the wire (Ghostty's macOS defaults decode
them to `ESC b` / `ESC f`); the Alt/Ctrl arrow chords stay bound for
terminals that report the chord itself.

**Multi-line input (AD-0018).** The newline chords insert at the cursor
and the composer grows a row per line (each line wraps on its own
budget; blank lines are visible; the six-text-row cap stands). Enter
still submits the whole draft — with pi's escape hatch: a backslash
right before Enter yields the newline instead. In a multi-line input
the arrows walk visual rows column-preserving and fall through to
history at the first/last row's edges; a single-line input keeps
history from anywhere. Cmd+← / → stay start/end of the whole input.

Prompt history: every non-empty Enter submission — commands included,
shell parity — lands in `App.history` (cap 100). ↑ from fresh input
parks the draft and loads the newest entry with the cursor at its
start; ↓ walks back down, and past the newest entry the draft returns
with the cursor at its end, as if you'd never left. Submitting a
loaded entry works as-is — that is the fix-up-a-cancelled-message
workflow. Typing mid-browse edits the loaded entry; there is no
kill-ring, undo, or Shift+Enter newline (named candidates for later).

The ring is a **derivation of the transcript** (#87): on resume, on
fork, and on in-app session switch it is re-derived from the file's
`Message::User` lines, oldest-first, the ceiling holding on seed
(drop-oldest). Nothing new is stored — #85's rule again: persist the
per-round facts, re-derive the totals on read. Commands never replay
(they ride the live ring only, never the transcript); drafts, browse
position, and the parked input stay session-local.

## markdown.rs — markdown → themed terminal rows

`markdown_rows(text, width, base_style) -> Vec<Line>` — parses with
`pulldown-cmark` (CommonMark plus pi's `marked` extension set: GFM tables,
strikethrough, tasklists; no math, by ruling) and renders with pi-tui's
conventions: h1 bold+underlined, h2 bold, h3+ keep their `### ` prefix
(headings color); fenced code restated with border-colored ``` lines and a
two-space indented body (no syntax highlighting — a recorded divergence);
`│ `-bordered blockquotes with italic bodies; `- `/`N. ` list markers with
`[x]`/`[ ] ` task checkboxes and four-space nesting; links as underlined
text plus ` (url)` when the text differs from the href (the no-OSC-8
fallback pi uses); `─` rules; tables drawn as `┌─┬─┐` box grids with bold
headers, per-column wrapping, and proportional shrink when natural widths
overflow (pi's algorithm, min word width capped at 30). A blank line
separates blocks inside one message, as pi does.

Inline content is collected as styled segments and wrapped by
`wrap_segments` — the styled sibling of `wrap.rs`, carrying styles across
line breaks. User and assistant messages render as markdown (ruling ② of
#33); system and error lines stay plain text (`System` dim, `Error` in the
error color — pi renders diagnostics as colored text, no labels).

## picker.rs — session selection

The session picker exists twice, over one composition. `picker_lines` builds
the rows — title, the visible rows around the selection, the filter line,
the key hint — and both consumers render exactly that. A row leads with
the words that recognize the session (#89): its **name**, else its **first
prompt line**, else the id prefix — and the id (the `--session` key)
demotes to the dim right edge, surviving any width (the words truncate
first, at a ten-column floor, pi's). `ctrl-r` on a highlighted row renames
it: the buffer takes over the prompt line, Enter appends the `name` record
to that file (empty buffer cancels — renames are only written for real
names), and the row updates in place. The record is the same append-only
seam as messages and usage.

- `pick_session(summaries)` — the standalone `--resume` chooser. It owns a
  short inline-viewport terminal before the main TUI boots (pi's
  startup-TUI pattern), restores the terminal, and returns the chosen file
  (`None` on Esc). The CLI owns everything around it: listing, what Esc
  means, the resumed-from note.
- `/resume` in-session — `app.session_picker` takes over the keyboard and
  the whole live area (status row stays), reusing `picker_lines`. Type
  filters by name/id/preview, Enter swaps the live session: the file
  reopens for appending, the transcript is replaced (same resumed-from
  note the startup path uses), and the print cursor resets so the event
  loop renders the new history below the old scrollback — inline mode's
  history is terminal-owned, so nothing is redrawn. Refused mid-turn:
  switching under a streaming reply would interleave two sessions'
  provider events.

Row helpers are pure and unit-tested: `age` (s/m/h/d buckets), `row_text`
(`(main, id)`: name ?? first prompt line ?? id, with the id `None` when it
already leads), `filter_matches` (case-insensitive over id, name, preview).

## theme.rs — the palette behind roles

`THEME.fg(Fg::…) -> Style` and `THEME.bg(Bg::…) -> Style` — renderers ask
for roles (`Accent`, `Heading`, `Link`, `ToolTitle`, `UserMessage`, `ToolPending`,
`ToolSuccess`, `ToolError`…), never colors, so palettes stay swappable.
Every value is pi's `dark.json` OKHSL token resolved to sRGB through pi's
own color math at authoring time; hi renders pi's exact tints. One built-in
dark palette for now: a light palette, a `theme` setting, and terminal
querying (`system`) are deliberate follow-ups, mirroring pi where `system`
is a separate theme.

## wrap.rs — text wrapping helpers

Display-width aware (counts terminal columns, not chars — CJK-safe), used by
input height estimation, plain system/error lines, and tool output bodies.
Oversized words (longer than the width) hard-split. Styled markdown wraps
through `markdown.rs`'s `wrap_segments`, which mirrors these semantics while
carrying styles across breaks. Tests pin the width semantics and the
hard-split behavior.

## scroll.rs — the flushed-prefix transcript window (AD-0016)

The scrollback above the viewport holds a prefix of the finalized
transcript; the live stream area shows the tail that follows it. Rows
move from tail to scrollback exactly once, in order — text segments
sub-flush row by row, image segments move atomically (a kitty image
cannot be split across the seam), and an image at the very end
over-flushes past the window target so it can never strand invisible.
The seam stays invisible because both sides are projections of the
same cached blocks: `block_segments` (scrollback, images real) and
`block_rows` (viewport, images dropped — the managed region re-renders
every frame and cannot host graphics). `App.scroll` carries the cache,
the fold pointer (the old `printed`), and the flush cursor; a session
switch refolds so the new history replays below the old conversation.
A resize re-wraps unflushed blocks and snaps the cursor to the partial
block's start — at most one block re-prints at the seam, once (pi's
behavior too). The window hugs the input: a short conversation sits
directly above the composer, not floating at the top of the screen.

## Extension-relevant seams (see decisions.md before touching)

- Status bar: `Status::label()` + `compose_status` are the current seam;
  extensions will contribute segments through a UI-state bridge (tui keeps
  drawing ownership; extension runtimes never draw).
- Busy states: `activity_label()` is the designated arm for new
  turn-adjacent activity display.
- Menu/picker: currently two arms (`picker` vs `completion`) in one layout
  slot — a pattern to follow for future modal surfaces.
