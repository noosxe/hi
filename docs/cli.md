# `hi` CLI reference

Files this doc describes: `crates/cli/` (`Cargo.toml`, `src/main.rs`). The
binary is thin on purpose: it decides *what* runs — mock, TUI, headless
`--print`, model listing — while `hi-core` resolves and `hi-tui` draws.
The headless branch links no `hi-tui` code paths at runtime.

## Flags (clap derive, `--help` text mirrors the README)

| Flag | Meaning |
|---|---|
| `--provider <NAME>` | Provider name from `models.json` |
| `--model <ID>` | Model id to use (see `--list-models`) |
| `--mock` | Built-in mock provider; conflicts with `--provider`, `--model`, `--list-models` |
| `--list-models` | Print the selected provider's models and exit |
| `-h, --help` / `-V, --version` | clap-standard |
| `--continue` | Resume the most recent session for this directory |
| `-r, --resume` | Interactive session picker — arrow list with type-to-filter; refused under `--print` |
| `--session <PATH\|ID>` | Open a session by file path or (partial) id — path shape wins, then exact id, then id prefix in this project, then across all projects |
| `--fork <PATH\|ID>` | Copy a session into a new file for this directory (fresh id, `forkedFrom` pointer) and open the copy; the original is never written |
| `-n, --name <NAME>` | Name the new session (fresh start or fork); shown in the picker |
| `--print`, `-p` | Run one prompt headlessly, print the final reply, exit |
| `PROMPT` | Positional prompt; requires `--print` (piped stdin is prepended) |
| `--no-session` | Run without loading or saving a session file |

The four selection flags are mutually exclusive (pi's rules), and none of
them combines with `--mock` or `--no-session`. `--session` that resolves to
*another project's* file never silently appends to it: interactively you
get a y/N fork prompt (pi's), headlessly a hard error naming `--fork`. A
`--session`/`--fork` argument matching nothing is a hard error.

Session layout and the on-disk format are in [core.md](core.md).
| `--tools <BUNDLE>` | Built-in set: `none` \| `read-only` \| `coding` \| `all` (default `coding`, or `defaultTools` in settings) |

`after_help` points at `~/.config/hi/models.json` and the README schema.

## Startup flow (`main`)

```
--mock (interactive)
  └─ launch(MockProvider::default(), Status{mock,mock}, Catalog::default(),
            ModelsConfig::default(), SessionInit{no session, empty transcript})
     (the mock ignores prompts — nothing to resolve)

--print (headless, `print_mode`):
  1. prompt = combine_prompt(piped stdin, positional PROMPT)
       — neither present → "hi: no prompt…", exit FAILURE
  2. --mock → MockProvider; otherwise resolve_target() as below
  3. open_session() per flags → headless::run(...)
       (selection errors are hard: --resume refuses headless — use
        --session; cross-project --session refuses — use --fork)
  4. Ok(text) → println! + SUCCESS; Err → "hi: …" on stderr,
     stdout empty, FAILURE

otherwise (interactive):
  1. ModelsConfig::load()                  — failure → "hi: …create one
                                              (see the README) or run with --mock", exit FAILURE
  2. --list-models → selection::list_for   — prints "name:" + one id per line
  3. resolve_target: Settings::load() →    — selection::resolve(config,
       cli.provider, cli.model, settings.default_model)
  3. resolve_target: Settings::load() →    — selection::resolve(config,
       cli.provider, cli.model, settings.default_model);
       --tools flag, else settings.defaultTools, else `coding`
       → ToolBundle → ToolRegistry built at the cwd
  4. OpenAiCompatProvider::from_config(...)
       .with_system_prompt(resolve() + tool prompt notes)
       .with_thinking_level(settings.thinking)
       .with_tools(the same registry — wire and execution stay in sync)
  5. Catalog built once at startup:        — for each provider,
       entry.list_model_ids().ok() non-empty; failures/empty silently skipped
       (same 10s discovery timeout as --list-models)
  6. open_session per flags — hard errors for selection misses; disk
       failures degrade to a stderr warning. Afterwards
       stamp_session_cache_key re-stamps the provider with the session
       id (the prompt-cache routing key, AD-0007):
       --fork      → session::resolve + Session::fork_from + load copy
       --session   → resolve → resume (Local) | fork prompt (Global,
                     interactive) | refusal (Global, --print)
       --resume    → sessions_in + hi_tui::pick_session (interactive only)
       --continue  → session::latest + Session::resume
       otherwise   → Session::start(provider, model, thinking, name)
  7. launch(...) → hi_tui::run(...)
```

Note the asymmetry worth preserving: `system_prompt` (with tool notes woven
in) and the tool registry are resolved from files/settings at startup, but
`thinking` travels from settings; all three are re-attached to the provider
whenever the `/model` picker rebuilds it.

## Exit codes

- `ExitCode::SUCCESS` — normal end of session / successful listing
- `ExitCode::FAILURE` — config load failure, selection failure, listing
  failure, TUI error, headless turn failure, missing `--print` prompt,
  unknown `--tools` bundle;
  message printed as `hi: <message>` on stderr (`fail()`), so scripts
  can tell success from failure

## Headless mode (`--print`)

`--print` runs exactly one turn and exits — the scripting interface:

```sh
hi -p "Summarize this repository"       # positional prompt
git diff | hi -p "Review this change"   # piped stdin is prepended
hi -p --continue "and the follow-up?"   # resume this project's session
hi -p --session 4f2c "rerun that idea"  # open by partial id (headless-safe)
hi --fork 4f2c                          # copy it here, open the fork
hi -n "auth work"                       # start fresh, named (picker shows it)
hi -p --mock "ping"                     # no network, no config
hi -p --no-session "throwaway"          # don't touch the session dir
```

- The prompt is piped stdin (first) and/or the positional `PROMPT`
  (both may be present, joined by a newline); neither → exit FAILURE.
- Output contract (pi's): the final assistant text and nothing else on
  stdout; every diagnostic on stderr. A failed turn prints `hi: …` to
  stderr and exits FAILURE with stdout left empty, so
  `out=$(hi -p …)` never captures a half-answer.
- Sessions behave exactly as in the TUI: written by default,
  `--continue` keeps appending to the same file, `--no-session` skips.
  `--session <id|path>` and `--fork <id|path>` are the deterministic
  headless selectors — `--resume` needs a terminal for its picker and
  refuses under `--print`.
- The machinery is `hi_core::headless` — the same agent loop, tools,
  and session files as the TUI (see [core.md](core.md)).
- `--tools` composes: `hi -p --tools read-only "…"` runs headless with
  only the observation tools; `--tools none` restores a pure-text model.
## Where new startup behavior goes

- New flags → the `Cli` struct (clap derive handles help).
- New selection rules → `hi_core::selection` (never in `main`).
- New providers → construct here behind a config/flag branch; implement the
  `Provider` trait in `hi-core` (see [core.md](core.md)).
- Headless runs → `hi_core::headless` owns the drain; `print_mode` here
  only assembles the prompt, session, and exit code. JSONL event output
  (`--mode json`) is issue #18; auto-print on non-TTY streams is #19.
- New built-in tools → `crates/core/src/tools/` (one file per tool, the
  truncation contract in `truncate.rs`); bundles and selection stay in
  `ToolBundle` (see [core.md](core.md)).
