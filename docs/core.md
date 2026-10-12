# `hi-core` reference

Files this doc describes: everything under `crates/core/src/` — `lib.rs`,
`message.rs`, `event.rs`, `tool.rs`, `agent.rs`, `session.rs`, `headless.rs`, `tools/`, `provider.rs`, `openai.rs`, `mock.rs`, `config.rs`,
`selection.rs`, `settings.rs`, `thinking.rs`, `system_prompt.rs`,
`test_support.rs`.

Public surface re-exported from `crates/core/src/lib.rs`: `NAME`, `Event`,
`Message`, `MockProvider`, `ModelsConfig`, `ModelConfig`, `ProviderConfig`,
`OpenAiCompatProvider`, `Provider`, `spawn_turn`, `Settings`, `ThinkingLevel`, `Tool`, `ToolBundle`,
`ToolAnnotations`, `ToolCall`, `ToolResult`, `ToolDecision`, `ToolGate`, `ToolRegistry`,
`DEFAULT_MAX_TOOL_ROUNDS`, `spawn_agent_turn`, `Session`, `SessionHeader`, `LoadedSession`.
Note: `selection` is a public module (`hi_core::selection::{resolve, list_for,
Selected}`), not re-exported at the root.

## message.rs — the transcript entry type

`enum Message { User(String), Assistant(String), ToolCall(ToolCall), ToolResult(ToolResult), Error(String), System(String) }`
plus `Message::text(&self) -> Option<&str>` (the four prose variants; the tool variants carry
structured payloads instead).

**Invariants**
- Enum, not struct-with-role: payload lives in the variant, `match` is
  exhaustive. Adding a kind (e.g. `ToolUse`) must force consumers to adapt.
- `Error` messages are displayed in the transcript but **never sent to the
  model** — enforced by `wire_messages` filtering and by the test
  `error_messages_are_never_sent_to_the_model` in `openai.rs`.
- `ToolCall`/`ToolResult` always appear as a pair in agent-loop-built history — one result per
  call, results directly after their calls (see [turn-lifecycle.md](turn-lifecycle.md)).

## event.rs — the turn event stream

`enum Event { TurnStart, Token(String), ToolCall(ToolCall), ToolResult(ToolResult), TurnEnd, Error(String) }`,
`Clone + PartialEq`. This is how a turn in progress talks to the UI.
**Invariants (the single finalization path)**
- A turn always emits `TurnStart` first and exactly one `TurnEnd` last.
- `Token`s arrive in order; concatenating them reproduces the full reply.
- On failure: partial `Token`s still stand, then exactly one `Error`, then
  `TurnEnd`. Producers: see `OpenAiCompatProvider::respond`. Consumers
  therefore need exactly one finalize code path (`App::pump` is the reference
  consumer).

## tool.rs — what a model may call, and who decides what runs

- `Tool` trait: `name / description / parameters (JSON Schema value) / annotations / execute`.
  Deliberately synchronous — tools run on the agent-loop worker thread, off the UI; long tools
  do their own threading. `ToolAnnotations { read_only, destructive }` are hints for gates/UIs;
  hi never enforces them.
- `ToolRegistry`: `BTreeMap<String, Arc<dyn Tool>>` — stable sorted wire order, duplicate-name
  rejection, `definitions()` for the request's `tools` array. Empty registry ⇒ no `tools` field
  on the wire at all.
