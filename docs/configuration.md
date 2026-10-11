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
          "contextWindow": 100000,             // optional; u32 tokens, informational
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
  "thinking": "high"               // off | minimal | low | medium | high; optional
}
```

- Missing file = defaults (normal first-run state). Unparseable file = error
  (surfaced, not ignored) — only `models.json` gets the
  "create one" startup hint.
- `defaultModel` applies **only when no `--provider`/`--model` flags are
  given** (precedence in [core.md](core.md) → selection.rs).
- `thinking` reaches the wire only through the selected model's
  `thinkingLevelMap`; the active level is shown in the status row
  (`(provider) model · thinking:high`).

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
