# hi

A minimalist agent harness: hold a streaming conversation with a language
model from your terminal. Written in Rust (ratatui TUI) as a learning
project, with a strict split: `crates/core` never touches the terminal,
`crates/tui` drives it. Everything beyond this core arrives as extensions
later.

## What works today

- Inline TUI — a few rows above your shell prompt, no alt-screen takeover
- Streaming replies, typed out dim in a live area, finalized into scrollback
- Two providers behind one trait:
  - **mock** — canned reply, no network, no key (used by tests and demos)
  - **OpenAI-compatible** — real HTTP, server-sent events, model discovery
- pi-style `models.json` configuration with `$ENV` API-key interpolation
- Failures keep partial output and add a red `error` line; the input stays live
- 12 tests, including a fake HTTP server that records requests

Not yet: provider selection flags and a saved default model, a `/model`
picker, Esc-to-cancel mid-turn, extensions. The core supports all of it;
the TUI wiring is the next milestone.

## Build & run

```sh
nix develop          # Rust toolchain + cargo (or any Rust 1.85+, edition 2024)
cargo run -p hi-tui  # currently always uses the mock provider
cargo test           # 12 tests; no network needed
```

## Configuration: `models.json`

One file, one schema. Location — deliberately identical on Linux and
macOS, friendly to dotfile syncing:

```
$XDG_CONFIG_HOME/hi/models.json
~/.config/hi/models.json        # when $XDG_CONFIG_HOME is unset or empty
```

Key names are **camelCase**, matching pi's `models.json`, so entries can
be copied back and forth. Unknown fields are **ignored on purpose** —
newer configs never break older builds.

### Schema

```jsonc
{
  "providers": {                    // map of provider name → endpoint
    "<name>": {
      "baseUrl": "http://…/v1",     // required; include the version path
      "apiKey": "$MY_API_KEY",      // optional; see credentials below
      "models": [                   // optional; see discovery below
        {
          "id": "model-id",         // required; sent to the API as-is
          "name": "Display Name",   // optional; shown instead of id
          "reasoning": false,       // optional; default false
          "contextWindow": 100000,  // optional; tokens, informational
          "maxTokens": 16384        // optional; max completion tokens
        }
      ]
    }
  }
}
```

| Field | Where | Required | Notes |
|---|---|---|---|
| `providers` | root | no | map of names; an empty/missing map is valid |
| `baseUrl` | provider | **yes** | root URL *including* version path, e.g. `http://localhost:8080/v1` |
| `apiKey` | provider | no | literal, `$NAME`, or `${NAME}`; see below |
| `models` | provider | no | array; absent or empty means *discover* (below) |
| `id` | model | **yes** | identifier sent to the API |
| `name` | model | no | display name; falls back to `id` |
| `reasoning` | model | no | default `false` |
| `contextWindow` | model | no | u32 tokens; informational for now |
| `maxTokens` | model | no | u32; max completion tokens |

### Credentials

`apiKey` is a credential *source*, not necessarily a literal key:

- `"$MY_API_KEY"` / `"${MY_API_KEY}"` — read from the environment
- `"sk-literal-key"` — used as-is
- missing/omitted — no `Authorization` header is sent at all (some
  local servers reject requests that carry one)

The environment is read **once, at launch**; changing variables
mid-session does nothing. That is deliberate: predictable and testable.

### Model listing — the hybrid policy

- Explicit `models` entries **win** when present. Only they can carry
  what discovery cannot know: `contextWindow`, `reasoning`, display name.
- An **empty or absent `models` list means "ask the server"**: hi calls
  `GET {baseUrl}/models` and uses whatever it reports, sorted and
  deduplicated. Point hi at a third-party provider with just a URL and a
  key and it finds everything that server serves.

Discovery failures say so explicitly and hint at the escape hatch
(*list models explicitly in models.json*) — e.g. when a server has no
`/models` endpoint.

### Example

```jsonc
{
  "providers": {
    "local": {
      // llama.cpp server / LM Studio / vLLM — models come from discovery
      "baseUrl": "http://localhost:8080/v1"
    },
    "cloud": {
      // remote OpenAI-compatible endpoint — models pinned explicitly
      "baseUrl": "https://api.example.com/v1",
      "apiKey": "$CLOUD_API_KEY",
      "models": [
        { "id": "model-small", "contextWindow": 32000, "maxTokens": 4096 },
        { "id": "model-large", "reasoning": true, "contextWindow": 200000, "maxTokens": 16384 }
      ]
    }
  }
}
```

### Wire protocol

The only protocol hi speaks (v0): OpenAI-compatible chat completions.

- turns: `POST {baseUrl}/chat/completions`, `"stream": true`,
  `Authorization: Bearer <key>` (only when a key resolves non-empty)
- listing: `GET {baseUrl}/models`, same auth rule

## Project layout

```
crates/core   # events, messages, provider trait, config, mock + OpenAI providers
crates/tui    # ratatui front end; polls core events, owns the terminal
```

Design notes worth keeping in mind:

- **Drop is cancellation.** A provider's next channel send fails once
  the consumer is gone; there are no cancel flags.
- **`Event::Error` + `TurnEnd`** — failures share one finalization path
  with successful turns; partial output always survives.
- **Blocking I/O on a thread, not async.** The turn thread blocks;
  the UI thread never does.
- Errors are plain `String`s for now — descriptive and user-facing;
  a dedicated error type arrives when it earns its keep.
