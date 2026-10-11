# `hi` documentation — agent-facing index

These docs describe the **current state of the code**. They exist so an agent
(new session, no memory) can answer "where is X and how does Y work" without
rediscovering the repository. The human-facing story lives in the root
[`README.md`](../README.md); when the two disagree, the code wins and these
docs need fixing.

## Reading order

| File | What it covers |
|---|---|
| [architecture.md](architecture.md) | Workspace layout, crate rules, dependency direction, threading model, design principles, current status |
| [turn-lifecycle.md](turn-lifecycle.md) | End-to-end data flow of one turn: input → provider thread → events → pump → finalize; cancellation; queueing |
| [core.md](core.md) | `hi-core` reference: every module, type, and contract (`Provider`, `Event`, `Message`, config, selection, settings, thinking, system prompt, mock, test support) |
| [tui.md](tui.md) | `hi-tui` reference: `App` state machine, turn handling, slash commands, completion, `/model` picker, status bar, rendering geometry, terminal lifecycle |
| [cli.md](cli.md) | The `hi` binary: flags, startup resolution flow, exit codes |
| [configuration.md](configuration.md) | `models.json` + `settings.json` schemas, credential interpolation, model discovery policy, system-prompt file chain |
| [testing.md](testing.md) | How to run tests, `FakeApi`, per-module test inventory, test conventions |
| [decisions.md](decisions.md) | Decision log: extension runtime = Luau (rationale, rejected alternatives), extension capability wishlist, prerequisites, roadmap |

## Conventions

- **No line numbers** in these docs — they rot. Find things by symbol name
  (`rg 'fn pump' crates/`) or by file, then read.
- Invariants are stated explicitly and are load-bearing. If a change breaks a
  stated invariant, update the invariant section in the same commit.
- Cross-references are relative links. Each doc lists the source files it
  describes at the top.
- `docs/decisions.md` is append-only in spirit: record new decisions as new
  entries; supersede rather than rewrite.

## Quick orientation

- Three crates: `crates/core` (no terminal access, ever), `crates/tui`
  (ratatui front end), `crates/cli` (thin binary).
- One wire protocol: OpenAI-compatible chat completions, streamed over SSE.
- No async runtime anywhere: blocking I/O on dedicated threads, `mpsc` events,
  cancellation by dropping the `Receiver`.
- Not implemented yet (by design): tools, multi-turn agent loop, session
  persistence, extensions. See [decisions.md](decisions.md) for what comes
  next and why.
