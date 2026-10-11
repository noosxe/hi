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

Current count: **76 `#[test]` functions** (the README still says 43 — it
lags; trust this file + `grep -c '#\[test\]' crates -r`). Per-module
breakdown: selection 14, app 14, openai 10, status 7, system_prompt 6,
settings 5, wrap 6, commands 4, config 4, mock 2, catalog 2, thinking 2.

## The central tool: `FakeApi` (`crates/core/src/test_support.rs`, cfg(test))

A minimal HTTP server on an ephemeral port:

- `FakeApi::start(response)` — binds `127.0.0.1:0`, one accept loop, one
  connection per request (responses carry `Connection: close`), answers every
  request with the one canned raw-HTTP response string, records each request
  verbatim; `base_url` is ready to drop into a `ProviderConfig`.
- `first_request()` — the recorded first request (assert on the JSON body,
  headers, absence of `Authorization`, …).
- `sse_response(&[frames])` / `json_response(body)` — build raw HTTP
  responses with correct `Content-Length`.

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
| SSE parsing, streaming end-to-end, discovery shapes, thinking kwargs on the wire, auth-header rules, error contract | `openai.rs` |
| event ordering, receiver-drop cancellation | `mock.rs`, `openai.rs` |
| git branch detection (HEAD ref, detached, worktree `.git` file, walk-up, no repo), status composition | `tui/status.rs` |
| slash matching, exact dispatch, registry hygiene | `tui/commands.rs` |
| catalog flattening, empty catalog | `tui/catalog.rs` |
| queueing, Esc-cancel + queue restore, completion keys, unknown commands | `tui/app.rs` |
| wrap width semantics, oversized-word hard split | `tui/wrap.rs` |

Gaps to be aware of: `ui.rs` has no tests (drawing), `lib.rs` event loop is
untested (needs a pty), CLI `main` is untested (thin by design).
