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
