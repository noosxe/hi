# Architecture

Files this doc describes: `Cargo.toml` (workspace), `crates/*/Cargo.toml`,
`crates/core/src/lib.rs`, `crates/cli/src/main.rs`, `crates/tui/src/lib.rs`,
`flake.nix`.

## What `hi` is

A minimalist agent harness: hold a streaming conversation with a language
model from the terminal. Written in Rust (ratatui TUI) as a learning project.
The long-term intent, stated in the README and in `crates/core/src/lib.rs`:
everything beyond the core (tools, extensions, persistence) arrives as
extensions later, and the core is being shaped now so that slots in without
rework.

## Workspace layout

```
crates/
  core/   → lib `hi-core`  — the brain: providers, events, messages, config,
                             selection, settings, thinking levels, system prompt.
                             THE ONE RULE: never touches the terminal.
  tui/    → lib `hi-tui`   — the terminal front end (ratatui, inline viewport).
  cli/    → bin `hi`       — thin: flag parsing, startup resolution, dispatch.
```

Dependency direction (nothing points up):

```
cli ──→ tui ──→ core
 └──────────→ core        (cli resolves selection/config directly)
```

Headless (`--print`) is the second branch in `crates/cli` that links `hi-tui`
never — that is the point of the core rule.

## Toolchain and dependencies

- Rust edition 2024, resolver 2. Rust 1.85+. `nix develop` provides the
  toolchain (flake.nix + .envrc/direnv).
- `hi-core`: `serde`, `serde_json`, `ureq` 3 (feature `json`). Nothing else —
  deliberately. HTTP is blocking `ureq`; there is **no async runtime
  anywhere** in the project (stated as a design decision in
  `crates/core/src/openai.rs`).
- `hi-tui`: `hi-core`, `ratatui`. Note: ratatui re-exports its crossterm
  dependency, so one dep covers both.
- `hi-cli`: `hi-core`, `hi-tui`, `clap` (derive).

## Threading model (the spine of the whole design)

- One turn = one OS thread, spawned by `hi_core::spawn_turn`
  (`crates/core/src/provider.rs`). The thread owns an `Arc<dyn Provider>` and
  a copy of the history, and streams `hi_core::Event`s into an `mpsc`
  channel.
- The UI thread never blocks: it drains the channel with `try_recv` once per
  frame (50ms poll — `crates/tui/src/lib.rs::event_loop`).
- **Cancellation = dropping the `Receiver`.** Every subsequent `send` fails,
  and a well-behaved provider returns promptly. Esc in the TUI works by
  dropping the `Turn` (which owns the receiver). There are no cancellation
  tokens, no abort flags.
- Because cooperative cancellation only works if the worker reaches a
  checkpoint, network *connect* is bounded by timeouts (15s connect / 10s DNS
  resolve, `bounded_agent()` in `crates/core/src/openai.rs`). Turn streams
  themselves are unbounded on purpose — turns think for minutes — and rely on
  the dropped receiver.
