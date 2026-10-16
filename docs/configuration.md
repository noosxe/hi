# Configuration

Files this doc describes: `crates/core/src/config.rs`, `settings.rs`,
`system_prompt.rs` (types and resolution), plus the schemas documented in the
root README. Behavior questions (precedence, discovery policy) are covered in
[core.md](core.md); this doc is the schema/path reference.

## File locations

| File | Path | Created how |
|---|---|---|
| `models.json` | `$XDG_CONFIG_HOME/hi/models.json`, else `~/.config/hi/models.json` | by hand (failure to load is fatal at startup, with a hint) |
| `settings.json` | same dir, `settings.json` | by the app (Ctrl-S in the `/model` picker); missing file is normal |
| `SYSTEM.md` / `APPEND_SYSTEM.md` | same dir | by hand, optional |
| `.hi/SYSTEM.md` / `.hi/APPEND_SYSTEM.md` | project root (cwd) | by hand, optional |
| sessions | `$XDG_DATA_HOME/hi/sessions/<project>/`, else `~/.local/share/hi/sessions/<project>/` | by the app, every run |

`XDG_CONFIG_HOME` must be set **and non-empty** to be honored; otherwise
`~/.config/hi` is used (tested: `config_dir_prefers_xdg_then_falls_back_to_home`).
One path scheme on every platform, friendly to dotfile syncing.

Key names are **camelCase** everywhere (pi-compatible, so entries can be
copied back and forth). Unknown fields are **ignored on purpose** — newer
configs never break older builds (tested for both files).

## `models.json`

```jsonc
{
  "providers": {
    "<name>": {
      "baseUrl": "http://localhost:8080/v1",   // required; INCLUDE the version path
      "apiKey": "$MY_API_KEY",                 // optional; see credentials
      "models": [                              // optional; empty/absent = discover
        {
          "id": "model-id",                    // required; sent to the API as-is
          "name": "Display Name",              // optional; falls back to id
          "reasoning": false,                  // optional; default false
          "contextWindow": 100000,             // optional; u32 tokens — the usage line's context-share denominator (AD-0026)
          "maxTokens": 16384,                  // optional; u32, max completion tokens
          "thinkingLevelMap": {                // optional; level name → request kwargs
            "off":  { "thinking": { "type": "disabled" } },
            "high": { "thinking": { "type": "enabled" } }
          }
        }
      ]
    }
  }
}
```

- `providers` is a map; names are lookup keys only (sorted iteration —
  `BTreeMap` — drives autopick order and `--model` search order).
- The model listing policy is hybrid: non-empty `models` wins (only config
  entries can carry `contextWindow`/`reasoning`); empty/absent means live
  discovery via `GET {baseUrl}/models` (`data[].id` or `models[].slug`).
- `"vision": false` on a model entry opts out of image input — the
  default is **true** (pi-parity: models discovered from the API have
  no entry and read as vision-capable, AD-0012). Gates `read`'s image
  attachments (see the settings section).
- `thinkingLevelMap` is the only place provider-specific reasoning options
  exist: when the session's thinking level has an entry here, that JSON
  object merges top-level into the chat request body. No provider knowledge
  lives in code.

### Credentials (`apiKey`)

A credential *source*, not necessarily a literal:

- `"$NAME"` / `"${NAME}"` — read from the environment **once, at launch**;
  mid-session changes do nothing (deliberate: predictable, testable)
- missing variable → empty string → **no `Authorization` header sent**
- literal string → used as-is
- omitted → no `Authorization` header (some local servers reject its presence)

## `settings.json`

```jsonc
{
  "defaultModel": "zai/glm-5.3",   // "provider/model"; stale/malformed falls through to autopick
  "thinking": "high",              // off | minimal | low | medium | high; optional
  "defaultTools": "coding"         // none | read-only | coding | all; --tools overrides per run
  "telemetry": false               // opt-in provider attribution; see below
  "images": true                   // image upload master switch; default on
  "taskTurns": false               // opt-in: finished background tasks may start a turn
  "sessionRetention": 30           // opt-in: prune session files older than N days at startup
  "maxToolRounds": 40              // opt-in: main-loop tool-round cap; default 16, must be >= 1
  "subagentTranscripts": true     // opt-in: record each subagent run as its own session file
}
```

- Missing file = defaults (normal first-run state). Unparseable file = error
  (surfaced, not ignored) — only `models.json` gets the
  "create one" startup hint.
- `defaultTools` picks the built-in tool bundle (`none`, `read-only`,
  `coding`, `all`); the `--tools` flag wins per run, and an unknown value
  fails startup loudly in both places.
- `defaultModel` applies **only when no `--provider`/`--model` flags are
  given** (precedence in [core.md](core.md) → selection.rs).
