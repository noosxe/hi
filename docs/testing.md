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

CI runs the same gates (fmt, clippy, test) plus a weekly `cargo audit` via GitHub Actions — see `.github/workflows/ci.yml`. The toolchain channel is `stable` everywhere (`rust-toolchain.toml` ↔ `flake.nix` ↔ CI).

Current count: **88 `#[test]` functions** (the README's count gets refreshed in passing;
trust this file + `grep -rc '#\[test\]' crates`). Per-module breakdown: selection 14,
app 14, openai 13, agent 9, status 7, system_prompt 6, wrap 6, settings 5, commands 4,
config 4, mock 2, catalog 2, thinking 2.
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
| SSE parsing, streaming end-to-end, discovery shapes, thinking kwargs on the wire, tool-call fragments + wire replay, auth-header rules, error contract | `openai.rs` |
| git branch detection (HEAD ref, detached, worktree `.git` file, walk-up, no repo), status composition | `tui/status.rs` |
| slash matching, exact dispatch, registry hygiene | `tui/commands.rs` |
| catalog flattening, empty catalog | `tui/catalog.rs` |
| queueing, Esc-cancel + queue restore, completion keys, unknown commands | `tui/app.rs` |
| wrap width semantics, oversized-word hard split | `tui/wrap.rs` |

Gaps to be aware of: `ui.rs` has no tests (drawing), `lib.rs` event loop is
untested (needs a pty), CLI `main` is untested (thin by design).
