# AGENTS.md

`hi` — a minimalist agent harness (Rust, ratatui TUI). Core rule: `crates/core`
never touches the terminal. Workspace: `crates/core` (hi-core), `crates/tui`
(hi-tui), `crates/cli` (the `hi` binary).

## Commands

```sh
nix develop          # toolchain (or any Rust 1.85+; edition 2024)
cargo build          # build the hi binary
cargo test           # full suite, no network needed
cargo run -- --mock  # run without config
```

Always work inside the **nix dev shell** (`nix develop`): do not expect the
Rust compiler or any other non-standard tool to exist on the host machine.
When a task needs a new tool (formatter, linter, debugger, language server,
etc.), add it to the dev shell in `flake.nix` instead of assuming it is
preinstalled or installing it globally. This rule governs **local
development only** — CI runs on plain GitHub Actions runners (see issue
#7), kept on the same toolchain channel as `flake.nix` so local and CI
builds match.

## Documentation

`docs/` is the agent-facing map of the codebase — read the relevant file
before working in that area, and **update the doc in the same commit when you
change behavior it describes**. Docs use symbol names, never line numbers;
find code with `rg '<symbol>' crates/`.
Docs are part of done: documentation must always be kept up to date. After
committing, **verify the change is actually reflected in the relevant
`docs/` files** (and `README.md` for user-facing behavior); if a gap slipped
through, send a follow-up commit to the same PR — never leave docs stale for
a later pass.

| Doc | Read when… |
|---|---|
| [docs/architecture.md](docs/architecture.md) | touching workspace/deps/threading, or needing the big picture |
| [docs/turn-lifecycle.md](docs/turn-lifecycle.md) | touching events, `spawn_turn`, `pump`, cancellation, queueing |
| [docs/core.md](docs/core.md) | working in `crates/core` (providers, messages, config, selection, settings, thinking, prompts) |
| [docs/tui.md](docs/tui.md) | working in `crates/tui` (App, commands, picker, status bar, rendering) |
| [docs/cli.md](docs/cli.md) | touching `crates/cli` (flags, startup flow, exit codes) |
| [docs/configuration.md](docs/configuration.md) | touching config schemas, credentials, system-prompt files |
| [docs/testing.md](docs/testing.md) | writing/running tests, using `FakeApi` |
| [docs/decisions.md](docs/decisions.md) | working on extensions/tools/subagents, or wondering "why is it built this way" |

## Ground rules

- **Do not trust training data alone**: verify every claim against available
  sources — the actual source code (read the file, `rg` for the symbol) for
  anything about this repo, and web search / current upstream docs for
  anything about external tools, libraries, or APIs. Versions and behavior
  drift; memory of them does not count as evidence.
- **All changes land via pull requests** — `main` is protected; work happens
  on short-lived branches, one focused change per PR.
- **Stay focused**: one task at a time, per the current issue/PR. Do not pull
  adjacent work into the current change — file non-blocking side missions as
  GitHub issues for later instead of doing them inline. Scope may grow only
  for work that blocks the current task, and separable blockers become their
  own PR first.
- **Merge hygiene & explicit starts**: when the maintainer says a PR is
  merged, clean up the merged branch (local and remote), switch to `main`,
  and sync — that and nothing else. Never start a new task, issue, or PR
  without an explicit go, even when the next step looks obvious.
- The event contract (single finalization path: `TurnStart` … `TurnEnd`,
  errors as `Event::Error` before `TurnEnd`) and the dropped-`Receiver`
  cancellation contract are load-bearing — see
  [docs/turn-lifecycle.md](docs/turn-lifecycle.md) before changing either.
- Config keys stay camelCase and unknown fields stay ignored (pi-compatible,
  forward-compatible).
- Decisions live in [docs/decisions.md](docs/decisions.md) (append; don't
  relitigate without new facts). Extension runtime = Luau via mlua (AD-0001);
  the core tool system is the prerequisite for most extension work (AD-0002).
