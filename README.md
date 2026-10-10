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
- A real CLI: `--provider`/`--model`, `--list-models`, `--mock`, `--help` — and Esc cancels a running turn
- `/model` picker in the TUI: arrows to move, Enter switches this session, Ctrl-S saves the default
- Slash commands with completion: type `/` to see the menu, Tab completes, Enter runs
- 43 tests, including a fake HTTP server that records requests

Not yet: headless one-shot mode, an effort indicator, extensions.
The selection and settings layers are built so each slots in without
rework.

## Build & run

```sh
nix develop          # Rust toolchain + cargo (or any Rust 1.85+, edition 2024)
cargo run            # builds the `hi` binary; `hi --mock` needs no config
cargo test           # 43 tests; no network needed
```


## Usage

```text
A minimalist agent harness

Usage: hi [OPTIONS]

Options:
      --provider <PROVIDER>  Provider name from models.json
      --model <MODEL>        Model id to use (see --list-models)
      --mock                 Use the built-in mock provider — no config, no network
      --list-models          Print the selected provider's models and exit
  -h, --help                 Print help
  -V, --version              Print version
```

How hi picks a provider and model when you don't name both:

1. `--provider P --model M` — used as given
2. `--provider P` — the first model P offers (discovery included)
3. `--model M` — the first provider whose configured list contains M
4. `defaultModel` from `settings.json` — only when no flags are given
5. nothing — the first provider (alphabetically) that offers any model

In the TUI: **Enter** sends, **Esc** cancels the running turn (partial
output stays in the transcript), **Ctrl-C** quits. A dim status row
under the input shows `cwd (branch)` on the left — the branch only
when you're inside a git repository — and `(provider) model` on the
right.

While a turn is streaming, the input's top rule shows a spinner, the
elapsed seconds, and a reminder that Esc cancels.

Type `/model` in the input to open the picker: **arrows** move,
**Enter** switches the current session (safe even mid-turn — the
running turn finishes on its original model), **Ctrl-S** saves the
choice as the startup default, **Esc** closes.

Ctrl-S writes `~/.config/hi/settings.json`:

```jsonc
{ "defaultModel": "zai/glm-5.3" }   // camelCase, unknown fields ignored
```

A missing settings file is normal; a stale or malformed
`defaultModel` falls through to autopick rather than failing startup.
Headless one-shot mode is planned as a second branch in the same
binary; an effort preference will join `defaultModel` in settings.

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

Discovery accepts the OpenAI shape (`data[].id`) and catalog-style
`models[].slug` responses. A 200 reply with *neither* is a loud error
suggesting explicit `models` entries — never a silent empty list — and
listing failures always name the provider and the cause.

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
crates/core   # events, messages, provider trait, config, selection, providers
crates/tui    # ratatui front end as a library: run(provider) draws and polls
crates/cli    # the `hi` binary: clap flags, dispatch, exit codes
```

Design notes worth keeping in mind:

- **Drop is cancellation.** A provider's next channel send fails once
  the consumer is gone; there are no cancel flags.
- **`Event::Error` + `TurnEnd`** — failures share one finalization path
  with successful turns; partial output always survives.
- **Blocking I/O on a thread, not async.** The turn thread blocks;
  the UI thread never does.
- **Nothing outlives its session.** Every thread or child process hi
  spawns is born with a deadline — bounded DNS/connect on network
  attempts, hard timeouts for any future child processes — and dies
  with the session: teardown sweeps children, so nothing orphans
  itself to the OS and burns a core for hours.
- Errors are plain `String`s for now — descriptive and user-facing;
  a dedicated error type arrives when it earns its keep.