- `ToolCall { id, name, arguments }` / `ToolResult { call_id, name, content, is_error }`:
  `arguments` stays **raw JSON text** (the model's exact words; the tool owns its schema).
  `is_error` is harness-side only — the OpenAI wire has no such flag.
- The interception seam: `ToolGate = dyn Fn(&ToolCall) -> ToolDecision`, with
  `ToolDecision::{ Allow, Deny(reason), Mutate(arguments) }`. A plain function type on purpose —
  the extension host (#6) backs this seam from outside core, so core stays runtime-free.

## agent.rs — the multi-turn loop

`spawn_agent_turn(provider, registry, gate, history, max_tool_rounds) -> Receiver<Event>`
(default rounds: `DEFAULT_MAX_TOOL_ROUNDS`). Runs rounds of `spawn_turn` + tool execution until
the model settles; the consumer sees ONE `TurnStart`/ONE `TurnEnd` per user message — the loop
swallows per-round framing and emits the final `TurnEnd` itself. Every proposed call yields
exactly one result (deny / unknown tool / bad JSON are error *results*, never `Event::Error`).
Cap exceeded ⇒ `Error("tool loop did not settle within N rounds")` + `TurnEnd`. The loop never
mutates the caller's history — consumers mirror the event stream (details in
[turn-lifecycle.md](turn-lifecycle.md)).

## tools/ — the built-in capabilities (#26)

The harness ships tools, not just a loop. One file per tool under
`crates/core/src/tools/`, plus two support modules: `truncate.rs` (the
output contract) and `walk.rs` (gitignore-aware directory walking for
grep/find).

- **The seven built-ins**: `read` (line-numbered, `offset`/`limit` paging),
  `bash` (optional `timeout` seconds, no default; tail-kept output; non-zero
  exit is an error result with the output inside), `edit` (`edits[]` matched
  against the ORIGINAL file, each exactly once, overlaps rejected),
  `write` (creates parents, full overwrite), `grep` (`regex` crate, rg-style
  rendering, `glob` filter, context), `find` (`globset`, .gitignore-aware),
  `ls` (sorted, `/` suffix for dirs, dotfiles).
- **Truncation contract**: every result clamps at 2000 lines or 50 KiB,
  whichever hits first — head-kept for inspection tools, tail-kept for bash.
  The contract lives in `truncate.rs` and has its own tests.
- **Bundles** (`ToolBundle`): `none`, `read-only` (read/grep/find/ls),
  `coding` (read/bash/edit/write — the default: hi is a coding harness
  first), `all`. Selected by `--tools` or `settings.defaultTools`;
  `ToolBundle::registry(cwd)` builds the registry.
- **Prompt contributions**: every tool may carry a `prompt_note`
  ("use read to examine files instead of cat"); the registry joins them and
  the CLI weaves them into the system prompt under `# Tool notes`.
- **Security stance (pi's, deliberate)**: tool paths resolve against the
  cwd but are **not jailed**; there is no per-call approval prompt. Tools
  run with the session's OS permissions — the boundary is the operating
  system, not the harness. Run hi in a container when the work is untrusted.
- The registry rides in two places by design: `.with_tools(...)` puts the
  definitions on the wire, and the same `Arc` drives execution inside
  `spawn_agent_turn` — resolve_target keeps them in sync, and the `/model`
  picker re-attaches both.
- Deferred with issues: full gitignore semantics (#28), images in read
  (#29), process-group kill for bash (#30), `/tools` command (#27).

## session.rs — transcripts that survive restarts

One JSONL file per session under the data dir (`config::data_dir()` +
`sessions/`), in a cwd-keyed project directory (pi-style encoding). Only
finalized messages are persisted, appended and flushed at turn end; the
reader tolerates a torn final line and rejects malformation anywhere else.

- `Session::start` / `Session::resume(path)` — create or reopen; the file
  handle stays open in append mode for the session's life.
- `Session::append(&[Message])` — one JSONL line per message, flushed.
- `session::latest(cwd)` — newest readable session for a project
  (`--continue`); `session::load(path)` — full parse.
- `SessionHeader` is the flat first line (`camelCase`, `version` field for
  future migrations); `Message` serializes as externally-tagged camelCase
  (`{"toolCall":{…}}`), so tool traffic round-trips losslessly.
- Persistence is best-effort by contract: callers degrade to a transcript
  note when the disk fails — chat never blocks on I/O.

## headless.rs — one-shot runs for scripts

`hi --print` is the scripting interface: prompt in, final assistant text
out, process exits. The drain lives here so it is testable without a
terminal — the CLI only assembles the prompt, session, and exit code.

- `headless::run(provider, registry, gate, history, prompt, session)` —
  pushes the prompt, drains the same agent loop the TUI uses, updates
  `history` with every finalized message, appends to the session
  (best-effort: an append failure drops persistence, never the turn),
  and returns `Ok(final_text)` or `Err(reason)`.
- Error contract (pi's): `Err` means the CLI prints the reason to
  stderr and exits non-zero with stdout left empty — partial output
  survives only in the transcript and session file.
- `headless::combine_prompt(stdin, arg)` — piped stdin first, positional
  argument second, joined by one newline; both empty is a usage error.
- `--mode json` (JSONL event stream) is deliberately not here yet —
  issue #18.

## provider.rs — the abstraction + the turn thread

```rust
pub trait Provider: Send + Sync {
    fn respond(&self, history: &[Message], tx: Sender<Event>);
}
pub fn spawn_turn(provider: Arc<dyn Provider>, history: &[Message]) -> Receiver<Event>
```

- `respond` runs **on its own thread** (spawned by `spawn_turn`), streams
  `Event`s into `tx`, and returns when done.
- `Send + Sync` are required because the provider is moved into an `Arc` and
  used through `&self` from the turn thread.
- **Cancellation contract: dropping the `Receiver` is the cancel signal.**
  Sends start failing; a well-behaved provider returns promptly (mock and
  openai both do; tested by `stops_when_receiver_is_dropped`).

Adding a provider = implement `Provider` + construct it somewhere in
`crates/cli` (see [cli.md](cli.md)). Nothing in core or tui needs to change.

## openai.rs — the one wire protocol

`OpenAiCompatProvider` (Clone) — any OpenAI-compatible endpoint. Everything
here is **blocking `ureq`** by design (no async runtime in the project).

Construction chain:
- `new(base_url, api_key: Option<String>, model)`
- `from_config(cfg: &ProviderConfig, model)` — resolves `apiKey` via env
  interpolation (`$NAME` / `${NAME}` → env at construction time; missing var
  → empty string → treated as no key)
- `.with_system_prompt(Option<String>)` / `.with_thinking_level(Option<ThinkingLevel>)`
  — builder-style, re-applied by the TUI whenever the model picker rebuilds
  the provider.

Wire behavior:
- Turns: `POST {baseUrl}/chat/completions`, body `{model, messages,
  stream: true}`, optional leading `role: "system"` message from the resolved
  system prompt. `Authorization: Bearer <key>` **only when a key resolves
  non-empty** (`authorized()` — some local servers reject the header).
- Thinking kwargs: when the session level has an entry in the model's
  `thinkingLevelMap`, that JSON object is merged **top-level** into the
  request body (`stream()`). No map or no entry → nothing sent.
- SSE parsing: `parse_data_line` classifies each line into
  `DataLine::{Token, Done, Ignore, Malformed}`. `data: [DONE]` ends the
  stream; a server closing without `[DONE]` is tolerated as complete.
- Error mapping: `describe()` turns `ureq::Error::StatusCode(code)` into
  `"chat request: server returned HTTP {code}"` (status code is usually the
  whole diagnosis).

Model listing (hybrid policy):
- `list_models()` returns configured `models[].id` when non-empty (only
  config entries can carry `contextWindow`/`reasoning`), otherwise
  discovers live.
- `discover_model_ids(base_url, api_key)` — `GET {baseUrl}/models`, overall
  10s timeout, extracts `data[].id` **or** `models[].slug`, sorts +
  dedups, and errors loudly when the JSON has neither shape (never fakes
  "no models").
- Timeouts live in one place: `bounded_agent()` — connect 15s, DNS resolve
  10s, **no stream timeout** (turns think for minutes; the dropped receiver
  is the cancel path).

Debug safety: hand-written `Debug` prints `[set]` for the API key, never the
value.

## mock.rs — the no-network provider

`MockProvider` (implements `Default`): canned reply, no network, no key.
Streams `TurnStart` → `Token`s → `TurnEnd` in order; honors the dropped-
receiver contract (`stops_when_receiver_is_dropped`). Ignores system prompts
entirely. Used by `hi --mock` and by most TUI/core tests.

## config.rs — `models.json`

Path (identical on Linux/macOS): `$XDG_CONFIG_HOME/hi/models.json` when set
and non-empty, else `~/.config/hi/models.json` (`config_dir()`,
`models_path()`). Load errors are fatal at startup by design (the CLI tells
the user to create one or use `--mock`).

Types (serde, camelCase, **unknown fields ignored**):
- `ModelsConfig { providers: BTreeMap<String, ProviderConfig> }` — the name
  is just a lookup key; `BTreeMap` gives sorted iteration (autopick order).
- `ProviderConfig { base_url ("baseUrl", required), api_key ("apiKey"),
  models: Vec<ModelConfig> }`
- `ModelConfig { id (required), name, reasoning, context_window
  ("contextWindow", u32, informational), max_tokens ("maxTokens", u32),
  thinking_level_map ("thinkingLevelMap", map of level name → arbitrary JSON
  object) }`

Credential resolution (`resolve_value`): `"$NAME"` / `"${NAME}"` read the
environment once; missing variables become the empty string; anything else is
a literal. Full details in [configuration.md](configuration.md).

## selection.rs — which provider/model a session uses

`resolve(config, provider: Option<&str>, model: Option<&str>,
default_model: Option<&str>) -> Result<Selected, String>` where
`Selected { provider, model }`. Precedence:

1. Both flags → used as given (`--model` deliberately not validated: for
   discovery providers the ids can only be known by asking the server).
2. `--provider P` → first model P offers (configured list or live discovery).
3. `--model M` → first provider **in sorted order whose configured list**
   contains M; discovery-only providers cannot be selected this way.
4. `defaultModel` from settings → only when **no** flags are given; stale or
   malformed values fall through to autopick (never fail startup).
5. Nothing → first provider in sorted order that offers any model; every
   discovery failure is remembered and reported in the final error.

`list_for(config, provider)` powers `--list-models`; errors name the causes
(HTTP status, unrecognized JSON shape).

## settings.rs — `settings.json`

`Settings { default_model: Option<String> ("defaultModel"), thinking:
Option<ThinkingLevel> }`, camelCase, unknown fields ignored. Path: next to
`models.json` (`settings_path()`).

- Missing file = `Settings::default()` (normal first-run state; tested).
- Unparseable file = `Err` (surfaced, not silently ignored; tested).
- `save()` creates parent dirs, writes pretty JSON + trailing newline.

## thinking.rs — reasoning effort levels

`enum ThinkingLevel { Off, Minimal, Low, Medium, High }`. `parse(name)` is
case-insensitive; `name()` is the canonical lowercase form; serde uses the
lowercase name. A level reaches the wire **only** via the model's
`thinkingLevelMap` (see `openai.rs` above) — there is no provider knowledge
in this file, the mapping is pure configuration.

## system_prompt.rs — the prompt chain

`DEFAULT_SYSTEM_PROMPT = "You are hi, a concise terminal assistant."`
(one sentence — hi makes no tool promises).

Resolution (`resolve()` / path-injected `resolve_from()`):
1. default
2. `SYSTEM.md` in the user config dir **replaces** it
3. `SYSTEM.md` in the project's `.hi/` directory **replaces** that (closer to
   the work wins)
4. `APPEND_SYSTEM.md` user, then project, appended blank-line separated

Whitespace-only files read as absent. The result travels as one leading
`role: "system"` message. See [configuration.md](configuration.md) for paths.

## test_support.rs — FakeApi (cfg(test) only)

`FakeApi::start(response)` — minimal HTTP server on an ephemeral port
(`127.0.0.1:0`), one accept loop, one connection per request (responses use
`Connection: close`), records every raw request, answers each with the one
canned response string. Helpers: `sse_response(&[frames])`,
`json_response(body)`, `first_request()`. This is why the whole test suite
needs no network. Usage patterns in [testing.md](testing.md).
