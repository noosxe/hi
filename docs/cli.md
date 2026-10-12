# `hi` CLI reference

Files this doc describes: `crates/cli/` (`Cargo.toml`, `src/main.rs`). The
binary is thin on purpose: it decides *what* runs (mock, TUI, model listing)
while `hi-core` resolves and `hi-tui` draws. A future headless mode branches
here without touching either library.

## Flags (clap derive, `--help` text mirrors the README)

| Flag | Meaning |
|---|---|
| `--provider <NAME>` | Provider name from `models.json` |
| `--model <ID>` | Model id to use (see `--list-models`) |
| `--mock` | Built-in mock provider; conflicts with `--provider`, `--model`, `--list-models` |
| `--list-models` | Print the selected provider's models and exit |
| `-h, --help` / `-V, --version` | clap-standard |
| `--continue` | Resume the most recent session for this directory (conflicts with `--mock`) |

`after_help` points at `~/.config/hi/models.json` and the README schema.

## Startup flow (`main`)

```
--mock
  └─ launch(MockProvider::default(), Status{mock,mock}, Catalog::default(),
  └─ launch(MockProvider::default(), Status{mock,mock}, Catalog::default(),
            ModelsConfig::default(), SessionInit{no session, empty transcript})
     (the mock ignores prompts — nothing to resolve)

otherwise:
  1. ModelsConfig::load()                  — failure → "hi: …create one
                                              (see the README) or run with --mock", exit FAILURE
  2. --list-models → selection::list_for   — prints "name:" + one id per line
  3. Catalog built once at startup:        — for each provider,
       entry.list_model_ids().ok() non-empty; failures/empty silently skipped
       (same 10s discovery timeout as --list-models)
  4. Settings::load() → default_model      — selection::resolve(config,
       cli.provider, cli.model, settings.default_model)
  5. OpenAiCompatProvider::from_config(...)
       .with_system_prompt(system_prompt::resolve())
       .with_thinking_level(settings.thinking)
  6. launch(...) → hi_tui::run(...)
 6. Session file: `--continue` → session::latest + Session::resume
       (warn + fresh start when nothing to resume); otherwise Session::start
       — failures degrade to a stderr warning either way
 7. launch(...) → hi_tui::run(...)
```

Note the asymmetry worth preserving: `system_prompt` is resolved from the
file chain at startup, but `thinking` travels from settings; both are
re-attached to the provider whenever the `/model` picker rebuilds it.

## Exit codes

- `ExitCode::SUCCESS` — normal end of session / successful listing
- `ExitCode::FAILURE` — config load failure, selection failure, listing
  failure, TUI error; message printed as `hi: <message>` on stderr
  (`fail()`), so scripts can tell success from failure

## Where new startup behavior goes

- New flags → the `Cli` struct (clap derive handles help).
- New selection rules → `hi_core::selection` (never in `main`).
- New providers → construct here behind a config/flag branch; implement the
  `Provider` trait in `hi-core` (see [core.md](core.md)).
- A headless `--print` mode → a second branch in `main`, linking no `hi-tui`
  code paths at runtime (the dependency stays but is inert).
