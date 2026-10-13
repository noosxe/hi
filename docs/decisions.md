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

---

## AD-0009 — image resize follows pi: one universal budget, no per-model knowledge (2026-10-14)

**Status:** implemented (issue #43). Supersedes AD-0008's 5 MB raw gate;
that gate described v1, before resizing existed.

**The question that shaped it:** how do we know a model's image limits?
Verified answer: we don't, and can't. The OpenAI-compat wire has no
capabilities endpoint; the documented limits disagree per provider
(Anthropic 10 MB/image API-direct, 5 MB on Bedrock/GCP, 8000×8000 —
collapsing to 2000 px for every image in a request that carries >20;
OpenAI has no per-image byte cap at all, resizing for itself and
rejecting only past 30k patches); some rules depend on request
context, which no catalog field can express. pi's own generated model
catalog defines the `inputLimits` type and populates it zero times —
even the biggest practitioner ships no per-model numbers and runs on
fallbacks. When the guess is wrong, the API's 4xx quotes the real
limit, and the note surfaces it verbatim.

**The decision:** pi's ladder, step for step (`image-resize-core.js` →
`tools/image_resize.rs`):

- **Budget: 2000×2000 px and 4.5 MB of base64 payload** — headroom
  under the tightest mainstream per-image cap (Bedrock/GCP's 5 MB
  base64). The budget counts encoded bytes, because that is what
  rides the wire; hi v1's 5 MB *file* gate encoded to ~6.7 MB and
  was tuned to folklore, not to a provider.
- **An image already within limits is never re-encoded** — original
  bytes, byte-identical, so small screenshots cost nothing.
- **Otherwise: PNG first, then JPEG at q{80, 85, 70, 55, 40}, first
  candidate under budget wins; still over → shrink 0.75× and repeat,
  to 1×1.** Unshrinkable is reported honestly, never sent.
- **Runs once, in `read`, before history.** The `ToolResult` stores
  the resized bytes; replays re-derive the same carrier, and the
  AD-0007 prefix invariant keeps holding (tested).
- **Guards, hi's additions**: a 64 MB raw-file cap (OOM guard, not a
  provider limit) and an 8192×8192 decode cap (a hostile PNG header
  must not become a 4 TB allocation). EXIF orientation is applied
  before anything else, like pi.
- **Dependency:** the `image` crate (AD-0004 canonical), features
  trimmed to the four wire formats. pi avoids native deps with
  Photon/WASM; in Rust the canonical pure-Rust crate is the same
  trade without the wasm.
- **No per-model config seam** — pi's `inputLimits` field is populated
  by nobody; hi adds nothing until a real model needs it (YAGNI,
  recorded so the absence is a decision, not an oversight).

