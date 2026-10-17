# hi

A minimalist agent harness: hold a streaming conversation with a language
model from your terminal. Written in Rust (ratatui TUI) as a learning
project, with a strict split: `crates/core` never touches the terminal,
`crates/tui` drives it. Everything beyond this core arrives as extensions
later.

## What works today

- Inline TUI — a few rows above your shell prompt, no alt-screen takeover
- pi-style transcript: markdown rendering with tables, role-themed blocks, and merged tool blocks tinted by outcome (pending → success/error)
- Streaming replies render live as markdown, then finalize into native scrollback
- Two providers behind one trait:
  - **mock** — canned reply, no network, no key (used by tests and demos)
  - **OpenAI-compatible** — real HTTP, server-sent events, model discovery
- pi-style `models.json` configuration with `$ENV` API-key interpolation
- Failures keep partial output and add a red `error` line; the input stays live
- A real CLI: session flags (`--continue`, `-r`, `--session`, `--fork`, `-n`, `--no-session`), headless `-p`/`--print` with piped stdin, `--tools` bundles, `--provider`/`--model`, `--list-models`, `--mock` — and Esc cancels a running turn
- `/model` picker in the TUI: arrows to move, Enter switches this session, Ctrl-S saves the default
- Slash commands with completion: type `/` to see the menu, Tab completes, Enter runs
- Session transcripts persist per project (JSONL, append-only) — created lazily, on the first recorded message, so opening hi and quitting writes nothing; `hi --continue` resumes the newest, `hi -r` opens a picker, `hi --session <id>` and `hi --fork <id>` select and branch deterministically, `/new` starts a fresh session mid-conversation — headless too
- Headless one-shot mode: `hi -p "prompt"` prints the final reply and exits — and on a redirected stream the flag is optional (`git diff | hi "review"` auto-runs print mode); `--mode json` streams the whole run as JSONL records instead — session header, token deltas, transcript messages, usage, one `done`/`failed` terminal
- Honest wire identity: `hi/<version> (<os>; <arch>)` on every request; prompt-cache affinity via the session id so multi-turn runs hit server-side caches; telemetry beyond that is strictly opt-in
- `read` attaches image files (png/jpeg/webp/gif) — fail-open like pi: on unless the `images` master switch is off or a model sets `vision: false`; resized/transcoded to a universal budget first, honest metadata notes otherwise
- on kitty-protocol terminals (kitty, Ghostty, WezTerm, Warp) those images render as real graphics in the transcript; everywhere else the text note stays
- Ctrl-V pastes from the clipboard: an image is staged at a temp file and its path lands in the input as plain text — nothing sends until Enter, and the image enters the conversation only when the model reads it
- Built-in agent tools: the model can read, search, list, write, edit, and run commands — the `coding` bundle by default, `--tools read-only|none|all` to change; `coding` and `all` also carry `subagent`: the model runs nested agent sessions (fresh history, own provider/model/toolset choice, capped rounds, no recursion); the `all` bundle additionally adds tracked background tasks (`task_run`/`task_status`/`task_output`/`task_cancel`): the model spawns long jobs and polls without blocking a turn
- A pi-style composer: `Ctrl-K`/`Ctrl-U`/`Ctrl-W` kill, `Ctrl-Y` yanks the newest kill back, `Alt-Y` cycles older kills; undo/redo (`Ctrl-Z`/`Alt-Z`); jump-to-char (`Ctrl+]` forward, `Ctrl+Alt+]` backward) — `/keybinds` prints the whole table on demand
- 540 tests, including a fake HTTP server and a pty harness that drive the real binary

Not yet: extensions.
The selection and settings layers are built so each slots in without
rework.

## Build & run

```sh
nix develop          # Rust toolchain + cargo (or any Rust 1.85+, edition 2024)
cargo run            # builds the `hi` binary; `hi --mock` needs no config
cargo test           # 540 tests; no network needed
nix run .            # run the app straight from the flake — no devshell needed
nix profile install github:noosxe/hi   # or install the binary (flake = the package)
```


## Usage

