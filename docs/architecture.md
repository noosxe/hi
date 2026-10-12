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

A future headless mode is a second branch in `crates/cli` that links `hi-tui`
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
- 88 `#[test]` functions, no network needed (see [testing.md](testing.md))

Deliberately not yet built:

- **Built-in tools** — the tool *system* is in (registry, gate, loop); no tools are shipped
  yet, so sessions still run with an empty registry. Subagents (#4), background tasks (#5),
  and the Lua host (#6) build on it.
- **Session persistence** — history is in-memory only, one process lifetime.
- **Headless one-shot mode** — planned as a second branch in `crates/cli`.
- **Extensions** — direction decided (Luau), nothing implemented.