- `sessionRetention` (#23) is opt-in and absent by default: sessions
  are user history and are never deleted unless you set it. When set,
  startup deletes `*.jsonl` session files older than that many days
  across every project — before any session opens, best-effort, one
  stderr line only when something was removed. A session being
  written right now refreshes its mtime, so a live run is never a
  prune candidate.
- `maxToolRounds` is opt-in and absent by default: the main loop's
  tool-round cap stays at the built-in 16 unless you set it. Must be at
  least 1 (zero fails every turn — rejected at startup with a clear
  error). Resolved once at startup, so `/settings` does not flip it;
  the `subagent` tool's `max_rounds` argument is a separate, per-call
  knob and is unaffected.
- `subagentTranscripts` (#116) is opt-in and false by default: when
  true, every subagent run writes its own ordinary session file
  (`kind: "subagent"`, the parent session's id in the header) under
  this project's sessions directory — inspectable and resumable with
  no special-casing, pruned by the same retention rules; the tool's
  result line names each recording (`recorded → <path>`). Off by
  default because a chatty parent turn can spawn many files. Startup
  policy only, not flippable from `/settings`.
- `/settings` flips what the running session can flip — `taskTurns`,
  `images` — without touching this file; Ctrl-S on the board writes
  one field back here (AD-0023). The file stays the startup source of
  truth.
- `taskTurns` is the orchestrator opt-in (#72, AD-0022): when a
  background task finishes and no turn is in flight, hi starts a turn
  whose payload carries the completion notes, marked
  `(auto-trigger)`. Default off — a turn spends the user's tokens, so
  only the human flips this. A per-session budget (10 turns) bounds
  completion→turn→spawn→completion chains; when it runs out hi says so
  once in the transcript and further completions ride the human's next
  turn, exactly as before.
- `thinking` reaches the wire only through the selected model's
  `thinkingLevelMap`; the active level is shown in the status row
  (`(provider) model · thinking:high`).
- **Telemetry is opt-in, and it is the only telemetry.** hi sends no
  analytics, ever. The always-on identifier is the honest
  `User-Agent: hi/<version> (<os>; <arch>)` on provider requests.
  `telemetry: true` additionally sends attribution headers
  (`HTTP-Referer`, `X-Title`) — and only toward `openrouter.ai`, the
  endpoint family that displays them in its dashboard. The
  `HI_TELEMETRY` env var overrides the file when set (`1`/`true`/`yes`
  on, anything else off; pi's `PI_TELEMETRY` pattern).
- **Image upload** (`images`, default on) is the master switch for
  sending pictures to the model — `read`'s attachments today, pasted
  attachments when they land (#41). Off means metadata notes only,
  regardless of the model.
- **Image policy**: `images` master switch first; then the model's
  rule — a configured `vision: false` opts out, and a model with no
  entry (API-discovered) reads as vision-capable (AD-0012, pi's
  fail-open). The gate is re-resolved on every `/model` and `/resume`
  switch; only an explicit opt-out makes `read` return metadata notes.

## System prompt files

Built-in default: `"You are hi, a concise terminal assistant."`

| File | Scope | Effect |
|---|---|---|
| `SYSTEM.md` (user dir) | user | replaces the default |
| `.hi/SYSTEM.md` (project) | project | replaces everything above |
| `APPEND_SYSTEM.md` (user dir) | user | appended |
| `.hi/APPEND_SYSTEM.md` (project) | project | appended after the user one |

Joined blank-line separated; whitespace-only files count as absent; the
result travels as one leading `role: "system"` message.

## Sessions

One JSONL file per session, under the **data** directory (not config —
transcripts are generated data):

```
~/.local/share/hi/sessions/--Users-mechsoull-Projects-hi--/1760000000000_<uuid>.jsonl
```

- The directory name is the cwd, pi-style (`/` → `-`, wrapped in `--`), so
  resume is per-project by construction.
- Line 1 is the session header (`type:"session"`, camelCase: `id`,
  `createdAt`, `cwd`, `provider`, `model`, optional `thinking`, `version`).
  Every later line is one finalized message:
  `{"type":"message","message":{"toolCall":{…}}}` — the same shapes as the
  `Message` enum, so tool calls and results round-trip natively.
- Writes are **append + flush at turn end**; tokens are never persisted. A
  crash costs at most the in-flight turn, and a torn final line is
  tolerated on read (a malformed line mid-file is a loud error).
- Unknown fields are ignored, like every hi file — newer sessions open in
  older builds.
- `hi --continue` reopens the newest session for the current directory and
  keeps appending to the same file; the model/provider come from today's
  flags and config, not frozen from the header.
