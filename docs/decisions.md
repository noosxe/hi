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

---

## AD-0026 — the bill on the board: usage facts ride the final chunk (2026-10-16)

#39's premise made most of pi's footer stats deliverable from the wire
alone. Every streaming request now asks for the bill
(`stream_options: {"include_usage": true}` — part of the compat
contract; a server that rejects it is a per-provider bug, not a
knob), and the final `usage` chunk surfaces as `Event::Usage` — raw
counts in core (`prompt_tokens`, `completion_tokens`, optional
`cached_tokens`/`reasoning_tokens`; absent detail means *not
reported*, never zero). Wire order is kept: content, tools, bill,
`[DONE]` — one bill per round, last-wins, so a chatty server cannot
double count. The exhaustive enum made every consumer adapt: the
agent loop forwards untouched, the TUI accumulates, headless
declines politely.

**Display** is row 0's right section — the resident the board held
empty since the model moved out (AD-0025a): `↑8.6M ↓1.9M R250M
CH99.2% 10.7%/1M`. Rulings: ↑↓R/CH are session-cumulative, the
context share is the *latest* round's prompt against the configured
`contextWindow` (a point-in-time fact, not a sum); ↓ is input net of
cache when cache is reported; every segment hides when its input is
absent — no detail object, no CH/R; no `contextWindow`, no share,
**nothing guessed** (pi guesses 1M; hi reads the config the user
already wrote). Compact formatting (`1.9M`, trims `.0`) is
presentation and lives in the tui. *Amended
same-day, on sight: the bill moved to the board's top-left corner
(row 1's left) and the background-task counts moved under the model
name (row 0's right) — the work sits beneath what it runs against.* Cost stays a non-goal (#39's own
carried ruling — a pricing table, not a wire fact). The mock emits a
fixed bill (↑250 ↓300 R100 CH70.0%), so tests and the pty assert the
rendered line exactly.

---

## AD-0027 — the bill survives the restart (2026-10-16)

#85 closed the gap #84 left: the usage line lived in `App` memory, so a
restart reset ↑↓R/CH to zero and `/resume` restored the transcript but
not the bill. The ruling is the session file's own shape applied once
more — **persist the per-round facts, re-derive the totals on read**.
Every round appends one `{"type":"usage"}` record beside the messages it
bills; replay merges them with the exact `Usage::merge` semantics and
takes the last record's prompt size as the context fact. No running
total is ever stored. The refinements the wire made cheap: the context
share survives resume too (the last recorded prompt IS a persisted
fact — nothing to guess), and a **fork inherits the transcript, never
the economics** — a forked session's board reads zero until it spends.
Headless prints no bill but records one; the exhaustive rule holds —
every front end that consumes `Event::Usage` also persists it. Old
session files load untouched (zero bill); new records are a new line
kind, the same forward-compat seam messages already use.

---

## AD-0028 — the ring is a derivation of the transcript (2026-10-16)

#87 closed the last in-memory-only pocket: the prompt ring died with
the process while the words it held sat in the session file. The
ruling repeats #85's shape one more time — **re-derive at the seam,
store nothing new**. `App` seeds `history` from the transcript's
`Message::User` lines (startup, fork, in-app switch alike), oldest
first, the ceiling holding on seed: the disk may hold more than the
ring ever shows. Commands stay live-ring-only (they never become
transcript lines — a reconstructed ring holds prompts, matching pi).
Drafts and browse position stay session-local; the ring seeds,
nothing else. A fork inherits the transcript — and the ring is
transcript-shaped — so its ↑ walks the forked words; the economics
alone stay fork-reset (#85).

---

## AD-0029 — the words lead, and names are appended (2026-10-16)

#89 started as a layout complaint and turned into the session's
identity ruling. **The picker row leads with the words that recognize
the session** — name, else first prompt line, else the id prefix —
and the id demotes to the dim right edge, where it survives any width
(the words truncate first, ten-column floor): it is the `--session`
key, and a key you cannot see is a key you cannot type. Verified
against pi before writing: pi generates nothing — its row is
`session.name ?? session.firstMessage`, and its naming is manual and
**append-only** (`session_info` records, last-wins, empty clears).
hi repeats that shape on its own seam: `{"type":"name"}` records ride
the same forward-compat file as messages and usage; `Session::rename`
sanitizes to one line (pi's normalization), appends, never rewrites;
`/name` names the live session, `ctrl-r` in the picker names the
highlighted file, and both refuse to write an empty name — clearing is
naming it something else. The summary pass scans past the header once
per listing (pi rescans every frame; hi doesn't need to). No model
involvement anywhere — titles are human words, and unnamed sessions
speak through their own first prompt.

---

## AD-0030 — ctrl-c is the line-killer, not the app-killer (2026-10-16)

#91: ctrl-c quit unconditionally, from anywhere — one reflex with a
draft in the box killed the whole session to kill one line. The
ruling is the shell's: a non-empty draft dies first (cleared, cursor
home); an empty composer is the user asking to leave, and quits.
Menus own no text, so from a picker or pane the anywhere-exit
survives everywhere ctrl-c can't mean "clear".

---

## AD-0031 — undo is snapshots on the app, not a crate (2026-10-16)

#65: the composer had kills and history but no undo — one Ctrl+Z too
many on a long prompt and the text was gone for good. The ruling is
the issue's own first design note, taken at face value: for a
few-KB box, **snapshot-per-unit beats a delta log** — the whole
`(input, cursor)` state before each edit, capped at 100 units, a few
hundred KB worst case. Undo is app policy, so it lives on `App` and
`editor.rs` stays pure (the AD-0014 seam, again): every mutation
flows through one choke — `apply_edit` for edits, `apply_insert` for
plain chars — and even ctrl+c's line-kill and a Tab-accepted
completion are units. **Insert runs coalesce by state, not flags**:
a plain insert merges into the open run only while the cursor sits
exactly where the run left off; any other key or cursor move opens a
fresh unit, and deletes never coalesce — emacs' self-insert rule,
without bookkeeping. **Ctrl+Z / Alt+Z** (the issue's emacs-style
redo pick; shift+ctrl+z is indistinguishable on legacy terminals),
both silent noops on empty stacks. History browse and queue handback
are navigation, not edits — their `set_input` never snapshots, so
undo never drags navigation with it. Redo is the states undo popped;
a fresh edit clears the future, and redo pushes its state back as a
closed unit — snapshot must not clear the stack it just popped from,
or one Alt+Z strands the rest.

---

## AD-0032 — the kill ring is the composer's memory of kills (2026-10-16)

#64: #52 shipped the kill set, but every kill was a dead end — emacs
answers with the kill ring, so hi grows one: killed text accumulates
in a ring capped at 16 (`KILL_CAP`; emacs keeps 60, sixteen is
plenty for moving paragraphs around a prompt), **Ctrl+Y** yanks the
newest kill, **Alt+Y** cycles the in-flight yank to older entries,
wrapping past the oldest. Clipboard stays **Ctrl+V** — Ctrl+Y was
free. All seven kill chords (Ctrl+K/U/W/D, Alt+D, Alt+Backspace,
Alt+Delete) feed the ring through one `kill_range` choke; plain
Backspace/Delete are deletions, not kills. **Consecutive kills
coalesce by the #65 rule** — a kill joins the previous entry only
while its range abuts the one before (forward appends, backward
prepends, reading order preserved); any non-kill edit (`apply_edit`,
`apply_insert`) ends the run, so kills separated by typing stay
separate. No region semantics — hi has no mark; only chords kill.
The ring rides AD-0027/0031 seams: entries are plain strings, yank
and yank-pop are their own undo units via `apply_edit`, and the
in-flight yank site (`yank_site`) dies with any other keypress —
captured before dispatch, because the Alt+Y arm is the one key that
still needs it.

---

## AD-0033 — the keybind table is held to the handler by a scan (2026-10-16)

#95: the chord vocabulary had grown to twenty-two modified keys
scattered across match arms, and only docs/tui.md answered "what
does Alt+D do again?". `/keybinds` now prints the answer — the
`keybinds::KEYBINDS` table (chord, one-line description), pushed as
the same kind of local-only notice `/tools` uses: persists in the
session file, never reaches the provider. **The table is the doc,
the test is the enforcement**: `every_bound_chord_has_a_row_and_
every_row_is_bound` scans `app.rs` source (baked in via
`include_str!`) for modified-chord arms — bare `'x' =>` arms of the
ctrl match, gated `KeyCode::Char` arms with their modifiers, and the
alt-backspace/delete pairs via two-line lookahead — and fails on
either drift direction: a bound chord without a row, or a row for a
chord nothing binds. Cmd+V folds into the Ctrl+V row's description;
plain unmodified keys (menus, arrows) stay out of scope — the table
speaks chords. Adding a binding is now: the arm in the handler, one
row in the table, or the build tells you which you forgot.

On macOS the chord column reads like the menu bar: `icon_label` maps
the modifier words to ⌃/⌥ (⌘ for any future cmd chord) and the named
editing keys to ⌫/⌦, icon then space then key — `⌃ S`, `⌥ D`. The
table stays canonical: `Ctrl+Y` labels are what the drift scan
checks; the icons are display, platform-gated at render time, and
the mapping is a pure function so a test pins mac and word forms on
any host. Ruled mid-review on #97: the user asked for mac icons; the
named-key glyphs ride along because a half-icon column reads worse
than either extreme.

---

## AD-0034 — /new rides the startup seam out and the resume seam in (2026-10-16)

#98: `/resume` walks back, but starting over meant quitting and
relaunching. `/new [name]` creates a session file through the same
`Session::start` startup path — header naming the cwd hi runs in —
then walks to it through `adopt`, the reset half of resume factored
out so `/resume` and `/new` cannot drift: fresh transcript, bill
replay, ring seed, cache key, provider rebuild, queue clear.
Mid-turn it refuses with the picker's exact wording — the same
two-sessions'-events-interleaving hazard. The model and thinking
level carry across: they are status, not session state, which is
why the header of a `/new` session can differ in provider from its
predecessor and that is honest. An optional argument names the
session (`/new scratch`); bare, and words-lead naming (#89) takes
over at listing time.

---

## AD-0035 — jump-to-char is a pending mode that eats one key; page-scroll waits for a viewport (2026-10-16)

#67's two halves split on feasibility. The composer half ships:
`Ctrl+]` arms a forward jump, `Ctrl+Alt+]` a backward one (pi's
chords, free in hi), and the pending mode eats exactly one key — a
printable character is the target, the jump chord itself cancels,
Enter cancels *without submitting* (pi lets Enter fall through; hi's
Enter commits, so a stray chord pair must not send a half-written
draft), everything else falls through with the mode consumed. The
search rule mirrors pi exactly: forward scans strictly after the
insertion cursor (`next_boundary` then `find`), so repeated jumps
step through occurrences and die at none; backward `rfind` counts
the character immediately behind the cursor. The fns are pure in
`editor.rs`; only `jump_pending: Option<Jump>` lives on the App.
Legacy terminals send Ctrl+] as the ESC byte — the chord lives under
the kitty protocol, pi's same trade.

The transcript half could not ship honestly: hi is pure inline mode
(AD-0015/16), the terminal owns the scrollback, and no in-process
viewport exists to page — #67's "scroller steps line-wise" described
the terminal emulator. Split to #100, where the chords get re-ruled
(#67's Ctrl+V is paste here); `Ctrl+Alt+]` folds into `Ctrl+]'s`
/keybinds row by the Cmd+V precedent (AD-0033), and the drift scan
learned to see `']'`.

---

## AD-0036 — one process-group module, both killers (2026-10-16)

#30 stopped being a scoping exercise: `own_group` / `kill_group` /
`stop_tree` lifted out of `tasks.rs` into `core/src/process.rs`, and
the bash tool now spawns into its own group and kills the tree at
timeout with the manager's exact TERM → grace → KILL. Before the
lift, a timed-out `bash` command left grandchildren running *and*
held the stdout pipe, so the tool blocked until the orphan exited —
the red test proved both at once (a backgrounded `sleep 30` kept
`execute` alive the full 30s). The liveness test polls
`libc::kill(pid, 0)` for ESRCH on a grandchild that announced its
own pid, the same proof shape the manager's tests use. Windows is
unchanged until #31 (Job Objects): `own_group` and `kill_group`
carry the `#[cfg]` halves, `stop_tree` keeps the manager's shape.

---

## AD-0037 — SHA-pin the actions, keep the toolchain branch moving, let dependabot drive both (#24, 2026-10-16)

Action tags are mutable; the CI supply chain is not. The three
third-party actions (`actions/checkout`, `Swatinem/rust-cache`,
`rustsec/audit-check`) are pinned to full commit SHAs with a
`# vX.Y.Z` comment — dependabot's recognized format, so its weekly
bump refreshes the human-readable version alongside the SHA.
`dtolnay/rust-toolchain@stable` deliberately stays a moving branch
ref: it is a branch, not a version tag, and the compiler channel is
already pinned by `rust-toolchain.toml` / `flake.nix` — freezing the
action's branch logic would buy immutability in the one place the
repo pins by file instead. `.github/dependabot.yml` opens one
grouped PR per ecosystem weekly (`cargo` and `github-actions`),
ignores Rust semver-major bumps (majors land by ruling, not by bot),
and prefixes commits `chore:` / `ci:` — conventional-commit types in
the house's `feat:`/`fix:` voice.

**AD-0037 addendum (2026-10-16):** the first cut of the config used
`dependency-type: "all"` to make the actions group catch-everything —
invalid: `groups.dependency-type` exists only for bundler/composer/
mix/maven/npm/pip and takes only `production`/`development`. The
default already includes all dependency types, so the group carries
`applies-to` alone; the `cargo` group's plain `"minor"`/`"patch"`
selector wording is the documented form for `groups.update-types`
(the `version-update:semver-*` prefixed form belongs to
`allow`/`ignore` only).

**AD-0037 addendum 2 (2026-10-16):** the selector-less group was
still wrong — dependabot's runtime schema requires every group to
match a selector-bearing form (`anyOf` over patterns /
exclude-patterns / dependency-type / update-types); `applies-to` is
metadata, not a selector. The actions group now uses the documented
catch-all, `patterns: ["*"]`. SchemaStore's `dependabot-2.0.json`
(ajv-validated locally) catches key and enum mistakes but is looser
than the runtime here — it accepted the selector-less group — so the
group shape follows the docs' examples, with the schema check as the
second gate, not the first.

**AD-0039 — auto-print on a redirected stream (2026-10-18, #19):**
the TUI opens only when BOTH stdin and stdout are a terminal; any
redirect with no explicit mode (`--print`/`--mode`) auto-selects text
print mode, pi-parity. Rulings: (a) either stream counts — a TUI
drawn into a redirected stdout is garbage, and crossterm cannot drive
a piped stdin; (b) a redirected stream with no prompt anywhere still
fails with the honest `hi: no prompt …` — never a TUI attempt on a
pipe; (c) `--mode json` stays explicit — silent auto-JSON on a pipe
would surprise. Explicit flags always win; the gate is a gap-filler,
not a takeover — `std::io::IsTerminal` only, zero new deps, core
untouched.

---

## AD-0040 — Subagents: a scoped loop, identity stamps, no recursion (2026-10-18)

#4's runner is a scoping function over `spawn_agent_turn`, never a second
implementation. The rulings:

- **History is built, not taken.** A subagent starts from
  `System(subagent prompt)` plus the task as its single `User` message;
  the parent transcript never leaks in. What crosses back is a shaped
  `SubagentResult` — final text, the run's own usage, round count —
  never raw events.
- **The slot stamps identity, not Arcs.** `stamp(provider, model)` records
  names; every run resolves its own provider from config and wires it to
  the subagent's own registry. Reusing the parent's provider `Arc` would
  put the parent's toolset on the wire. The front end restamps on every
  provider rebuild — `/model`, `/resume` — the telemetry-stamp pattern.
- **Recursion is impossible by construction.** The nested registry is
  built with `subagent: None` (and `tasks: None` — background-task
  ownership stays with the main session), so there is no path to a nested
  `subagent` call and no depth counter exists to enforce.
- **Sequential, bounded.** One tool call runs one subagent to completion;
  parallelism stays task_run territory (#5). The round cap defaults to 8
  — half the main loop's belt — and hits as a loud error.
- **`coding` and `all`, per ruling.** The maintainer overrode the
  conservative design draft: subagents ship in the default bundle. The
  name is `subagent`.
- **No transcript persistence in v1.** The result rides the caller's
  transcript as an ordinary tool result (spawn-echo precedent);
  recording subagent runs as their own session files is a filed
  follow-up. Cancellation needs no new primitive — the abandoned
  subagent self-stops at its own next send (turn-lifecycle.md).

**AD-0040 addendum (2026-10-18, field feedback):** the first hi-on-hi
audit sharpened three edges without amending a ruling. Cap failures are
now model-actionable — the runner recognizes its own loop's
`ROUND_CAP_MARKER` and answers with the cap fact, a retry hint, and the
subagent's last narration (the main loop's human-facing message is
untouched). The default cap stays 8: schema guidance now documents the
ranges (20–40 for repo-wide audits) so the model raises it itself —
field evidence that an audit wanted 30 is real, but guidance-raises-it
keeps the belt. And a soft `HIGH TOKEN USE` marker at
`SUBAGENT_HIGH_TOKEN_USE` (100k cumulative input) makes cost outliers
visible; hard mid-run budgets stay out of scope — input re-sends every
round, so the real lever is round count, which the caller already sets.

**AD-0041 (2026-10-19): the main-loop round cap becomes configurable,
off by default.** Ecosystem check (pi: no counter anywhere — the
watching human is the bound; OpenAI Agents SDK: `max_turns` on by
default with a remedy-bearing handler; Claude Agent SDK: `max_turns`
opt-in + `max_budget_usd`; smolagents: `max_steps` default 20; SWE-agent
limits configurable, none in the shipped default) says caps are the
norm for anything that runs unattended, and hi can be left unattended —
the existing loud 16 cap stays the default. `settings.maxToolRounds`
(absent = 16, must be ≥ 1, rejected at startup otherwise) raises or
lowers it, resolved once in `resolve_target` so TUI and headless agree.
The subagent's own cap is NOT this knob — it stays the tool call's
`max_rounds` (default 8): a parent turning the main cap up should not
silently uncap nested runs. Startup policy only, not `/settings`-flippable
(same rule as `sessionRetention`).

**AD-0042 (2026-10-19): optional subagent transcript persistence (#116).**
Deferred by AD-0040, ruled in-thread after field use made token
post-mortems (the 280k question) a real need. Rulings: opt-in via
`settings.subagentTranscripts`, false by default — a chatty parent
turn can spawn many files, and sessions are opt-in history, not
runtime state. Granularity is full per-round recording: the runner
mirrors the loop's own working history event by event (the same
reconstruction rule consumers already follow), one `usage` line per
round, so `load()` reproduces exactly what the subagent saw and
`LoadedSession.usage` is the post-mortem total for free. No
special-casing anywhere: a recorded run is an ordinary session file
(`kind: "subagent"`, parent id in the header — serde-default both
ways), inspectable and resumable via the existing surfaces, pruned by
the same retention rules; the parent transcript stays spawn-echo.
Recording is best-effort — a full disk never fails a run. The runner
restamps a parent slot wherever the identity is restamped (session
open, `/resume`), so lineage follows the live session.

**AD-0042 addendum (2026-10-19): #125, recordings say where they are.**
First field use of recording (the hi-on-hi battery) validated the
feature and exposed a discoverability gap: the parent agent hunted
for the artifacts, guessed a wrong directory, and initially concluded
no recordings happen — because the result line said nothing. Result
lines now append `recorded → <path>` when a transcript was written;
open failures stay silent (best-effort contract) and recording-off
stays byte-identical. Field evidence also recorded here: cap
under-provisioning cost ~148k where one well-sized run would spend
~80k, and the parent session out-burned its subagents (260k vs 190k)
— both fuel for #121's trimming design.

**AD-0043 (2026-10-19): `subagents` master switch.** The nested-agent
tool ships as a default capability: `settings.subagents`, absent =
on (sibling of `images`, opposite default of `taskTurns`). Rulings:
(1) camelCase `subagents`, default on; (2) off removes the tool from
the registry entirely — absent beats erroring, since a schema entry
that can only be rejected wastes context and invites retries; (3)
startup policy only, no `/settings` toggle — the registry is built
once in `resolve_target` and the TUI never rebuilds it, so no
SessionInit plumbing exists or is needed (unlike `images`, whose
AD-0012 per-message consent semantics earn a runtime switch).
Composes by omission: off also means no recordings (nothing runs).

**AD-0044 (2026-10-19): model metadata tiers + the thinking ladder.**
Research across provider docs (OpenAI, Anthropic, Gemini, xAI, z.ai)
and pi's implementation: effort vocabularies are model-dependent
(OpenAI runs `none…max`, Gemini subsets of `minimal…high`, z.ai is a
binary switch), provider defaults vary wildly when nothing is sent,
and — the decisive negative — **no provider returns a machine-readable
level list on error**; wrong values are prose 400s. pi's answer is
knowledge before the call: a generated catalog plus pre-flight
clamping. hi adopts the same shape, config-first:

1. **Ladder of seven**: `off | minimal | low | medium | high | xhigh |
   max` — pi's and OpenAI's ladder exactly. Vendors map what they map.
2. **Tier chain for metadata** (context window, max output, efforts,
   pricing, vision): explicit `models.json` fields win; then live
   OpenRouter metadata *when the selected provider is OpenRouter*
   (best-effort, 3s, silent on failure); then the shipped snapshot
   (`catalog.json`, regenerated per release by
   `scripts/gen-catalog.py`); then nothing. AD-0026's "nothing is
   guessed" now reads "nothing is guessed beyond vendor metadata" —
   a context share from the vendor's published number is a fact, not
   a guess.
3. **URL-family default kwargs** (`thinking::family_kwargs`): z.ai →
   `thinking.type` binary, OpenAI → `reasoning_effort`, OpenRouter →
   `reasoning.effort`. Config map entries override; unknown hosts send
   nothing. This is what makes `/thinking` work on unconfigured models.
4. **`/thinking [level|auto]`**: bare lists the ladder with per-model
   marks (✓ reaches the wire via map, family, or vendor efforts; –
   would send nothing); a level switches mid-session, rebuilds the
   provider, and persists to settings.json (taskTurns pattern); `auto`
   clears to send-nothing. The status board always shows
   `thinking:auto` or `thinking:<level>` — invisible states invite
   wrong assumptions (field report: "ultra" was invented in exactly
   such a gap).
5. **No error-driven autoswitch.** Errors surface verbatim; the
   pre-flight marks are the protection, because a silent downgrade
   changes cost and quality invisibly. Wrong levels on the wire are
   the model of record: the human decides.
6. **Dollar estimates on the bill** (`~$3.90`): net prompt × prompt
   rate + cached × cache-read rate + output × completion rate, from
   vendor pricing. Cache writes stay out (session-shape-dependent).

Rejections: interactive picker for `/thinking` in v1 (the listing is
the marks surface; a picker can come later); tiered `pricing.overrides`
(flat rates v1); vendoring `description`/`benchmarks`/`tokenizer`
(nothing acts on them). The suffix match (`claude-opus-4.7` →
`anthropic/claude-opus-4.7`) is unique-or-nothing — the snapshot's
bare names are collision-free today and the uniqueness guard keeps
that load-bearing, not lucky.

**AD-0044 addendum (2026-10-19): `/thinking` grew into the picker.**
The original ruling kept the listing as the marks surface and
deferred a picker. Reality settled it: selection-type commands here
are modals (`/model`, `/resume`, `/settings`, `/tasks`), and the
ladder is a selection. Bare `/thinking` now opens the ladder picker
— same rows, same honest marks, preselected at the current state,
Enter commits through the same `apply_thinking_choice` path the
argument form uses. `/thinking <level|auto>` stays for direct
switches. Nothing else in the ruling moves: marks stay honest, no
autoswitch, persistence unchanged.

**AD-0045 (2026-10-19): thinking is part of session identity.**
Resuming a session recorded at `high` under settings moved to `low`
came back at `low` — the header field written at open had no
readers. Three rulings, taken as posted:

1. **Restore on resume**: the session header's level outranks the
   settings default on every resume path (TUI `/resume` via
   `adopt`, headless via `apply_header_thinking` before the provider
   rebuild). A header without a level — old files, auto — keeps the
   current value; auto is never forced.
2. **Restamp on switch**: the live header follows the session.
   `/thinking` calls `Session::set_thinking`, which rewrites line
   one in place (transcript untouched, append handle reopened). An
   IO failure is reported and never rolls the switch back.
3. **Settings stay the default for new sessions**; no migration —
   old files just behave per ruling 1.

Out of scope, filed: restamping the header's `provider`/`model`
when `/model` switches mid-session (the same staleness, different
field — needs the picker's commit path, not this change). The wire
test pins the ruling end to end: session at `high`, settings moved
to `low`, `--continue` sends `reasoning_effort: high`.

**AD-0046 (2026-10-19): the model is part of session identity too.**
The pattern AD-0045 established for thinking extends to the model,
same three rulings in its key:

1. **Restore on resume**: the header's `provider`/`model` outrank
   the startup selection when still available
   (`selection::model_available`: the provider entry exists, and a
   configured model list contains the id — an entry without a list
   discovers, so any id is fair game). A vanished model falls back
   to the selection with a warning: a system message in the TUI, a
   stderr line headless. Never a refusal — a session's transcript
   stays readable.
2. **Restamp on switch**: `/model` calls `Session::set_model` on the
   live header, same line-one rewrite as the thinking restamp; the
   next resume restores the model the session *ended* on.
3. **Boot order stays**: discovery and selection run first
   (`resolve_target`), restoration is a post-open correction through
   the same seam that already rebuilds the provider for the
   cache-key stamp (`apply_session_identity`, called before the
   rebuild so the provider sees the restored identity). Restoration
   never does discovery — it only picks among what discovery found.

AD-0045's helper grew into `apply_session_identity` (thinking +
provider/model); its wire tests pin both branches end to end.

**AD-0047 (2026-10-19): `/context` — the request bucketed by class.**
Providers report round totals only (`Usage.prompt_tokens`), never a
per-message breakdown, so any by-type view is an estimate. Four
rulings:

1. **Classes mirror the wire, not the transcript**: system prompt,
   tool schemas, user, assistant, tool calls, tool results, images.
   The transcript's `Error`/`System` notes are absent by
   construction — `wire_messages` never sends them, so they cost
   the context nothing.
2. **The estimator re-serializes the actual outgoing request** (the
   same `wire_messages` shapes `stream` sends) and bills wire bytes
   over four per class. Deterministic, dependency-free, honest to
   the tokenizer's drift.
3. **Anchored, not raw**: class estimates scale by largest remainder
   to sum exactly to the last real `prompt_tokens`; the header
   names its provenance ("anchored to the provider's bill" vs
   "estimated (bytes over four)"). The exact number stays the
   provider's; only the split is ours.
4. **Core computes, the TUI renders**: `context_report` in hi-core
   (it owns the wire shapes); `/context` opens a read-only
   menu-slot panel — rows recompute fresh each frame, any key
   closes. A known window (config entry or catalog, the AD-0044
   chain) adds a free-space row.

**AD-0048 (2026-10-19): reasoning is content — store it, replay it
(#137).** The chat-completions wire streams thinking under
`reasoning_content` / `reasoning` / `reasoning_text`; hi used to drop
it at parse and pay its tokens anyway. Five rulings, maintainer-set:

1. **Pi-style blocks.** `Assistant` carries a block list
   (`AssistantBlock::Text` | `Thinking { thinking, signature,
   redacted }`) in stream order — interleaved thinking stays
   representable, and the Anthropic shape costs nothing later.
2. **OpenAI-completions only.** Ingest probes the three fields in
   pi's order; replay writes back onto the recorded field,
   `reasoning_content` by default. Everything else: later.
3. **Replay always on.** No `models.json` knob — if a real server
   rejects replayed reasoning, the flag arrives then.
4. **Trimming untouched.** #121 proceeds on its own premises;
   superseded-turn thinking is a candidate there, not here.
5. **No session-format marker.** Old files carry the assistant
   payload as a plain string and deserialize into a single `Text`
   block; absent field means no thinking.

The `signature` field records, opaquely, the wire field a block
streamed on — it is replay's write-back key. Live thinking streams
dimmed; the transcript renders it dim below the prose; `/context`
bills it as its own `Thinking` class — it is real context now.

**AD-0049 (2026-10-19): the tool-round default moves 16 → 32.**

`DEFAULT_MAX_TOOL_ROUNDS` was 16 — chosen when the loop had no real
workload behind it ("real tool conversations settle in single
digits"). Field use (#137-era testing against a reasoning model that
chains reads and greps) showed 16 is not headroom, it is a ceiling the
loop actually hits; a turn that dies at the cap loses all its pending
work at once. The default doubles to 32; the cap's *nature* is
unchanged — still a loud `Event::Error` on the single finalization
path, still a constant, not a preference, still overridable per
installation via `settings.maxToolRounds`. The doc's "settle in single
digits" claim is retired: the belt exists so an unattended turn cannot
burn the night, not because any specific number is sacred.

**AD-0050 (2026-10-19): session files are created lazily.**

Opening hi without `--continue` used to create a session file at
startup — accidental opens polluted session storage and `/resume`
listings with empty transcripts. `Session::start*` now plans the path
and touches no disk; the file materializes on the first recorded
record (message, usage, or name), and `materialize()` is the explicit
force verb for callers that must know at open time — the subagent
transcript opens eagerly or not at all, preserving its silent-degrade
contract. An empty `append` is a no-op: the transcript pollers call it
every frame, and a session with no records must not exist for them.

The e2e test written for this decision exposed a second, older bug:
the TUI never recorded the user's sent prompt at all — the frame
pump's append marker is captured after the send handler pushed the
line, so no later append's range ever included it (headless recorded
prompts correctly; interactive transcripts silently lost them). The
send path now appends its own line immediately (`persist_last`).

**AD-0051 (2026-10-20): transcript seams are inserted, not inherited.**

Every block used to bake its own blank rows in (`pad_plain` put one
above and below its content), so spacing was an emergent accident: two
plain blocks met in two blank rows, two filled blocks met in two
tinted rows touching — two different backgrounds flush against each
other — and thinking and the reply, welded into one display block by
`derive_blocks`' 1:1 message mapping, met in nothing at all.

The contract is now three rules. One: the scroll inserts **exactly one
blank separator row between any two adjacent blocks**, whatever their
kinds — the one spacing decision, in one place (`Scroll::cached`), and
the seam is part of the lower block so it flushes and survives
resizes. Two: a block with a background keeps its tinted fill rows
top and bottom — unbreakable — which is precisely why the seam must
exist between filled neighbors: backgrounds never touch. Three: a
plain block carries no padding rows; it ends on text rows, so the
separator is all the air two plain blocks get. Assistant messages
derive one display block per content block, so thinking and the reply
are governed by the same seam as everything else; the live stream
mirrors it (one seam per live unit) to keep AD-0024's no-reflow
guarantee.

The work also fixed a latent bug the seam tests flushed out: the
welcome seed was synthetic and width-invariant, but `rebuild_from_cursor`
re-derived every block including index 0 — so the first frame's width
sync clobbered "hi · say something" into blank `System("")` rows. The
seed is now never re-derived; a regression test holds the line.

**AD-0052 (2026-10-20): request compression is a per-provider opt-in.**
Chat-request bodies are the only large payloads hi ships (they scale
with context); all five major clouds accept `Content-Encoding: gzip` on
requests — verified by 401-parity probes with a valid gzip'd body and
no key — but a server that silently ignores request content-encoding
would feed gzip bytes to the model as the prompt. So the knob is
`requestCompression` in `models.json`, default off: config's "hi has no
opinion about which services exist" rules out host-sniffing auto-on,
and default-on would break the long tail. Bodies under 1 KiB
(`COMPRESS_MIN_BODY`) ride identity, since deflate overhead can exceed
the savings on small JSON. flate2 rides the tree via ureq's default
gzip feature (which is response-side); the FakeApi test double records
gzip'd request bodies decoded, so tests assert on the payload a
decompressing server would hand its parser.

**AD-0052 correction (2026-10-20): the verification claim was
overstated.** The 401-parity probes behind "all five major clouds
accept gzip'd request bodies" proved nothing: the discriminator — a
plain *invalid* JSON body, no key — also 401s, which shows auth runs
before body parsing, so a server that never decodes request bodies
returns the same 401 as one that decodes fine. The first real test was
the maintainer's daily driver: `api.z.ai/api/coding/paas/v4` 400s on a
gzip'd body with a valid key — the compressed bytes reach the JSON
parser. The default-off ruling kept a bad verification from shipping
as a break: the knob was opt-in, the user turned it off, done. Docs now
claim only what is attested: Anthropic decodes (Claude Code ships
gzip'd requests); Z.ai's coding endpoint does not; the rest is
unverified. The probe lesson generalizes: an endpoint that 401s
invalid JSON without a key cannot witness body handling at all.

**AD-0053 (2026-10-20): harness turns are a message variant, not a
string prefix.** Resuming a `taskTurns` session put the auto-trigger
mandate into the ↑ prompt-history ring: AD-0022 marked the turn with a
text prefix, but the seeded ring derives from transcript
`Message::User` lines and could not tell machine-spoken from
human-typed. The differentiation is now structural: `Message::Harness`
— a turn hi itself spoke to the model as the user (the mandate, the
subagent's brief). Rulings: the variant names the *speaker*, not the
occasion, so the next harness-spoken turn already has a home; the wire
is unchanged (same role, same text — the `(auto-trigger)` prefix stays
as the model's signal); session records gain a record-level
`origin: "harness"` while the payload stays `{"user": …}`, so old
binaries read new files unchanged and new binaries read old files
unchanged (the record parser navigates by key, so the extra key is
invisible to them); the transcript renders harness turns dim like
system notes; prompt history and the resume picker's preview treat
them as not-yours. The prefix-as-marker idea generalizes badly — a
marker the model must see and a marker the harness must act on are
different jobs, and only one of them belongs in the text.

**AD-0054 (2026-10-20): `/reload` — files win when they speak.**
Editing `models.json` or `settings.json` mid-session meant restart;
the running session kept its startup-frozen provider, catalog, and
resolved settings. `/reload` re-reads both files and applies what
they say, with per-file independence (a broken file keeps its old
state and says so — reload never kills the session, and a mid-turn
turn finishes on its own provider `Arc`). Settings follow "files win
when they speak": present keys apply live (`thinking`, `taskTurns`,
`images`, `maxToolRounds`, `subagentTranscripts`, `telemetry`);
silence keeps the live value, which forced `subagentTranscripts` from
a plain `bool` to `Option<bool>` — "off" must stay distinguishable
from unspoken. Startup-only knobs (`defaultModel`, `defaultTools`,
`subagents`, `sessionRetention`) are named in the note, not applied.
Models: the catalog gather moved out of `crates/cli` into
`Catalog::from_config`, shared by startup and `/reload`; the active
provider rebuilds through the same path a `/model` switch uses, so
baseUrl and `$VAR` key edits reach the next turn — the credential
rule becomes "read at launch and on explicit `/reload`" — and a
vanished model keeps the old provider running until the next
`/model`. The catalog's discovery re-runs synchronously with the
same 10s-per-provider bound as startup; if that ever hurts, the
escape hatch is incremental discovery, not a semantics change.