**Non-goal:** per-model limits, image protocols (#42), resize outside
`read`.

---

## AD-0010 — kitty graphics by one-shot emission; no re-render, no registry (2026-10-15)

**Status:** implemented (issue #42). Scope ruled on the thread:
kitty-protocol terminals only, text-note fallback everywhere else.

**The fit with the architecture decided everything.** pi-tui needs a
scrollback image registry (`registerKittyImageMetadata`), placement-only
re-emission, and viewport cropping because pi-tui re-renders its
viewport and must clip images line by line. hi's inline ratatui prints
history exactly once via `insert_before` into terminal-owned
scrollback — the image, once placed, is kept by the terminal the way
text is. The hardest third of pi's module is structurally unnecessary
here, and we do not build it.

**The decisions:**

- **Detection is env-sniffing, never terminal queries.** pi's exact
  matrix, kitty scope: kitty/Ghostty/WezTerm/Warp → kitty; tmux →
  none (passthrough unreliable); everything else → none.
  `HI_IMAGE_PROTOCOL=kitty|none` is the env's last word. Detection is
  a pure function over an env snapshot — testable without process-env
  races, read once per print batch.
- **Emission is one cell, not a widget.** The block builder returns
  `Segment`s; a kitty sequence rides as the symbol of a single buffer
  cell and overlays `rows` untouched cells below it. Verified in
  ratatui-crossterm's source that cursor positioning is cell-index
  based, so a 200 KB symbol cannot skew the layout. No
  `ratatui-image` dependency (v11 resolves, but brings chafa/sixel/
  negotiation and a query-based picker hi doesn't use — AD-0004).
- **`C=1` is mandatory.** By default kitty moves the cursor right and
  down after placing; ratatui's backend skips cursor moves between
  contiguous cells, so the next row's text would land where kitty
  left the cursor. Verified against the kitty spec, not assumed.
- **Protocol-exact encoder**: `a=T,f=100,q=2,C=1,c,r,i`, base64 in
  4096-byte chunks with `m=1`/`m=0`, control keys only on the first
  chunk — byte-tested against the spec's chunking rules, not
  transcribed from memory.
- **Pixel work stays in core** (`image_util::{dimensions, to_png}`):
  header-only dimension sniff; PNG passthrough only after a
  structural chunk-walk to IEND (a corrupt "PNG" sent blind would
  render as nothing instead of falling back honestly); other formats
  decode under the AD-0009 limits and re-encode. The TUI never takes
  the image dependency.
- **Sizing follows pi**, including upscaling small images to the
  budget and the distortion chooser that sometimes prefers the
  neighboring cell count. Caps: 80 columns × 24 rows.
- **Fallback is a text note, never nothing**: undecodable base64,
  corrupt bytes, or a terminal without the protocol all render
  `· image: <mime>, <size>`.

**Non-goals:** iTerm2 protocol (its encoder is a new `Protocol`
variant away), sixel, halfblocks/ASCII art, pasted attachments
(#41), animation.**

---

## AD-0011 — paste stages a reference; context entry stays `read`'s job (2026-10-15)

**Status:** implemented (issue #41). Ruled on the thread: design A
(reference staging, pi-parity) over design B (attachment chips that
reshape `Message::User`).

**The requirement:** a pasted image must never send itself. The design
that honors it with the smallest blast radius is pi's own: Ctrl-V
writes the clipboard image to `temp_dir()/hi-clipboard-*.png` and
inserts the **path as inert text** into the input. Enter is the
explicit send — and even then, the image reaches the model only when
`read` runs on that path, through every rail already built: the
`vision`/`images` gates, the resize ladder (AD-0009), honest
not-attached notes, and kitty rendering in the transcript (AD-0010).
A staged paste and an attached file are the same thing, by
construction.

**Rejected:** staged attachment chips with a `Message::User` reshape —
a core protocol change (serde back-compat, new user-image wire parts,
session JSONL bloated with base64 blobs, transcript rendering) to
duplicate semantics `read` already owns.

**The mechanics:**

- **Clipboard access is arboard, off-thread.** One crate covers
  macOS/Windows/X11/Wayland (with `wayland-data-control`), collapsing
  pi's wl-paste/xclip/PowerShell subprocess matrix. The read runs on a
  one-shot thread per press and reports through a channel the frame
  loop drains — arboard's X11 backend can stall, and the UI must
  never block on the clipboard. WSL's PowerShell fallback is a
  non-goal (WSLg bridges the clipboard).
- **arboard hands back raw RGBA**, never an encoded format — so core
  gained `image_util::from_rgba` (pixels → PNG, shape-checked in u64
  math). The only core touch; the TUI never encodes images itself.
- **Order is pi's**: image first, then text (a screenshot must not be
  shadowed by a stale X11 selection). Failure is a transient note in
  the composer's title, cleared by the next keypress — a failed paste
  is not an event, so it never enters the transcript or the session.
- **Bracketed paste** came free in the same PR and fixed a latent
  bug: without it, a terminal paste arrives as raw keystrokes and
  every newline submits the input mid-paste. Enabled at startup,
  `Event::Paste` appends verbatim (line endings normalized), disabled
  at teardown.
- **Paths collide with the command dispatcher** — found by test, not
  by review: `/tmp/hi-clipboard-….png` entered alone was eaten as an
  unknown `/tmp/...` command. The rule is now: a leading `/` dispatches
  a command only when the rest contains no `/` (command names never
  do; paths always do). Typo'd commands keep their teaching error.
- **Staged files are never auto-deleted** (pi leaves its too): OS
  temp hygiene owns cleanup, and the model may legitimately re-read
  the path later in the session.

**Non-goals:** Finder-style file-path pastes (pi's
`readClipboardFilePaths` needs a native clipboard API arboard does
not expose), attachment chips, pasted-image preview in the composer,
WSL PowerShell fallback.

---

## AD-0012 — image policy is fail-open, pi's direction; the gate follows model switches (2026-10-15)

**Status:** implemented. Supersedes the *default direction* of #29
(the "safe direction"): unknown models now read as vision-capable.

**The field report that overturned it:** models loaded dynamically via
`GET /models` — no `models.json` entry — matched the policy's `find`
nothing, so *every* discovered model was blind while pi attached
fine for the same endpoint. The pessimistic default assumed a curated
model list; with discovery, the list is the API's, and the safe
direction became "always off".

**pi's rule, verified in source** (`core/tools/read.js`):
`if (!model || model.input.includes("image"))` — no metadata at all
**attaches**; only metadata explicitly excluding image input withholds.
pi's own custom-model definitions default `input` to
`["text", "image"]`. hi now mirrors that at every layer:

- `vision` defaults **true** on configured entries; `vision: false`
  is the explicit opt-out (the note path survives for exactly that).
- No entry (API discovery) → attach.
- `settings.images` master switch still wins over everything.

**The gate follows the model.** The policy used to be baked into the
registry at startup; `/model` switches re-buildded the provider around
the same registry and silently kept the startup model's policy. The
policy now lives in a shared `ImageGate` (an `Arc<AtomicBool>` inside
`read`); `rebuild_provider` — the one construction path for `/model`
and `/resume` — re-resolves it per switch: master switch, then
configured `vision: false`, then fail-open. The registry never
rebuilds; the wire never loses its tools.

**Honest cost:** a text-only model that reads an image now sends an
`image_url` part it may reject — pi's exact exposure. The failure is
loud, one turn, and self-explanatory; versus the old cost, which was
silently blinding models that could see.

---

## AD-0013 — Cmd+V aliases the paste chord under the kitty keyboard protocol (2026-10-15)

**Status:** implemented. A deliberate hi-specific nicety — pi binds
Ctrl+V everywhere and stops there.

**The gap:** on macOS, Cmd+V is muscle memory, but a terminal owns it:
the emulator performs its own text paste and no key event reaches the
app, and clipboard pixels never cross a TTY — so image paste must be a
chord the app actually receives (Ctrl+V is byte `0x16`).

**The mechanism:** the kitty keyboard protocol's minimal enhancement,
`DISAMBIGUATE_ESCAPE_CODES`. The spec guarantees that under this flag
every key event that does not generate text — Cmd+V produces none —
arrives as a `CSI u` sequence carrying its modifier bits, so
supporting terminals (Ghostty, kitty, WezTerm) can forward the chord;
crossterm reports it as `Char('v') + SUPER` and hi aliases it to the
same paste path. Terminals without the protocol ignore the enable
sequence and nothing changes — Ctrl+V remains the paste everywhere.

**Scope discipline:** only the disambiguate flag is pushed — not
`REPORT_ALL_KEYS_AS_ESCAPE_CODES` (repeat/release noise, text
reporting changes) or alternate-keys. The spec's explicit exceptions
hold: Enter/Tab/Backspace keep their legacy bytes, plain typing is
untouched, Ctrl+C arrives as an event instead of SIGINT (raw mode had
already disabled the signal). Paired pop at teardown; the panic-hook
caveat from AD-0011 applies to both modes.

**Whether a given terminal actually hands Cmd+V to the app is its own
policy** (some keep Cmd+V bound to their text paste even under the
protocol). The feature degrades to today's behavior wherever that
happens — worst case is the status quo.

---

## AD-0014 — the composer becomes an editor; owned, not a crate (2026-10-15)

**Status:** implemented. Supersedes nothing; extends AD-0011's paste
and AD-0013's protocol work into a full editing surface.

**The gap:** the input was append-only — no cursor, no movement, no
word jumps, no history recall. Fix a typo in a long line and the only
tool was Backspace to the mistake and retype. Recall a cancelled
message and the only tool was memory.

**The ruling that shaped it:** pi's input can do all of this, and the
Rust ecosystem was checked before writing code. ratatui ships no input
widget (render-only by design). tui-textarea is a full editor but a
multi-line, self-rendering widget whose "history" is undo history —
the prompt-recall part is absent because it is app state, not editor
state. tui-input covers cursor movement — the thin, replaceable slice.
The valuable parts (history with a parked draft, the chord set,
dispatch interplay) are app-shaped regardless, and one chord —
Cmd+Left/Right — exists only because AD-0013 turned on the kitty
keyboard protocol; no crate can know that. pi reached the same verdict
with a 2,000-line editor of its own. AD-0004 (minimal deps) settled
it: `editor.rs`, ~150 owned lines, every rule unit-tested without an
`App`.

**The model:** flat `String` + byte-offset cursor on a `char`
boundary. The caret is the real terminal cursor, parked via
`wrap::cursor_pos` (row starts from the same greedy walk that renders
— one algorithm, no drift; a row-boundary cursor belongs to the
previous row's end). Word unit: alphanumeric + `_`, separators
skipped — pi's granularity, right for paths. History: every non-empty
submission (commands included — shell parity), cap 100, draft parked
on first ↑ and restored past the newest ↓; loaded entries load with
the cursor at the start so the first keystrokes fix the beginning —
the cancel-edit-resend workflow the user asked for.

**Named out of scope:** kill-ring yank, undo, jump-to-char,
Shift+Enter newline (crossterm cannot report it), page-scroll. Each is
a clean later PR.

---

## AD-0015 — the live viewport is the whole terminal (2026-10-15)

**Status:** implemented. Revises the first layout sketch, which fixed
the managed region at 12 rows and capped every streaming reply at ~8
lines on any screen — the transcript above stayed visible, but the
reply was a keyhole.

**The ruling:** pi's live viewport is the bottom `terminal.rows` lines
of its working buffer — the whole screen. hi already had pi's other
half (commit-on-completion to scrollback); only the viewport size was
wrong. The partition is now computed from the real height, bottom-up:
status 1 pinned, input (3–8) above it, menu at its natural height
above that, and the stream keeps every remaining row. The `min(9 −
input_height)` menu cap existed only to keep the stream ≥ 2 inside
the 12-row sketch and is gone; item caps in `menu_height()` already
bind.

**The trade, named:** while a reply streams, the scrollback above the
viewport is not visible — pi behaves identically. The completed block
commits to scrollback as always, so nothing is lost, only deferred.
`layout()` is a pure function over `(area, input_height, menu_h)`,
saturating by construction: on absurdly short terminals the stream
shrinks first, to nothing; nothing panics.

---

## Correction to AD-0015 (2026-10-15, recorded with AD-0016)

AD-0015's claim that "a reply fills the window" was **false as
shipped**. The inline viewport height is frozen at construction, and
`lib.rs` still created `Viewport::Inline(12)` — so the full-height
partition math in `draw()` never saw more than 12 rows at runtime.
The layout code was right and fed by the wrong geometry; the 12-row
sketch had simply moved from `draw()` into `lib.rs`. Discovered while
designing AD-0016; the viewport is now sized from the real terminal
at startup, and this note stands so the decision log never claims
more than the code did.

---

## AD-0016 — the scrollback is a flushed prefix; the live area is its window (2026-10-15)

**Status:** implemented. The blank band between the conversation and
the input — reserved since the first 12-row sketch for menus and
streaming — is gone by construction, not by sizing.

**The problem:** the managed region owned its rows whether or not it
had content. Idle, the stream area rendered nothing over the whole
space between the last committed block and the input — after AD-0015
the band was nearly the entire screen. Fullscreen mode was the
assumed remedy; it is also the wrong one.

**The architecture (pi's, verified in pi-tui's source):** the
scrollback is a flushed prefix of the transcript; the live area is
its window. `scroll.rs` caches each finalized block in both
representations — `block_segments` (scrollback truth, images real)
and `block_rows` (viewport truth, images dropped; the managed region
re-renders every frame and cannot host graphics). A flush cursor in
segment space advances row-wise through text and atomically through
images, pushing rows into the scrollback via the existing
`segments_into` path exactly once, in order; an image at the very end
over-flushes so it can never strand invisible. The window renders
`[flushed..total)`, hugging the input — the conversation flows
straight down to the composer, menus included. The old commit batch
(`printed` watermark, per-block `insert_before` at turn end) is gone:
folding happens incrementally per frame, and a finished reply simply
stops being live instead of teleporting into scrollback.

**The rulings folded in:** the viewport is the whole terminal (the
AD-0015 correction above); menus render UNDER the input, pi-style
(editor.js appends the list after the bottom border), taking the
bottom edge while the status row yields for their brief lifetime —
menus outrank status, user-ruled. A session switch refolds so the new
history replays below the old conversation. A resize re-wraps
unflushed blocks and snaps the cursor to the partial block's start:
at most one block re-prints at the seam, once — pi's behavior too.

**Named costs:** the inline height is frozen at startup (a mid-run
resize keeps it, clamped — the documented ratatui constraint); the
finalization of a reply adds its two padding rows with a one-row
settle, as pi's spacing does; a resize mid-block may re-print a
couple of seam rows once.


---

## AD-0017 — ratatui's `scrolling-regions` feature is on; the pty harness guards the real loop (2026-10-16)

`insert_before` ships in two modes: with the `scrolling-regions`
feature it scrolls with a `CSI r` region; without it, a fallback
redraws whole screens per flush. hi never enabled the feature, so
**every flush, in every terminal, ran the fallback** — worked, but
churned full-screen redraws per row. Worse, the fallback drops bytes
written into the terminal while it drains (the pty harness's `/exit`
keystrokes vanished mid-finalize; real terminals tolerate it, the
harness cannot). The feature is now on in `crates/tui/Cargo.toml`;
the flush path is the scrolling-region path everywhere.

The pty harness (`crates/cli/tests/tui_pty.rs`, dev-only
`portable-pty`) drives the real binary through a pseudo-terminal:
handshake queries answered (crossterm re-queries the cursor position
on every resize — one missed reply is a 2s stall then a boot error),
typed keys asserted in the escape-stripped frame stream, and a clean
`/exit` asserted within a timeout — the 50ms poll clock and teardown
ordering are exactly what a mock event source cannot reach. The
harness kills its child on drop: a panicking assert must not leave an
orphaned `hi` burning a core. Three pty facts are load-bearing for
anyone extending it: the pty needs an explicit size (crossterm
renders nothing into a 0×0 terminal), keys typed before raw mode is
on are lost to the cooked-mode line discipline (wait for a rendered
frame first), and needles must be space-free (a space cell is
byte-identical to the empty background cell, so the frame diff never
emits it).

---

## Correction to AD-0017 (2026-10-16, the same day): the feature is off again

AD-0017 shipped `scrolling-regions` on the strength of a pty harness that
matched needles in a byte stream — which cannot see the glass. A real
terminal-emulator rig (the harness now renders through `avt`, asciinema's
emulator, and asserts on frames) reproduced the report that motivated the
revert: **the input box shredded, backgrounds smeared, and the flushed
rows reached scrollback as blanks** — data loss, not just churn. The
steal path draws into screen row 0 and scrolls it away per row while
ratatui's buffer model keeps believing the viewport unchanged, so the
next diff skips every cell it thinks is untouched and the glass never
converges; the flushed rows it hands the terminal are not the rows the
scrollback receives. The feature is back off; the fallback
(`insert_before_no_scrolling_regions`) repaints through a full
`clear()` and converges — the behavior that held up in real terminals.

The rig also caught what the byte-stream harness had been sleeping
around: the fallback's `Terminal::clear` snapshots the cursor position
before clearing, and crossterm's position query drains the input queue
on the way — **keys typed during a flush's finalize window are eaten**.
A human rarely types inside that window; a test harness must wait out
the settle before typing, and the product owes either an upstream fix
(ratatui's full-screen steal branch, the cleared-viewport cursor query)
or hi-side handling. Until then the churn stays: correctness over the
per-row redraw cost.

---

## AD-0018 — the composer goes multi-line; the newline chords are the capability (2026-10-16)

Shift+Enter inserts a newline; Enter submits — pi's daily-driver contract,
now hi's. The design point: **the wire encoding is the capability, not the
handshake**. A kitty-capable terminal reports Shift+Enter as `CSI 13;2u`
(crossterm hands us `Enter` + `SHIFT`); a literal LF arrives where a
terminal binds shift+enter to text; and Ctrl+J — pi's second binding —
works in every terminal ever made. No kitty flag management, no
handshake gating: whichever encoding shows up proves the terminal can
send it. The chords outrank the `/` menu (a newline is an edit, not a
menu pick) but not the pickers, and Enter keeps one escape hatch from pi:
a backslash right before Enter yields the newline instead of a
submission, for terminals with none of the chords.

Multi-line input forced the composer's rendering honest: `wrap` breaks on
newlines first (every line earns at least one row, blank lines visible),
byte spans stay a contiguous partition with the newline riding at the end
of the row that closed the line, and a caret just after a newline belongs
to the row that *opens* there — the inverse of the wrap-boundary rule.
Arrows walk visual rows column-preserving, pi-style, falling through to
history at the first/last visual row's edges (the single-line contract —
history from anywhere — is untouched); Home/End/Ctrl+A/E and Ctrl+U/K
scope to the logical line; word motions treat the newline as a separator.
The composer's six-text-row height cap stands; a longer draft still
clamps (recorded limit, not new behavior).

---

## AD-0019 — background tasks: threads, process groups, completion-as-fact (2026-10-16)

The harness can hold work that outlives a turn: a `TaskManager` in core
(#5), four `task_*` tools on top. The rulings the implementation encodes:

- **Threads, not tokio.** One worker thread per task polls its child at
  50ms — the bash tool's pattern generalized over a registry. The
  project's no-async-runtime stance holds; the Luau bridge (#6) inherits
  threads as its bridge model.
- **The process group is the kill unit.** Tasks spawn in their own group
  (Unix `process_group(0)`), and cancel, timeout, and shutdown kill the
  *group* — grandchildren (a dev server's watchers) die with the task.
  This is deliberately stronger than the `bash` tool, whose #30
  grandchild limitation stands for `bash` itself.
- **One merged pipe.** The child runs under `bash -c "exec 2>&1; …"`,
  so stdout and stderr interleave in one totally ordered stream — the
  terminal model, not the diagnostic model (that's `bash`'s labeled
  capture, unchanged).
- **Two-tier buffering.** A 256 KiB ring backs instant tail reads for
  the model; the full stream lands in a log file for post-mortems.
  Reads are bounded by the one truncation contract.
- **Completion is a fact, not a trigger.** A finish sends one
  `TaskCompletion` into a channel the front end drains per frame; it
  becomes a dim `System` note in the transcript and rides the next
  turn's payload. Nothing auto-starts a turn — that is an orchestrator
  behavior and belongs to #4 (subagents). This is the deliberate
  divergence from pi's `triggerOnCompletion`.
- **Tasks are runtime objects.** Spawn echoes persist as ordinary
  transcript messages; running state is never resurrected on resume;
  dropping the last manager handle kills every running group, so a
  clean `/exit` orphans nothing. A crash can orphan — the OS is the
  boundary, the same trust stance as every tool.
- **Idle defaults, explicit bounds.** No default timeout (daemons are
  the point; `timeout` is opt-in per task), a cap of 16 running tasks
  with a readable refusal, small monotonic ids (`#1`) the model can
  echo reliably, and a derived two-word name when none is given.

---

## AD-0020 — the composer breathes: one margin row above the input (2026-10-16)

**Status:** implemented. AD-0016 killed the *reserved* blank band
between conversation and input; this ruling gives back exactly one row,
deliberately, as part of the input's own footprint: a message should
not sit flush against the composer's top rule.

- **The margin is the input's padding, not a layout band.**
  `input_height` = wrapped rows + 2 rules + **1 breathing row** — the
  one formula draw and the flush pass share, so the stream window, the
  scrollback flush target, and the box move together. No consumer
  re-derives the gap.
- **Owned and cleared, not merely outside the stream area.** Ratatui
  only repaints what widgets draw; a row no widget claims would keep
  stale transcript cells as content scrolled through. `draw_input`
  renders `Clear` over the footprint's first row, then draws the box
  one row down — the margin can never show ghosts.
- **Menus and the status row are unaffected.** The menu area anchors to
  `input_area.y + input_height` (the box's rules, not the margin), and
  the caret's inner-text position is invariant: the +1 footprint and
  the +1 draw offset cancel.

---

## AD-0021 — the tasks pane lives in the menu slot and reads live (2026-10-16)

The model got `task_status`/`task_output` with #5; the human got a
counter. `/tasks` closes that gap: bare, a SelectList of every task
(`#id name — state · wall`); `/tasks <id>` (or `#id`, the listing's own
spelling) opens one task's output directly.

- **The menu slot, not a takeover.** The pane renders where the model
  picker and the `/` menu live — under the input, status yields, the
  session continues underneath. Esc backs out (output → list →
  closed); arrows walk; Enter opens. The pane owns the keyboard while
  open, like every menu.
- **Draw reads live.** The frame already redraws ~20 times a second;
  the pane re-reads `status()` and `output_tail()` on every frame —
  the frame clock is the poll. No cache to invalidate, no thread, no
  subscription: watching a running task is just keeping the pane open.
- **Honest edges.** `--mock` sessions (no manager) and unknown ids get
  transcript notices, never an empty pane. Output bottom-anchors —
  newest rows at the bottom, head-trimmed to the six rows that fit —
  and the header carries the same state phrase and wall phrasing the
  task tools use (`format_duration` moved to core's `tasks` module so
  every consumer spells time identically).
- **The arg seam is general.** Dispatch splits a command line on the
  first space; `/tasks` is merely its first consumer. Future commands
  take arguments without touching the submit path again.

---

## AD-0022 — task completions may start a turn: opt-in, armed, budgeted (2026-10-16)

AD-0019 R6 ruled completion a fact, not a trigger: the note rides the
next turn, and only the human spends one. #72 keeps that default and
sells the exception: `settings.taskTurns` (default off) lets an
unattended flow — run the suite, then summarize — finish without a
keystroke.

- **The human owns the decision, once.** One flat settings key,
  resolved at startup like `images`. No per-task `notify` in v1: the
  model must never opt a session into turn-spending. No `--auto` flag:
  two surfaces for one boolean is double the docs and tests.
- **Core resolves, the front end triggers.** Core carries the setting;
  the TUI's `pump` implements the trigger, keyed on the completion
  drain's count — never on generic transcript growth, so a command
  notice cannot spend a turn. Headless ignores it (one prompt, one
  exit).
- **Idle fires; in-flight arms.** Notes folding while idle (and
  opted-in, and budgeted) start a mandate turn immediately. Notes
  folding mid-turn only set an arm; when the turn ends, a queued
  human message fires and consumes the arm — it already carries the
  notes, the human's instruction dominates, no budget spent —
  otherwise the mandate fires. The one-turn invariant never breaks,
  and the `queued` slot stays human-only.
- **The mandate is marked and the budget is honest.** The trigger
  speaks as `(auto-trigger) a background task finished — act on the
  notes above as warranted`: honest in the session file, and the
  transcript ends user-role for strict providers. The budget is a
  constant 10 per session, bounding completion→turn→spawn→completion
  chains by construction; the last spend pushes a dim note explaining
  the silence that follows.
- **#4 is the consumer, not the prerequisite.** A future `Agent` task
  kind wants completion-triggered continuation as its core loop; this
  hook is the seam it will register against, landed small and proven
  first.

---

## AD-0023 — `/settings`: a curated board, not an editor (2026-10-16)

#72's review surfaced the real friction: not default-off, but the cost
of consent — a JSON trip plus a restart. `/settings` makes consent two
keystrokes, which is what lets conservative defaults stay honest.

- **A row exists only when the session can flip it.** The board is
  curated by hand (`SETTINGS_TOGGLES`): `task turns` (live on the next
  pump) and `images` (re-resolved through AD-0012's one construction
  path, so the gate follows immediately; mock providers early-return —
  honest no-op). Startup-stamped settings stay off the board until
  they declare a setter path — absence, not a grayed-out row: the
  board shows only what Enter can do.
- **The `/model` grammar, one divergence.** Arrows walk (wrapping),
  Esc closes, Ctrl-S persists the highlighted row, Enter toggles —
  and keeps the board open. A chooser closes on Enter; a board is a
  place you watch the row flip and act again.
- **Core persists, the board toggles.** `persist_task_turns` /
  `persist_images` share one `persist_with` — load, mutate one field,
  save (#45's shape, generalized). An unparseable file is refused
  untouched and surfaced as a transcript error. The app-level tests
  own the toggle behavior; core's path-independent `_to` halves own
  the file contract — an app test would write the developer's real
  config, the same seam split `/model` has had since #45.
- **Defaults stay conservative.** The board changes the cost of
  consent, never the default. #4's subagents will register their own
  policy here when they land.

---

## AD-0024 — live == flushed: the streaming reply renders as its block (2026-10-16)

The live region rendered the in-flight `turn.text` raw — full width,
no paddingY, no paddingX — while the flushed assistant block carries
pi's block padding. Two defects in one: during streaming the text
hugged the terminal edge with no separation above it, and at TurnEnd
the padded, re-wrapped block replaced it — a visible reflow jump.

The rule: **the live region renders byte-identically to what the
flush will print.** Same `pad_plain`, same `width − 2` wrap, same
blank rows, for the streaming reply and its error twin. The pending
tool block already complied (`block_rows`); the flush becomes a
visual no-op, which is the test's assertion shape — padding present
mid-stream, not merely correct history after.

Found in review (#80 follow-up); fixed in ui.rs only — no app, core,
or flush-mechanics change. The pty suite ran 5× per the
flush-adjacent-change rule.

---

## AD-0025 — the status board: rows are earned, never reserved (2026-10-16)

The status bar was one hardcoded row with left/right strings and
bolt-ons (`· thinking:high`, `· N bg` crammed onto the model label).
Extensions on the roadmap (#4 subagents, #6 Luau host) need somewhere
honest to surface state. The board:

- **Height follows content.** `status_rows()` returns only non-empty
  rows — a quiet session renders exactly the classic one-row bar
  (byte-identical; the pty suite is untouched). No fixed count, no
  reserved blank rows: absence, the same principle as AD-0023's
  board.
- **One shared formula.** `status_height()` feeds `layout()` and
  `stream_window_height()` — the flush pass and draw measure the same
  board, so they can never disagree (the `input_height` pattern,
  extended).
- **Menus still outrank status.** The whole board yields when a menu
  is open — the previously ruled yield, generalized from row to
  board.
- **Rows come from facts on `App`, not strings on the side.** Row 0
  is location | model(+thinking). Row 1 is the session's own work:
  tasks (`tasks: 2 running · 1 done`, which replaces the `· N bg`
  bolt-on) and the auto-trigger arm (`auto: armed` — #72's trigger
  finally has visible feedback). #4/#6 register here when they land.

---

## AD-0025a — amendment: the model is a resident, not an earned row (2026-10-16)

AD-0025 shipped with the model on row 0 and "a quiet session renders
exactly today's one row." One session with the board made that wrong:
the model indicator is permanent session context, not an earned
event — it belongs on the context row, next to the tasks and the arm
it governs.

The amendment (append-only, per house rule): the board has **two
permanent residents**. Row 0 is location on the left, its right held
open for future residents; row 1 carries the model (+thinking) on
the right — tasks on the left, the auto-trigger arm appended after
the model, transient noise riding right. "Height follows content"
still holds; the content now always includes row 1. The cost is one
terminal row in every session — the thing AD-0025's ruling 1
originally refused — spent knowingly, with three total rows the
declared ceiling.
