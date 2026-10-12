# Decision log

Append-oriented: new decisions get a new entry; superseded entries stay with
a note. Each entry records context, the decision, and what it rules out, so
future sessions don't relitigate.

---

## AD-0001 — Extension runtime: Luau (via mlua)

**Status:** decided (2026-10-11). Nothing implemented yet.

**Context.** `hi` will get an extension system. Requirements gathered up
front: LSP support, subagent support, MCP support, extensions that make HTTP
requests, status-bar contributions, add/replace agent tools, event
interception on the agent loop (allow/deny actions), background-task
support. Ground rules set by the maintainer:

- JS/TS runtimes are out (no QuickJS, no V8/deno, no Node parity goal).
- Extensions are written **by agents**, not by hand (pi's philosophy too):
  the runtime should optimize for machine-checkability and LLM-corpus
  quality, not human ergonomics.
- A handful of important pi extensions may be ported to Lua when needed;
  most pi extensions are duplicates or noise.

**Decision.** Embed **Luau** through the `mlua` crate (feature `luau`;
`luau-jit` available later). Rationale, strongest first:

1. **Sandboxing is Luau's product.** Designed at Roblox to run actively
   malicious, untrusted user scripts: unsafe stdlib removed, VM isolation
   primitives (safeenv, `table.freeze`), memory-safety fuzzing, host-set
   memory limits, and the global **interrupt** (mlua `Lua::set_interrupt`,
   guaranteed to fire at any call/loop boundary) for CPU budgets. Project-level
   extensions in an agent harness have exactly this threat model.
2. **A static gate for agent-written code.** Gradual types with first-class
   annotations: ship `hi.d.luau` definition files for the whole extension
   API, run `luau-analyze` at load/dev time, and hallucinated APIs are
   rejected before any host state is touched. The agent's write→fix loop
   becomes analyze-error → correct → reload.
3. **Ergonomics reduce LLM codegen error**: `continue`, compound assignment,
   string interpolation, generalized iteration.
4. **Performance is a non-issue for glue.** Interpreter ≈ LuaJIT interpreter
   performance; every wanted extension is I/O-bound anyway (HTTP, processes,
   LLM streams live in Rust host APIs exposed to Lua).
5. **mlua support is first-class** (vendored Luau ~0.740 as of mlua 0.12.x):
   async (any executor), `send`, serde bridging, `Table::set_safeenv`.

**Accepted costs:** no 64-bit integers (doubles only, safe to 2^53; `bit32`
for bit ops — acceptable because extensions orchestrate; JSON and numeric
heavy lifting stay host-side; mlua ≥0.12 rejects fractional→int conversions
loudly), no `goto`, no tail calls (irrelevant for glue), Roblox-skewed
training corpus (expect occasional `game:GetService()`-style hallucinations;
the analyze gate corrects them deterministically).

**Alternatives rejected (do not relitigate without new facts):**

| Option | Why not |
|---|---|
| Lua 5.4 | Boring-solid and fully featured in mlua (memory limits + instruction hooks), but no static checking surface for agent-written code; load-time validation is runtime-only |
| Lua 5.5 | `global` declarations would catch invented globals, but the corpus is too thin for agents (2025 language); breaking changes trip 5.4-trained codegen. Revisit later |
| LuaJIT | Optimizes the one axis we don't need (hot-loop JIT) while failing the sandbox axes: no mlua memory limits (needs Lua ≥ 5.2), debug hooks disable JIT while installed, FFI is a sandbox hole, 5.1 dialect |
| QuickJS/rquickjs | JS-family excluded by maintainer direction |
| deno_core / V8 / Node | Excluded; also heavy (binary size, memory, build) |
| WASM components | Strongest sandbox, but poor UI ergonomics and slow boundary crossing for trickle interactions (status bar); keep in mind for a future untrusted marketplace tier |
| Out-of-process RPC plugins | Keep as a tier-2 story for heavyweight extensions; LSP/MCP don't need it (their subjects are external processes anyway) |

**Design rules agreed alongside the runtime choice:**

- The extension surface is defined as a **Rust-side host trait** (load,
  register_tool, register_command, on_event, UI contributions) so the
  runtime stays swappable behind it.
- The host **mediates every capability** (HTTP, process spawn, fs) — no raw
  net/OS APIs handed to Lua. This doubles as the permission model.
- The `core never touches the terminal` rule is preserved: extensions feed a
  UI-state bridge; `hi-tui` keeps sole drawing ownership. **Lua never draws.**
- Per-`Token` event hooks must be **opt-in or batched** — the default
  streaming path stays zero-cost across the FFI boundary.
- Subagents and background tasks are **core features** (agent runner, task
  manager) that extensions configure/wrap — not extension-runtime problems.

## AD-0002 — Extension prerequisites live in core first

**Status:** agreed direction (2026-10-11), not started.

Six of the eight extension wants presuppose machinery `hi` does not have:

1. Tool system in core: `ToolCall`/`ToolResult` variants for `Message` +
   `Event`, a tool registry, and a multi-turn agent loop that dispatches
   tool calls and consults hooks before executing (the intercept/deny seam).
2. An agent runner (nested loop over the same `Provider`/`Event` machinery)
   for subagents.
3. A task manager (spawn/lifecycle/cancel/notify) for background work;
   extensions subscribe.
4. Session persistence (extensions and subagents need resumable context).

Sequencing consequence: build the tool system and agent loop before the Lua
host; the Lua host wraps these seams rather than replacing them. The only
extension wants that could land without tools are status-bar segments and
slash commands.

## AD-0003 — Tool system: separate variants, loop-owned stream, closure gate

**Status:** implemented (issue #2).

- `Message::ToolCall(ToolCall)` / `Message::ToolResult(ToolResult)` are separate
  transcript entries, not fields on `Assistant` — rendering, wire replay, and tests treat
  them as first-class, and `arguments` is kept as raw JSON text (the model's exact
  words; the tool owns its schema).
- The agent loop (`spawn_agent_turn`) owns the event stream: consumers see one
  `TurnStart`/one `TurnEnd` per user message regardless of how many provider rounds or
  tool executions happened. Round framing is an implementation detail.
- Execution is sequential in proposal order — determinism over parallelism; background
  tasks are #5's concern. Every proposed call produces exactly one result;
  deny/unknown/invalid-JSON are error *results*, never `Event::Error`.
- The interception seam is a plain `dyn Fn(&ToolCall) -> ToolDecision` (`ToolGate`). The
  smallest thing that satisfies allow/deny/mutate; the Lua host (#6) backs this seam from
  outside core, so core stays runtime-free (AD-0001's swappable-host rule).
- Wire mapping: consecutive `Assistant` text + `ToolCall` history entries merge into ONE
  assistant message (OpenAI's shape); results ride as `role:"tool"` + `tool_call_id`.
  Streamed calls arrive as index-keyed fragments and surface as `Event::ToolCall`s after
  the message's text tokens.

## Open items (parked)

- Prototype the Luau↔tokio-style scheduler bridge (coroutines driving host
  futures) before committing the host design — note hi currently has **no
  async runtime**; decide whether the bridge stays thread/callback-based or
  introduces tokio. The decision above does not depend on this.
- Decide the trust model for project vs user extensions (pi's `project_trust`
  is the reference pattern).
- README numbers lag the code (says 43 tests; 76 exist) — refresh when
  touching the README next.


## AD-0004 — the harness ships its tools (2026-10-12)

**Status:** implemented (issue #26). AD-0003 built the tool *system*; this
decision ships the tools.

**The decision:** built-in capabilities live in `hi-core`
(`crates/core/src/tools/`), selected per run by a bundle — `none`,
`read-only` (read/grep/find/ls), `coding` (read/bash/edit/write), `all` —
via `--tools` or `settings.defaultTools`. The default is **`coding`**: hi
is a coding harness first (maintainer ruling on #26), and it matches pi's
`defaultTools`.

- **Tool prompt contributions**: each tool carries a prompt note ("use read
  to examine files instead of cat"), and the registry weaves them into the
  system prompt — tool metadata teaches the model, not just the harness.
- **Security stance (pi's, deliberate)**: tool paths resolve against the
  cwd but are *not jailed*; there is no per-call approval prompt. Tools run
  with the session's OS permissions — the boundary is the operating system,
  not the harness. Containerize when the work is untrusted.
- **Dependencies**: regex and glob matching use the `regex` and `globset`
  crates (ruling: no hand-rolled parsers). Precedent set: canonical
  pure-Rust crates are acceptable when the alternative is hand-rolling —
  the first additions since `ureq`.
- **One truncation contract**: every tool result clamps at 2000 lines or
  50 KiB (head for inspection tools, tail for bash), implemented once in
  `tools/truncate.rs` with its own tests.
- **v1 limitations are filed, not hidden**: root-level `.gitignore` only
  (#28), no image reads (#29), bash timeout kills the child but not
  grandchildren (#30), no `/tools` command yet (#27), no Windows powershell
  twin (#31).

---

## AD-0005 — the transcript renders pi-style (2026-10-14)

**Status:** implemented (issue #33, rulings recorded in the issue).

**The decision:** the transcript abandons dim text labels ("you ",
"assistant ") for pi's identity model — **role blocks with background
fills**. User messages render as markdown inside a tinted full-width block;
assistant replies render as plain markdown on the terminal's default colors;
tool executions render as one merged block (bold tool name + collapsed args
preview over the output preview) whose fill is state-tinted — pending while
running, then success or error. Colors are **roles, never literals** at call
sites (`THEME.fg(Fg::Heading)`), with the single dark palette being pi's
`dark.json` OKHSL tokens resolved through pi's own color math.

- **Architecture: inline parity** (researched in `pi-tui/dist/tui-main-screen.js`):
  pi's default mode owns only a bottom live viewport; rows pushed into native
  scrollback carry their baked ANSI colors and freeze there. hi does the same
  with ratatui's primitives — `insert_before` prints blocks once at final
  state; the managed region hosts the live pending tool block and the
  streaming markdown. Accepted consequences, identical to pi inline: history
  freezes at print-time width on resize, no expand/collapse. A fullscreen
  opt-in mode (alt-screen viewport, rewrap, expandable blocks) remains a
  possible future issue — pi has the same two-mode split.
- **Markdown engine**: `pulldown-cmark` with pi's `marked` extension set
  (GFM tables, strikethrough, tasklists; no math, by ruling). The renderer
  maps events → themed rows following pi-tui's conventions (heading
  prefixes, restated code fences, `│ ` quotes, box-drawn tables with
  proportional column shrink). Deps: `pulldown-cmark` (plus its
  `getopts`/`unicase`/`pulldown-cmark-escape` tree) and `serde_json`
  (already in the workspace) land in `hi-tui` — the AD-0004 precedent
  extends: canonical crates over hand-rolling CommonMark.
- **User input renders as markdown too** (pi parity), accepting the
  paste-distortion trade; system and error lines stay plain colored text.
- **Spacing is per-block padding, not separators**: every block carries one
  blank row above and below its content (`paddingY`), tinted with the fill
  for blocks that have a background, plain otherwise — two blank rows
  between any adjacent content rows, with no cross-block bookkeeping in
  the print loop (a first cut used a separator counter that reset every
  frame; blocks owning their padding removed the whole class of bug).
- **Known divergences from pi, recorded**: no syntax highlighting in code
  blocks (pi uses cli-highlight), collapsed args preview sorts keys (serde
  maps vs marked's source order), no expand keybind on the preview marker
  (a fullscreen-mode feature), no OSC 8 hyperlinks or OSC 133 scroll zones.

---

## AD-0006 — session selection: core facts, CLI policy (2026-10-14)

**Status:** implemented (issue #22, rulings ①–⑧ recorded in the issue).

**The decision:** selection splits by crate. `hi-core::session` returns
*facts* — `SessionSummary` listings (`sessions_in`/`sessions_all`, mtime
newest, unreadable files skipped), argument resolution
(`resolve → Local | Global { path, cwd } | NotFound`, pi's order:
path shape, exact id, id prefix — project first, then global), and
`Session::fork_from` (source read-only; copy gets fresh id + `createdAt`,
the forking directory in its header, and a `forkedFrom` pointer to the
source path). The CLI owns *policy*: which flag wins, what Esc means,
when a cross-project hit is a prompt (interactive y/N, pi's) versus a hard
error (`--print`, pointing at `--fork`).

- **Headless determinism over pi's rough edge**: pi's print mode starts
  the interactive picker with no guard. hi refuses `--resume` under
  `-p` — `--session <id|path>` and `--continue` are the headless
  selectors. Deliberate divergence, recorded.
- **Two new optional header fields**, both camelCase and skipped when
  absent (old builds read new files): `name` (`--name`/`-n`, shown in
  the picker; fresh starts and forks only — renaming existing files is
  future work) and `forkedFrom`. pi stores the name as a `session_info`
  *entry* because its files are append-only trees; hi's header is
  written once at creation, so fields are simpler.
- **The picker lives in hi-tui over shared composition**
  (`picker::picker_lines`): the standalone `--resume` chooser owns a
  short terminal before the main TUI (pi's startup-TUI pattern);
  `/resume` reuses the same lines in-session and swaps the live session
  by resetting the print cursor — the previous conversation stays in
  scrollback, terminal-owned, exactly like every other finalized block.
- **Flag surface** mirrors pi: `-r/--resume`, `--session`, `--fork`,
  `-n/--name`, mutually exclusive per pi's conflict matrix; `--name`
  pairs with fresh starts and forks only.

---

## AD-0007 — wire identity: honest UA, opt-in telemetry, session-id cache affinity (2026-10-14)

**Status:** implemented (issue #38; design rulings in the issue). Deferred
usage display: issue #39.

**The decision:** every outbound request carries
`User-Agent: hi/<version> (<os>; <arch>)` — honest product identification,
never gated, because providers and proxies use it for support triage and
abuse attribution and a bare `ureq/3.x` misidentifies every hi user.
Everything beyond that identity is opt-in.

- **`telemetry` (settings, default off; `HI_TELEMETRY` env wins — pi's
  `PI_TELEMETRY` pattern) gates exactly one thing**: attribution headers
  toward hosts that display them (OpenRouter's `HTTP-Referer`/`X-Title`,
  host-matched exactly, never suffix-matched). hi has no analytics, no
  install pings, no update checks — the documented statement is "the
  User-Agent is the whole always-on telemetry surface."
- **Session affinity is routing, not tracking, and is not gated**: with an
  open session, requests carry `prompt_cache_key: <session id>` (and
  `x-session-id` toward OpenRouter). The value is the session's random
  uuid — no user content, no cross-session linkability beyond what the
  provider already sees. The session id is the *correct* key: stable
  across `--continue`/`--resume`/`--session`, follows `/resume` switches
  (the TUI rebuilds the provider with the new id), fresh on forks.
- **Prefix stability is enforced, not hoped for**: caches on
  OpenAI-compatible servers match by request prefix, so the test
  `consecutive_turns_keep_a_byte_stable_prefix` pins the invariant that
  turn N+1 re-serializes turn N's system prompt, tool definitions, and
  history byte-for-byte. Session headers carry `name`/`forkedFrom`
  skipped-when-absent so old headers never perturb the wire.
- **Recorded non-goals**: Anthropic-native `cache_control` breakpoints
  (hi speaks OpenAI-compat only; OpenRouter handles Anthropic caching
  server-side), `prompt_cache_retention`/ttl knobs (extra cost, niche),
  cost display. Surfacing `cached_tokens` from the final usage chunk is
  issue #39.

---

## AD-0008 — images in read: tool-result image blocks, policy at the only entry point (2026-10-14)

**Status:** implemented (issue #29; rulings recorded on the issue). Deferred:
#42 kitty rendering, #43 resize/transcode, #41 user-attached images.

**The decision:** images enter the system in exactly one place — the `read`
tool — and the policy that decides whether they may be attached is resolved
exactly once, at startup, where both inputs exist:

- **`ToolResult.images: Vec<ImageBlock>`** (base64 + mime type) — facts, not
  policy: the result records what the tool produced. Additive serde
  (`default` + skip-when-empty), so pre-image session files load unchanged
  and old builds ignore the field — no `Message` variant churn anywhere.
- **Policy = `settings.images && startingModel.vision`**, computed in
  `resolve_target` and handed to the registry → `read`. The tool attaches
  only when policy allows, and its content note always tells the truth
  (attached / disabled / over the 5 MB gate) — a non-vision model never
  wonders where its image went. `/model` does not re-evaluate; the failure
  direction is a metadata note, never a burned request. "No point in
  sending if not supported" (ruling).
- **The wire transform is wire-only** and mirrors pi-ai exactly: OpenAI-compat
  `role: "tool"` is text-only, so the tool message keeps its note (or
  `(see attached image)` when image-only) and the images ride a synthetic
  follow-up user message of typed parts. History stays clean; every turn
  re-derives the carrier from stored facts; deterministic base64 keeps the
  AD-0007 prefix invariant intact.
- **Magic-byte sniffing, never extensions**: a `.png` full of text is text.
  Hand-rolled (png/jpeg/webp/gif — the four formats the `image_url` wire
  accepts); the only new dependency is `base64` (canonical, zero-transitive).
- **`execute_with_images` is the only execution path the loop calls**; the
  default wraps `execute` with no images, so the other ten tools are
  untouched and image support stays opt-in per tool.

**Recorded non-goals (each filed):** resize/transcode to model input limits
(#43 — the dep question is the whole question there), kitty graphics
rendering in the TUI (#42 — v1 shows dim metadata notes), pasted/user
attachments (#41 — takes the `Message::User` enum churn this decision
deliberately avoided), Anthropic-native content blocks (OpenRouter
translates), sixel.