```text
A minimalist agent harness

Usage: hi [OPTIONS] [PROMPT]

Arguments:
  [PROMPT]  The prompt for --print or --mode (piped stdin is prepended)

Options:
      --provider <PROVIDER>  Provider name from models.json
      --model <MODEL>        Model id to use (see --list-models)
      --tools <TOOLS>        Built-in tool bundle: none | read-only | coding | all (default: coding,
                             or defaultTools in settings.json)
      --mock                 Use the built-in mock provider — no config, no network
      --list-models          Print the selected provider's models and exit
      --continue             Resume the most recent session for this directory
  -r, --resume               Pick a session to resume (arrow list with type-to-filter)
      --session <SESSION>    Open a session by file path or (partial) id
      --fork <FORK>          Fork a session (path or id) into a new file for this directory
  -n, --name <NAME>          Name the new session (fresh start or fork)
  -p, --print                Run one prompt headlessly: print the final reply and exit
      --mode <MODE>          Headless output mode: json streams JSONL records (#18) [possible
                             values: json]
      --no-session           Run without loading or saving a session file
  -h, --help                 Print help
  -V, --version              Print version

How hi picks a provider and model when you don't name both:

1. `--provider P --model M` — used as given
2. `--provider P` — the first model P offers (discovery included)
3. `--model M` — the first provider whose configured list contains M
4. `defaultModel` from `settings.json` — only when no flags are given
5. nothing — the first provider (alphabetically) that offers any model

In the TUI: **Enter** sends, **Esc** cancels the running turn (partial
output stays in the transcript), **Ctrl-C** clears the draft — and quits when it is already empty. A dim status row
under the input shows `cwd (branch)` on the left — the branch only
when you're inside a git repository — and `(provider) model` on the
right.

While a turn is streaming, the input's top rule shows a spinner, the
elapsed seconds, and a reminder that Esc cancels.

Type `/model` in the input to open the picker: **arrows** move,
**Enter** switches the current session (safe even mid-turn — the
running turn finishes on its original model), **Ctrl-S** saves the
choice as the startup default, **Esc** closes.

The model's reasoning streams **live and dimmed** ahead of the reply,
is stored in the session, and replays to the provider on later turns —
the model remembers its own thinking (AD-0048). `/context` bills it
as its own class.

Ctrl-S writes `~/.config/hi/settings.json`:

```jsonc
{ "defaultModel": "zai/glm-5.3" }   // camelCase, unknown fields ignored
```

A missing settings file is normal; a stale or malformed
`defaultModel` falls through to autopick rather than failing startup.
Tools ship with the harness: the `coding` bundle (read, bash, edit, write)
is the default, `--tools` or a `defaultTools` setting picks another, and an
effort preference will join `defaultModel` in settings.

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
| `contextWindow` | model | no | u32 tokens; the usage line's context-share denominator (AD-0026) |
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

### System prompt

hi sends a short built-in default (`"You are hi, a concise terminal
assistant."`) as the leading system message. Two markdown files
override or extend it, at the user level and the project level:

| File | Scope | Effect |
|---|---|---|
| `~/.config/hi/SYSTEM.md` | user | replaces the default |
| `~/.config/hi/APPEND_SYSTEM.md` | user | appends |
| `.hi/SYSTEM.md` (in the project) | project | replaces everything above |
| `.hi/APPEND_SYSTEM.md` (in the project) | project | appends |

Precedence: project replaces user replaces default; appends join in
user-then-project order, blank-line separated. Whitespace-only files
count as absent.

### Thinking level

`settings.json` accepts `"thinking": "off" | "minimal" | "low" | "medium" |
"high"` (default: unset — the provider's own default applies). The level
reaches the wire only when the model's `thinkingLevelMap` defines it; the
map's JSON object for that level merges into the chat request body. The
status row shows the active level.

```jsonc
// models.json, per model:
{ "id": "glm-4.6", "thinkingLevelMap": {
    "off":  { "thinking": { "type": "disabled" } },
    "high": { "thinking": { "type": "enabled" } }
} }
// settings.json:
{ "defaultModel": "zai/glm-4.6", "thinking": "high" }
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
  attempts, hard timeouts for bash child processes — a timeout kills the whole process group, not just the child — and dies
  with the session: teardown sweeps children, so nothing orphans
  itself to the OS and burns a core for hours.
- Errors are plain `String`s for now — descriptive and user-facing;
  a dedicated error type arrives when it earns its keep.

## License

Dual-licensed under either the [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE) license (`SPDX: MIT OR Apache-2.0`) — choose whichever you prefer. All dependencies are permissively licensed.