- Background tasks (#5, AD-0019) extend the same spine: one worker thread
  per task polling its child at 50ms, a completion channel the UI drains
  with `try_recv` per frame (the same rhythm as the event pump), and
  process groups so kills reach grandchildren. Threads, not tokio —
  decided here because the Luau bridge (#6) inherits it.

## Design principles (visible in the code, enforced by tests)

1. **Exhaustive enums over stringly data.** `Message` and `Event` are enums
   so the payload lives in the variant; adding a variant (e.g. a future
   `ToolCall`) forces every consumer to handle it at compile time. This is
   the designated extension axis.
2. **Single finalization path.** Whatever happens during a turn, the event
   sequence always ends with `TurnEnd`; errors ride as `Event::Error` *before*
   the `TurnEnd`, after any partial tokens. Consumers keep exactly one
   finalization code path. See [turn-lifecycle.md](turn-lifecycle.md).
3. **Blocking I/O + threads, not async.** Simplest tool that keeps the UI
   responsive. Revisit only if the threading model actually hurts.
4. **Forward-compatible config.** Unknown fields in `models.json` /
   `settings.json` are ignored on purpose, and key names are camelCase to
   match pi, so entries can be copied back and forth.
5. **Errors name the fix.** Selection/discovery errors list what *was*
   available (configured providers, per-provider failure reasons), so the
   message is actionable.
6. **Mutate in handlers, read in render.** All TUI state lives in `App`;
   `handle_key`/`pump` mutate it, `ui::draw` only reads it. No widget draws
   outside `ui.rs`.
7. **Credentials never leak.** `OpenAiCompatProvider`'s hand-written `Debug`
   prints `[set]` instead of the key; env interpolation happens once at
   launch.

## Current status

Works today (see also README "What works today"):

- Inline TUI (bottom 12 rows, no alt-screen, transcript lives in native
  scrollback), streaming replies, Esc cancel, queued mid-turn messages
- pi-style transcript: markdown rendering (pulldown-cmark), role-themed
  blocks with background fills, merged tool blocks with state tints (see
  [tui.md](tui.md), [decisions.md](decisions.md) AD-0005)
- Session selection: `--continue`, `--resume` picker (also `/resume`
  in-session), `--session <path|id>`, `--fork <path|id>`, `--name` —
  core returns facts (listing, resolution), the CLI owns policy (see
  [cli.md](cli.md), [decisions.md](decisions.md) AD-0006)
- Wire identity & caching: honest `User-Agent` always, telemetry-gated
  attribution, session-id prompt-cache affinity with a tested
  byte-stable payload prefix (see [core.md](core.md),
  [decisions.md](decisions.md) AD-0007)
- Images in read: magic-byte sniffed, fail-open policy (pi-parity:
  off only via the `images` master switch or an explicit
  `vision: false`), attached via pi's synthetic-user-message wire
  (see [core.md](core.md), [decisions.md](decisions.md) AD-0008)
- Image resize: pi's ladder — 2000×2000 / 4.5 MB base64 budget, PNG
  + JPEG-quality candidates, run once before history (see
  [core.md](core.md), [decisions.md](decisions.md) AD-0009)
- Kitty graphics: attached images render as real graphics on
  kitty-protocol terminals — env-sniffed detection, one-shot
  escape-in-cell emission into scrollback, text-note fallback
  elsewhere (see [tui.md](tui.md), [decisions.md](decisions.md)
  AD-0010)
- Paste: Ctrl-V stages a clipboard image at a temp file and inserts
  the path as text — nothing sends until Enter, and context entry
  stays `read`'s job (see [tui.md](tui.md),
  [decisions.md](decisions.md) AD-0011)
- `mock` + `hi_core::OpenAiCompatProvider` behind the one `Provider` trait
- pi-style `models.json` with `$ENV` key interpolation, model discovery,
  hybrid listing policy
- Model selection precedence (flags → saved default → autopick), `/model`
  picker with Ctrl-S save, `/exit` + `/quit` commands, `/` completion
- Thinking levels (`off`…`high`): settings key → per-model `thinkingLevelMap`
  kwargs merged into the request body; the active level is shown in the
  status row.
- System-prompt override chain (user + project `.hi/` markdown files)
- Tool system + multi-turn agent loop: `Tool` trait, registry, `ToolGate` interception seam
  (allow/deny/mutate), `spawn_agent_turn` rounds — the model can call tools and the loop
  executes them until it settles (see [decisions.md](decisions.md), AD-0003)
- Session persistence: append-only JSONL per project under
  `~/.local/share/hi/sessions` (cwd-keyed, pi-style); `--continue` resumes
  the newest session for the directory
- Headless mode: `hi -p "prompt"` (piped stdin prepended) runs one turn
  through the same loop and prints the final text — errors on stderr,
  empty stdout, non-zero exit; `--no-session` for ephemeral runs
- Built-in tools: read/bash/edit/write/grep/find/ls ship in `core/tools/`,
  selected by bundle (`--tools`, default `coding`), results clamped by one
  truncation contract, prompt notes woven into the system prompt (see
  [decisions.md](decisions.md), AD-0004)
- Background task manager (#5): `task_run`/`task_status`/`task_output`/
  `task_cancel` — long-lived work outlives the turn, groups die at exit
  (see [decisions.md](decisions.md), AD-0019)
- 403 `#[test]` functions, no network needed (see [testing.md](testing.md))

Deliberately not yet built:

- **Subagents, background tasks, the Luau extension host** — the loop, the
  tool seam, and the tool set they need are in place. Subagents (#4),
  background tasks (#5), and the Lua host (#6) build on them.
- **Extensions** — direction decided (Luau), nothing implemented.
