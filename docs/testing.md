# Testing

Files this doc describes: `#[cfg(test)]` modules throughout `crates/core/src/`
and `crates/tui/src/`, `crates/core/src/test_support.rs`, root `README.md`
(build section).

## Running

```sh
nix develop          # or any Rust 1.85+ toolchain (edition 2024)
cargo test           # full suite; NO network needed
cargo run            # hi --mock needs no config
```

CI runs the same gates (fmt, clippy, test) plus a weekly `cargo audit` via GitHub Actions — see `.github/workflows/ci.yml`. The toolchain channel is `stable` everywhere (`rust-toolchain.toml` ↔ `flake.nix` ↔ CI). Third-party actions are SHA-pinned with a `# vX.Y.Z` comment; `dtolnay/rust-toolchain@stable` stays a moving branch ref on purpose — the compiler channel is pinned by `rust-toolchain.toml` (#24, AD-0037). Dependabot (`.github/dependabot.yml`) opens one grouped PR per ecosystem weekly (`cargo`, `github-actions`), ignoring Rust major bumps.

Current count: **435 `#[test]` functions** (the README's count gets refreshed in passing;
trust this file + `grep -rc '#\[test\]' crates`). Per-module breakdown: tools 66
core: openai 21, selection 14, session 12, read 13, settings 15, agent 11, tasks 17, grep 8,
walk 7, truncate 7, image_util 7, config 6, edit 6, image_resize 6, system_prompt 6,
mod 5, headless 9, bash 5, find 4, ls 3, write 3, mock 2, thinking 2, tool 1. tui: app 56,
markdown 19, ui 29, kitty 11, wrap 12, status 8, scroll 7, editor 7, picker 5,
commands 4, clipboard 3, theme 3, catalog 2. cli: 8 unit + 5 wire + 3 auto-print + 3 json-mode + 7 pty integration
(`tests/tui_pty.rs` — the real binary in a pseudo-terminal, its byte stream
rendered by the avt emulator (asciinema's), and every wait a predicate over
rendered frames — typed keys must echo in the composer, a settled turn must
stop spinner-cell churn, the composer must remain the last content on screen
and untinted, a full window must give its rows up to the flush, and `/exit`
must exit cleanly; dev-only `portable-pty` and `avt`).
## The central tool: `FakeApi` (`crates/core/src/test_support.rs`, cfg(test))

A minimal HTTP server on an ephemeral port:

- `FakeApi::start(response)` — binds `127.0.0.1:0`, one accept loop, one
  connection per request (responses carry `Connection: close`), answers every
  request with the one canned raw-HTTP response string, records each request
  verbatim; `base_url` is ready to drop into a `ProviderConfig`.
- `first_request()` / `request(n)` — the recorded requests, verbatim (assert on JSON body,
  headers, absence of `Authorization`, …).
- `start_sequence(responses)` — like `start`, but each request consumes the next response
  (last one repeats): the agent loop's multi-round conversations need round-specific
  answers.
- `sse_response(&[frames])` / `json_response(body)` — build raw HTTP
  responses with correct `Content-Length`.
- `ScriptedProvider` / `EchoTool` — a provider that replays scripted event rounds and
  records the history each round received, plus an echo tool; together they test the agent
  loop without any wire noise.

Usage pattern (see any test in `openai.rs` or `selection.rs`):

```rust
let api = FakeApi::start(sse_response(&["{\"choices\":[…]}", "[DONE]"]));
let cfg = config_with(&[("alpha", &api.base_url, &[])]);
// … exercise selection/provider against api.base_url …
assert!(api.first_request().contains("\"stream\":true"));
```

That rig lives behind `cfg(test)` — it can never see the **binary's**
construction path. `crates/cli/tests/wire.rs` is the process-level
sibling (#21): its own `TcpListener` fake answers the real `hi` with
scripted SSE while the test asserts on recorded request bodies, the
system-prompt chain, stdout, exit codes, and session files. It exists
because of the #20 bug class — the startup provider never attached
`SYSTEM.md`, and only a wire-level read could see it.

## Conventions

- Tests live in a `#[cfg(test)] mod tests` inside the file they test. No
  integration-test directory exists; keep it that way until it hurts.
- Temp dirs are created under `std::env::temp_dir()` with a
  `hi-<topic>-{tag}-{pid}` name — unique per process because tests run in
  parallel; cleaned best-effort.
- Environment mutation (edition 2024 marks `set_var` unsafe) appears only in
  single-threaded test contexts with an explicit `unsafe` block and a comment
  (see `resolves_env_interpolation_like_pi`).
- No test touches the real network: providers are pointed at `FakeApi`, or
  the `MockProvider` is used.
- Test names read as sentences describing behavior
  (`stale_saved_default_falls_through_to_autopick`) — keep that style; the
  name is the documentation.
- The event contract is pinned by tests in `mock.rs` and `openai.rs`
  (`streams_turnstart_tokens_turnend_in_order`,
  `http_error_becomes_error_then_turnend`,
  `stops_when_receiver_is_dropped`) — if you change `Event` semantics, those
  are the gates.

## Coverage map (what is deliberately tested where)

| Area | Where |
|---|---|
| models.json parsing, camelCase, unknown fields, XDG paths, env interpolation | `config.rs` |
| selection precedence (5 rules), autopick, listing errors | `selection.rs` |
| settings round-trip, missing/junk file, camelCase | `settings.rs` |
| thinking level parse/serde | `thinking.rs` |
| system prompt chain, whitespace-only files | `system_prompt.rs` |
| tool registry rules, gate decisions (deny/mutate), loop rounds + cap, scripted replay | `agent.rs`, `tool.rs` |
| headless drain: final text, narration flush before tool calls, error contract, session append, prompt assembly | `headless.rs` |
| json records: transcript order with token deltas, failed terminal, session header when open, usage bill | `headless.rs` |
| `--mode json` through the real binary: framing, header/done, `--no-session`, text `-p` contract | `crates/cli/tests/json_mode.rs` |
| auto-print on redirected streams: pipe→print, guard bypass, honest no-prompt, terminal still opens the TUI | `crates/cli/tests/auto_print.rs` + `tui_pty.rs` |
| wire-level: the real binary against a recording fake API — system-prompt chain on the wire, thinking kwargs, `--continue`/`--no-session` files, failure contract, json stream | `crates/cli/tests/wire.rs` |
| built-in tools: truncation contract, gitignore walk, per-tool behavior (paging, uniqueness, exit codes, timeouts), bundle selection | `tools/` |
| session layout, header/message round-trip incl. tools, torn-tail tolerance, project keys, XDG data dir | `session.rs`, `config.rs` |
| SSE parsing, streaming end-to-end, discovery shapes, thinking kwargs on the wire, tool-call fragments + wire replay, auth-header rules, error contract | `openai.rs` |
| git branch detection (HEAD ref, detached, worktree `.git` file, walk-up, no repo), status composition | `tui/status.rs` |
| markdown rendering: wrap, headings, fences, inline roles, links, lists/tasklists, quotes, rules, box tables + shrink, hard splits | `tui/markdown.rs` |
| transcript blocks: user fill + padding, plain assistant, system/error styles, tool state fills, args collapse, preview cap + marker, call/result pairing, pending detection | `tui/ui.rs` |
| palette roles resolve to pi's dark.json values | `tui/theme.rs` |
| session listing (mtime order, unreadable skipped), id resolution (path/exact/prefix, local then global), fork (copy-only source, header rewrite, name override), header camelCase fields | `core/session.rs` |
| picker rows (age buckets, words-lead with demoted id, rename prompt), filter matching, line composition incl. selection marker and empty-filter note | `tui/picker.rs` |
| `/resume` opens/filters/switches, Esc closes untouched, refused mid-turn | `tui/app.rs` |
| selection flags' clap conflict matrix (pi's rules; `--name` pairs with start/fork only) | `cli/main.rs` |
| wire identity: UA format + presence on chat/discovery, gated attribution (host-exact OpenRouter), cache-key body field, byte-stable prefix across turns; `HI_TELEMETRY`-beats-setting resolution | `core/openai.rs`, `core/settings.rs` |
| images in read: magic-byte sniffing (never extensions), attach/withhold policy notes, base64 round-trip; the resize ladder — byte-identical pass-through, dimension scaling, JPEG transcode under budget, unshrinkable refusal, raw/decode guards; kitty rendering — chunk-exact encoding (`C=1`, 4096-byte `m` chunks), pi's detection matrix via injected env snapshots, fit math with the distortion chooser, PNG conversion guards (structural chunk-walk, decode-limited re-encode), corrupt fallback notes, segment placement and height; pasted attachments — RGBA→PNG encoding with shape checks, paste padding and line-ending normalization, staged-path outcomes through the frame pump, transient failure notes, and the command-dispatch rule that keeps `/tmp/…` paths messages; synthetic user carrier + image-only placeholder on the wire; carrier determinism under the byte-stable prefix; image session round-trip + pre-image files load; the fail-open image policy (configured `vision: false` honored, discovered models vision-capable, master switch first) and switch-time gate re-resolution | `core/tools/read.rs`, `core/tools/image_resize.rs`, `core/image_util.rs`, `core/openai.rs`, `core/session.rs`, `core/config.rs`, `core/settings.rs`, `tui/kitty.rs`, `tui/ui.rs` |
| slash matching, exact dispatch, registry hygiene | `tui/commands.rs` |
| catalog flattening, empty catalog | `tui/catalog.rs` |
| queueing, Esc-cancel + queue restore, completion keys, unknown commands | `tui/app.rs` |
| wrap width semantics, oversized-word hard split | `tui/wrap.rs` |

Gaps to be aware of: `ui.rs` drawing has row-level tests (scrollback wrapping) but no pty/frame snapshots, `lib.rs` event loop is
untested (needs a pty), CLI `main` is untested (thin by design).
