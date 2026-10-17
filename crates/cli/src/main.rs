//! The `hi` binary: flags, dispatch, exit codes.
//!
//! Thin on purpose. It decides *what* runs — mock, TUI, headless
//! (`--print`), model listing — while hi-core resolves and hi-tui draws.

use std::io::IsTerminal;
use std::io::Read;
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Parser, ValueEnum};

use hi_core::{
    ImageGate, Message, MockProvider, ModelsConfig, OpenAiCompatProvider, Provider, Session,
    Settings, ThinkingLevel, ToolBundle, ToolRegistry, event::Usage, headless,
};
use hi_tui::{Catalog, SessionInit, Status, run};

#[derive(Parser)]
#[command(
    name = "hi",
    version,
    about = "A minimalist agent harness",
    after_help = "Config: ~/.config/hi/models.json — see the README for the schema."
)]
struct Cli {
    /// Provider name from models.json
    #[arg(long)]
    provider: Option<String>,

    /// Model id to use (see --list-models)
    #[arg(long)]
    model: Option<String>,

    /// Built-in tool bundle: none | read-only | coding | all
    /// (default: coding, or defaultTools in settings.json)
    #[arg(long)]
    tools: Option<String>,

    /// Use the built-in mock provider — no config, no network
    #[arg(
        long,
        conflicts_with_all = [
            "provider", "model", "list_models", "continue_last", "resume_picker", "session", "fork",
        ]
    )]
    mock: bool,

    /// Print the selected provider's models and exit
    #[arg(long)]
    list_models: bool,

    /// Resume the most recent session for this directory
    #[arg(
        long = "continue",
        conflicts_with_all = ["mock", "no_session", "resume_picker", "session", "fork", "name"]
    )]
    continue_last: bool,

    /// Pick a session to resume (arrow list with type-to-filter)
    #[arg(
        short = 'r',
        long = "resume",
        conflicts_with_all = ["mock", "no_session", "continue_last", "session", "fork"]
    )]
    resume_picker: bool,

    /// Open a session by file path or (partial) id
    #[arg(
        long,
        conflicts_with_all = ["mock", "no_session", "continue_last", "resume_picker", "fork", "name"]
    )]
    session: Option<String>,

    /// Fork a session (path or id) into a new file for this directory
    #[arg(
        long,
        conflicts_with_all = ["mock", "no_session", "continue_last", "resume_picker", "session"]
    )]
    fork: Option<String>,

    /// Name the new session (fresh start or fork)
    #[arg(short = 'n', long, conflicts_with_all = ["continue_last", "session"])]
    name: Option<String>,

    /// Run one prompt headlessly: print the final reply and exit
    #[arg(short = 'p', long)]
    print: bool,

    /// Headless output mode: json streams JSONL records (#18)
    #[arg(long, value_enum)]
    mode: Option<Mode>,

    /// The prompt for --print or --mode (piped stdin is prepended)
    prompt: Option<String>,

    /// Run without loading or saving a session file
    #[arg(
        long,
        conflicts_with_all = ["continue_last", "resume_picker", "session", "fork"]
    )]
    no_session: bool,
}

/// Headless output modes (`--mode`). Only json exists: it streams one
/// JSONL record per event instead of the final text (#18).
#[derive(Clone, Copy, PartialEq, ValueEnum)]
enum Mode {
    Json,
}

fn main() -> ExitCode {
    dispatch(Cli::parse())
}

/// Everything `main` does after parsing: validation, dispatch by flag,
/// and launch. An own function so the dispatch arms are testable —
/// `main` itself would run the TUI. Launching stays `hi_tui::run`.
fn dispatch(cli: Cli) -> ExitCode {
    // Validate before any dispatch: the mock paths never resolve a target,
    // so a bad --tools value must still fail loudly there.
    if let Some(raw) = cli.tools.as_deref()
        && let Err(err) = ToolBundle::parse(raw)
    {
        return fail(&err);
    }

    if cli.list_models {
        let config = match ModelsConfig::load() {
            Ok(config) => config,
            Err(err) => {
                return fail(&format!(
                    "{err}\n  create one (see the README) or run with --mock"
                ));
            }
        };
        return match hi_core::selection::list_for(&config, cli.provider.as_deref()) {
            Ok((name, models)) => {
                println!("{name}:");
                for id in models {
                    println!("  {id}");
                }
                ExitCode::SUCCESS
            }
            Err(err) => fail(&err),
        };
    }

    // pi-parity auto-print (#19): a redirected stream is a script's
    // context, so with no explicit mode the text path runs instead of
    // a TUI that could not work there.
    let auto_print = !cli.print
        && cli.mode.is_none()
        && !streams_look_interactive(
            std::io::stdin().is_terminal(),
            std::io::stdout().is_terminal(),
        );

    // `[PROMPT]` feeds the headless surfaces; an interactive run has
    // no use for it and must not swallow it silently.
    if cli.prompt.is_some() && !cli.print && !auto_print && cli.mode.is_none() {
        return fail("[PROMPT] needs --print or --mode json");
    }

    if cli.print || auto_print {
        return print_mode(&cli, false);
    }

    if cli.mode == Some(Mode::Json) {
        return print_mode(&cli, true);
    }

    if cli.mock {
        return launch(
            Arc::new(MockProvider::default()),
            Status {
                provider: "mock".into(),
                model: "mock".into(),
                tool_count: 0,
            },
            Catalog::default(),
            ModelsConfig::default(),
            // The mock ignores system prompts — nothing to resolve.
            // The runner is inert here (empty registry, no subagent
            // tool), but the field is mandatory.
            SessionInit {
                system_prompt: None,
                thinking: None,
                tools: Arc::new(ToolRegistry::new()),
                messages: Vec::new(),
                session: None,
                usage: Usage::default(),
                usage_last_prompt: 0,
                images: ImageGate::new(false),
                images_setting: true,
                tasks: None,
                task_turns: false,
                // The mock ignores config — built-in default cap.
                max_tool_rounds: hi_core::DEFAULT_MAX_TOOL_ROUNDS,
                subagent: hi_core::subagent::SubagentRunner::new(
                    ModelsConfig::default(),
                    std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir()),
                    false,
                    None,
                ),
            },
            // The mock never touches the wire — nothing to attribute.
            false,
        );
    }

    let mut target = match resolve_target(
        cli.provider.as_deref(),
        cli.model.as_deref(),
        cli.tools.as_deref(),
    ) {
        Ok(target) => target,
        Err(err) => return fail(&err),
    };

    // The `/model` picker's menu, gathered once at startup (each
    // provider bounded by the 10s discovery timeout). Providers that
    // fail or list nothing simply don't appear; their errors are
    // visible through --list-models.
    let catalog = Catalog {
        entries: target
            .config
            .providers
            .iter()
            .filter_map(|(name, entry)| {
                entry
                    .list_model_ids()
                    .ok()
                    .filter(|models| !models.is_empty())
                    .map(|models| (name.clone(), models))
            })
            .collect(),
    };

    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(err) => return fail(&format!("cannot determine current directory: {err}")),
    };
    // Session persistence: every run plans a session file, but the
    // file only materializes on the first recorded record — an opened
    // and abandoned run leaves nothing behind. --continue reopens the
    // newest one for this directory; --no-session skips the disk
    // entirely. Failures degrade to a warning — chat never depends on
    // the disk.
    let opened = open_session(
        &cli,
        true,
        &cwd,
        &target.status.provider,
        &target.status.model,
        target.thinking.as_ref().map(|level| level.name()),
    );
    // Backing out of a picker or prompt means "nothing to do", not
    // failure — the same contract as pi's "No session selected".
    let opened = match opened {
        Ok(opened) => opened,
        Err(err) => return fail(&err),
    };
    let Some(opened) = opened else {
        return ExitCode::SUCCESS;
    };
    let (messages, session) = (opened.messages, opened.session);
    let (usage, usage_last_prompt) = (opened.usage, opened.usage_last_prompt);
    if let Some(session) = session.as_ref() {
        // Identity first: the stamp's provider rebuild must see it.
        apply_session_identity(&mut target, session);
        stamp_session_cache_key(&mut target, session);
    }
    let init = SessionInit {
        system_prompt: target.system_prompt,
        thinking: target.thinking,
        tools: target.tools,
        messages,
        session,
        usage,
        usage_last_prompt,
        max_tool_rounds: target.max_tool_rounds,
        images: target.images,
        images_setting: target.images_setting,
        tasks: target.tasks,
        task_turns: target.task_turns,
        subagent: target.subagent,
    };
    launch(
        target.provider,
        target.status,
        catalog,
        target.config,
        init,
        target.telemetry,
    )
}

/// Everything a run needs once the interface is chosen: the resolved
/// provider (with system prompt and thinking attached), its status line,
/// the config it came from, and the settings-derived extras.
struct Target {
    provider: Arc<dyn Provider>,
    status: Status,
    config: ModelsConfig,
    system_prompt: Option<String>,
    thinking: Option<ThinkingLevel>,
    /// The registry built from the selected bundle, handed to SessionInit
    /// and headless runs.
    tools: Arc<ToolRegistry>,
    /// The user's attribution opt-in, resolved once here so provider
    /// rebuilds (the session cache-key stamp, the TUI's /model and
    /// /resume switches) never drop it.
    telemetry: bool,
    /// The main-loop tool-round cap: the `settings.maxToolRounds`
    /// override or the built-in default. Resolved once here so the TUI
    /// and the headless paths agree.
    max_tool_rounds: usize,
    /// The shared image gate (inside the registry's `read`), plus the
    /// settings master switch — handed to SessionInit so the TUI's
    /// /model switch re-resolves the policy (AD-0012).
    images: hi_core::ImageGate,
    images_setting: bool,
    /// The `settings.taskTurns` opt-in, resolved once (#72) so the
    /// TUI's trigger honors the human's choice for the whole session.
    task_turns: bool,
    /// The background-task manager the registry's `task_*` tools share;
    /// the TUI drains its completion channel per frame (#5, AD-0019).
    /// Headless runs hold it only here, so drop = exit = groups killed.
    tasks: Option<hi_core::tasks::TaskManager>,
    /// The nested-agent runner (#4) the registry's `subagent` tool
    /// shares; handed to SessionInit so the TUI's provider rebuilds
    /// (/model, /resume) restamp the inherited identity.
    subagent: hi_core::subagent::SubagentRunner,
}

/// Fill the selected model's missing context/output fields from the
/// live fetch. The fetch is injected so the merge is testable without
/// the network; the real one lives in hi-core (the cli ships neither
/// serde nor an HTTP client, AD-0004). Gaps only — explicit config wins.
fn enrich_from_live(
    provider: &mut hi_core::config::ProviderConfig,
    model_id: &str,
    fetch: impl FnOnce() -> Option<Vec<hi_core::catalog::LiveRow>>,
) {
    let Some(row) = fetch().and_then(|rows| rows.into_iter().find(|r| r.id == model_id)) else {
        return;
    };
    let Some(model) = provider.models.iter_mut().find(|m| m.id == model_id) else {
        return;
    };
    if model.context_window.is_none() {
        model.context_window = Some(u32::try_from(row.context_window).unwrap_or(u32::MAX));
    }
    if model.max_tokens.is_none() {
        model.max_tokens = row.max_output.and_then(|v| u32::try_from(v).ok());
    }
}

fn resolve_target(
    provider_flag: Option<&str>,
    model_flag: Option<&str>,
    tools_flag: Option<&str>,
) -> Result<Target, String> {
    let mut config = ModelsConfig::load()
        .map_err(|err| format!("{err}\n  create one (see the README) or run with --mock"))?;
    let settings = Settings::load()?;
    // Opt-in session retention (#23) — before any session opens, so a
    // run never prunes the file it is about to write. Best-effort; one
    // stderr line only when something was removed.
    if let Some(days) = settings.session_retention
        && let Some(root) = hi_core::session::sessions_dir()
    {
        let removed = hi_core::session::prune_sessions(
            &root,
            std::time::Duration::from_secs(days.saturating_mul(24 * 3600)),
        );
        if removed > 0 {
            eprintln!(
                "session prune: removed {removed} file(s) from {}",
                root.display()
            );
        }
    }
    let selected = hi_core::selection::resolve(
        &config,
        provider_flag,
        model_flag,
        settings.default_model.as_deref(),
    )?;
    // Tier-2 metadata (AD-0044): when the selected provider *is*
    // OpenRouter, a best-effort live refresh tops up context/output
    // for the selected model — fresher than the shipped snapshot,
    // silent when the network or the shape fails. Config fields the
    // user set still win (enrich fills gaps only).
    if let Some(provider) = config.providers.get_mut(&selected.provider)
        && provider.base_url.contains("openrouter.ai")
    {
        let model = selected.model.clone();
        enrich_from_live(provider, &model, || {
            hi_core::catalog::fetch_live_rows(std::time::Duration::from_secs(3))
        });
    }
    let entry = &config.providers[&selected.provider];
    let bundle = match tools_flag {
        Some(value) => ToolBundle::parse(value)?,
        None => match settings.default_tools.as_deref() {
            Some(value) => ToolBundle::parse(value)?,
            None => ToolBundle::default(),
        },
    };
    let cwd = std::env::current_dir()
        .map_err(|err| format!("cannot determine current directory: {err}"))?;
    // Image policy (AD-0012, pi-parity): the master switch first,
    // then the model's rule — configured models honor an explicit
    // `vision: false`, discovered models (no entry) read as
    // vision-capable. The gate is shared: /model switches re-resolve
    // it without rebuilding the registry.
    let images_setting = settings.images_enabled();
    let task_turns = settings.task_turns_enabled();
    let images = ImageGate::new(entry.images_allowed(&selected.model, images_setting));
    let tasks = hi_core::tasks::tasks_dir().map(hi_core::tasks::TaskManager::new);
    // The nested-agent runner (#4): restamped on every provider rebuild
    // so inherits follow /model and /resume.
    let subagent = hi_core::subagent::SubagentRunner::new(
        config.clone(),
        cwd.clone(),
        settings.telemetry_enabled(),
        // The `settings.subagentTranscripts` opt-in (#116): record each
        // run as its own session file under the project's directory.
        if settings.subagent_transcripts {
            hi_core::session::sessions_dir()
        } else {
            None
        },
    );
    // The `subagents` master switch (AD-0043): off = the tool is
    // absent from the registry entirely — no schema entry, no call
    // to reject (ruling 2). Built once here; the TUI shares this
    // registry via SessionInit and never rebuilds it, so no
    // SessionInit plumbing is needed.
    let subagent_tool = if settings.subagents_enabled() {
        Some(subagent.clone())
    } else {
        None
    };
    let tools = Arc::new(bundle.registry(&cwd, images.clone(), tasks.clone(), subagent_tool));
    let system_prompt = {
        let mut prompt = hi_core::system_prompt::resolve();
        let notes = tools.prompt_notes();
        if !notes.is_empty() {
            prompt.push_str("\n\n# Tool notes\n");
            prompt.push_str(notes.trim_end());
        }
        Some(prompt)
    };
    let thinking = settings.thinking;
    // Attach here, not just in the /model picker: a fresh session's
    // first turn must honor SYSTEM.md and settings.thinking too.
    // The registry rides in two places: with_tools puts the definitions on
    // the wire; the same Arc drives execution inside spawn_agent_turn.
    let provider = Arc::new(
        OpenAiCompatProvider::from_config(entry, selected.model.clone())
            .with_system_prompt(system_prompt.clone())
            .with_thinking_level(thinking)
            .with_tools(Arc::clone(&tools))
            .with_telemetry(settings.telemetry_enabled()),
    );
    subagent.stamp(selected.provider.clone(), selected.model.clone());
    Ok(Target {
        provider,
        status: Status {
            provider: selected.provider,
            model: selected.model,
            tool_count: tools.len(),
        },
        config,
        system_prompt,
        thinking,
        tools,
        telemetry: settings.telemetry_enabled(),
        max_tool_rounds: settings
            .max_tool_rounds
            .unwrap_or(hi_core::DEFAULT_MAX_TOOL_ROUNDS),
        images,
        images_setting,
        tasks,
        task_turns,
        subagent,
    })
}

/// Stamp the open session's id onto the provider as the prompt-cache
/// routing key. The provider is built in `resolve_target` — before a
/// session exists — so the stamp rebuilds it once; nothing shares the
/// `Arc` yet, so no live thread sees the swap. Provider rebuilds in
/// the TUI (`/model`, `/resume`) re-apply both stamps themselves.
/// AD-0045 + AD-0046: the session's recorded identity outranks the
/// startup selection on resume — the thinking level, and the
/// provider/model when still available. A vanished model falls back
/// to the selection with a stderr warning. Runs before the provider
/// rebuild so the rebuilt provider sees the restored identity.
fn apply_session_identity(target: &mut Target, session: &Session) {
    if let Some(level) = session
        .thinking()
        .and_then(hi_core::thinking::ThinkingLevel::parse)
    {
        target.thinking = Some(level);
    }
    let (recorded_provider, recorded_model) = (session.provider(), session.model());
    if recorded_provider != target.status.provider || recorded_model != target.status.model {
        if hi_core::selection::model_available(&target.config, recorded_provider, recorded_model) {
            target.status.provider = recorded_provider.to_string();
            target.status.model = recorded_model.to_string();
        } else {
            eprintln!(
                "hi: session recorded model {recorded_provider}/{recorded_model} — not available; using {}",
                target.status.model
            );
        }
    }
}

fn stamp_session_cache_key(target: &mut Target, session: &Session) {
    let Some(entry) = target.config.providers.get(&target.status.provider) else {
        return; // mock or unknown provider — nothing on the wire to stamp
    };
    target
        .subagent
        .stamp(target.status.provider.clone(), target.status.model.clone());
    target
        .subagent
        .stamp_session(Some(session.id().to_string()));
    target.provider = Arc::new(
        OpenAiCompatProvider::from_config(entry, target.status.model.clone())
            .with_system_prompt(target.system_prompt.clone())
            .with_thinking_level(target.thinking)
            .with_tools(Arc::clone(&target.tools))
            .with_telemetry(target.telemetry)
            .with_cache_key(Some(session.id().to_string())),
    );
}

/// `--print`: one prompt in, final assistant text out, no interface.
/// Errors go to stderr with stdout left empty (pi's contract), so
/// `out=$(hi -p …)` never captures a half-answer. `--mode json` runs
/// the same turn with a JSONL sink instead: one record per event on
/// stdout, ending in `done` or `failed` (#18).
fn print_mode(cli: &Cli, json: bool) -> ExitCode {
    let prompt = match headless::combine_prompt(read_piped_stdin(), cli.prompt.clone()) {
        Ok(prompt) => prompt,
        Err(err) => return fail(&err),
    };

    // Kept whole: the session stamp below must rebuild the provider,
    // which needs the config entry and system prompt alongside it.
    let mut target = if cli.mock {
        None
    } else {
        Some(
            match resolve_target(
                cli.provider.as_deref(),
                cli.model.as_deref(),
                cli.tools.as_deref(),
            ) {
                Ok(target) => target,
                Err(err) => return fail(&err),
            },
        )
    };
    let (status, thinking, tools) = match &target {
        Some(target) => (
            target.status.clone(),
            target.thinking,
            Arc::clone(&target.tools),
        ),
        None => (
            Status {
                provider: "mock".into(),
                model: "mock".into(),
                tool_count: 0,
            },
            None,
            // The mock never emits tool calls — keep its registry empty.
            Arc::new(ToolRegistry::new()),
        ),
    };

    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(err) => return fail(&format!("cannot determine current directory: {err}")),
    };
    let opened = open_session(
        cli,
        false,
        &cwd,
        &status.provider,
        &status.model,
        thinking.as_ref().map(|level| level.name()),
    );
    // `Ok(None)` cannot happen headless (the picker refuses first); the
    // arm exists so both dispatch sites share one open_session contract.
    let opened = match opened {
        Ok(opened) => opened,
        Err(err) => return fail(&err),
    };
    let Some(mut opened) = opened else {
        return ExitCode::SUCCESS;
    };
    // Headless seeds no bill — it prints none either; the record still
    // rides the session file (headless::run).
    let (mut history, session) = (opened.messages, opened.session.take());
    let _ = (opened.usage, opened.usage_last_prompt);

    if let (Some(target), Some(session)) = (target.as_mut(), session.as_ref()) {
        // The recorded identity first: the stamp below rebuilds the
        // provider and must see it (AD-0045, AD-0046).
        apply_session_identity(target, session);
        stamp_session_cache_key(target, session);
    }
    let max_tool_rounds = target
        .as_ref()
        .map(|t| t.max_tool_rounds)
        .unwrap_or(hi_core::DEFAULT_MAX_TOOL_ROUNDS);
    let provider: Arc<dyn Provider> = match target {
        Some(target) => target.provider,
        // The mock path: no wire, no stamp.
        None => Arc::new(MockProvider::default()),
    };
    let turn = if json {
        headless::run_with(
            provider,
            tools,
            None, // gate: permission UX arrives with the extension host
            &mut history,
            &prompt,
            session,
            max_tool_rounds,
            &mut |record| println!("{}", record.to_line()),
        )
    } else {
        headless::run(
            provider,
            tools,
            None,
            &mut history,
            &prompt,
            session,
            max_tool_rounds,
        )
    };
    match turn {
        Ok(text) => {
            if !json {
                println!("{text}");
            }
            ExitCode::SUCCESS
        }
        Err(reason) => fail(&reason),
    }
}

/// The TUI gate (#19): the interface opens only when BOTH streams are
/// a terminal — pi-parity. Any redirect (piped stdin like
/// `git diff | hi "review"`, redirected stdout like `hi "prompt" >
/// log.md`) is a script's context, and dispatch auto-selects text
/// print mode instead (ruling a). Explicit `--print`/`--mode` always
/// win; this only fills the no-mode-selected gap.
fn streams_look_interactive(stdin_is_tty: bool, stdout_is_tty: bool) -> bool {
    stdin_is_tty && stdout_is_tty
}

/// Stdin contents when piped or redirected; `None` on a terminal or on
/// a read error (an unusable stdin must not hang waiting for bytes).
fn read_piped_stdin() -> Option<String> {
    if std::io::stdin().is_terminal() {
        return None;
    }
    let mut piped = String::new();
    std::io::stdin().read_to_string(&mut piped).ok()?;
    Some(piped)
}

/// What a run opens with: the transcript to seed from, the file to
/// append to, and the replayed bill (#85). A fork inherits the
/// transcript, never the economics.
struct OpenedSession {
    messages: Vec<Message>,
    session: Option<Session>,
    usage: Usage,
    usage_last_prompt: u64,
}

impl OpenedSession {
    /// A run that starts from nothing — no transcript, no bill.
    fn fresh() -> Self {
        Self {
            messages: Vec::new(),
            session: None,
            usage: Usage::default(),
            usage_last_prompt: 0,
        }
    }
}

/// Resolve the session-selection flags into an open session — shared by
/// the TUI and headless paths. Selection failures (`--session` with no
/// match, cross-project under `--print`, the picker under `--print`)
/// are hard errors; plain disk failures degrade to a warning and an
/// in-memory-only run, as they always have. `Ok(None)` means the user
/// backed out of a picker or prompt — nothing to do, exit 0.
fn open_session(
    cli: &Cli,
    interactive: bool,
    cwd: &Path,
    provider: &str,
    model: &str,
    thinking: Option<&str>,
) -> Result<Option<OpenedSession>, String> {
    let name = cli.name.as_deref();
    if cli.no_session {
        return Ok(Some(OpenedSession::fresh()));
    }
    // --fork: copy, then run in the copy. The original is never opened
    // for write, and the copy carries this directory in its header.
    if let Some(arg) = cli.fork.as_deref() {
        let source = match hi_core::session::resolve(cwd, arg)? {
            hi_core::session::SessionRef::Local(path)
            | hi_core::session::SessionRef::Global { path, .. } => path,
            hi_core::session::SessionRef::NotFound => {
                return Err(format!("no session found matching '{arg}'"));
            }
        };
        let session = Session::fork_from(&source, cwd, name)?;
        let messages = hi_core::session::load(session.path())?.messages;
        return Ok(Some(OpenedSession {
            messages,
            session: Some(session),
            // A fork restarts the economics: transcript inherited,
            // bill not.
            ..OpenedSession::fresh()
        }));
    }
    // --session: open by path or id. Another project's file is offered
    // as a fork interactively (pi's prompt) and refused with the fork
    // hint headlessly — never silently appended to.
    if let Some(arg) = cli.session.as_deref() {
        return match hi_core::session::resolve(cwd, arg)? {
            hi_core::session::SessionRef::Local(path) => open_resumed(&path).map(Some),
            hi_core::session::SessionRef::Global { path, cwd: other } => {
                if !interactive {
                    Err(format!(
                        "session '{arg}' belongs to another project ({other}) — run `hi --fork '{arg}'` in this directory to copy it"
                    ))
                } else if confirm(&format!(
                    "session found in different project: {other}\nfork it into the current directory?"
                ))? {
                    let session = Session::fork_from(&path, cwd, name)?;
                    let messages = hi_core::session::load(session.path())?.messages;
                    Ok(Some(OpenedSession {
                        messages,
                        session: Some(session),
                        ..OpenedSession::fresh()
                    }))
                } else {
                    Ok(None) // declined — the original is untouched
                }
            }
            hi_core::session::SessionRef::NotFound => {
                Err(format!("no session found matching '{arg}'"))
            }
        };
    }
    // --resume: the interactive picker. Headless has no menu; the
    // deterministic selectors are --session and --continue.
    if cli.resume_picker {
        if !interactive {
            return Err(
                "--resume needs a terminal — use --session <id|path> or --continue under --print"
                    .into(),
            );
        }
        let summaries = hi_core::session::sessions_in(cwd)?;
        if summaries.is_empty() {
            return Err("no previous sessions in this directory".into());
        }
        return match hi_tui::pick_session(&summaries) {
            Some(path) => open_resumed(&path).map(Some),
            None => Ok(None),
        };
    }
    if cli.continue_last {
        return Ok(Some(open_resumed(cwd).unwrap_or_else(|warning| {
            eprintln!("hi: {warning}");
            OpenedSession::fresh()
        })));
    }
    // Fresh start, optionally named.
    Ok(Some(
        Session::start(provider, model, thinking, name)
            .map(|session| OpenedSession {
                session: Some(session),
                ..OpenedSession::fresh()
            })
            .unwrap_or_else(|warning| {
                eprintln!("hi: {warning}");
                OpenedSession::fresh()
            }),
    ))
}

/// A y/N stdin prompt — the cross-project fork offer (interactive mode
/// only; callers check first). Anything but y/yes is no.
fn confirm(question: &str) -> Result<bool, String> {
    use std::io::Write;
    print!("{question} [y/N] ");
    std::io::stdout()
        .flush()
        .map_err(|err| format!("cannot write prompt: {err}"))?;
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .map_err(|err| format!("cannot read answer: {err}"))?;
    Ok(matches!(answer.trim().to_lowercase().as_str(), "y" | "yes"))
}

/// Reopen the newest session file for `cwd` and prepare it for
/// continued appending. A system note marks the seam in the transcript.
fn open_resumed(cwd: &std::path::Path) -> Result<OpenedSession, String> {
    let loaded = hi_core::session::latest(cwd)?.ok_or("no previous session in this directory")?;
    let session = Session::resume(&loaded.path)?;
    let count = loaded.messages.len();
    let mut messages = loaded.messages;
    messages.insert(
        0,
        Message::System(format!(
            "resumed {count} messages from {}",
            loaded.path.display()
        )),
    );
    Ok(OpenedSession {
        messages,
        session: Some(session),
        usage: loaded.usage,
        usage_last_prompt: loaded.usage_last_prompt,
    })
}

/// Hand the session to the interactive interface.
fn launch(
    provider: Arc<dyn Provider>,
    status: Status,
    catalog: Catalog,
    config: ModelsConfig,
    init: SessionInit,
    telemetry: bool,
) -> ExitCode {
    match run(provider, status, catalog, config, init, telemetry) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => fail(&err.to_string()),
    }
}

/// Report a failure the way CLI tools do: program-name prefix and a
/// non-zero exit code, so scripts can tell success from failure.
fn fail(message: &str) -> ExitCode {
    eprintln!("hi: {message}");
    ExitCode::FAILURE
}

#[cfg(test)]
mod tests {
    use super::*;

    /// XDG variables are process-global; Rust runs tests on threads
    /// by default, so every env-mutating dispatch test takes this lock.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// A throwaway config/data root the dispatch tests can pollute.
    /// Both XDG variables point at it, so neither the user's real
    /// models.json nor their session store is ever touched.
    fn hermetic_root(name: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("hi-dispatch-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("hi")).unwrap();
        root
    }

    fn point_xdg_at(root: &std::path::Path) {
        // Edition 2024 marks set_var unsafe (same precedent as core's
        // config tests); ENV_LOCK makes the mutation race-free.
        unsafe { std::env::set_var("XDG_CONFIG_HOME", root) };
        unsafe { std::env::set_var("XDG_DATA_HOME", root) };
    }

    fn config_with_models(raw: &str) -> String {
        String::from(
            r#"{"providers": {"test-provider": {"baseUrl": "http://localhost:9/v1", "models": "#,
        ) + raw
            + r#"}}}"#
    }

    #[test]
    fn mock_headless_succeeds_without_any_config() {
        let _guard = ENV_LOCK.lock().unwrap();
        point_xdg_at(&hermetic_root("mock-headless"));
        let cli = Cli::try_parse_from(["hi", "--mock", "-p", "say something"]).unwrap();
        assert_eq!(dispatch(cli), ExitCode::SUCCESS);
    }

    #[test]
    fn headless_without_config_fails_with_guidance() {
        let _guard = ENV_LOCK.lock().unwrap();
        point_xdg_at(&hermetic_root("headless-no-config"));
        let cli = Cli::try_parse_from(["hi", "-p", "review this"]).unwrap();
        assert_eq!(dispatch(cli), ExitCode::FAILURE);
    }

    #[test]
    fn list_models_without_config_fails() {
        let _guard = ENV_LOCK.lock().unwrap();
        point_xdg_at(&hermetic_root("list-no-config"));
        let cli = Cli::try_parse_from(["hi", "--list-models"]).unwrap();
        assert_eq!(dispatch(cli), ExitCode::FAILURE);
    }

    #[test]
    fn list_models_prints_the_static_catalog_and_succeeds() {
        let _guard = ENV_LOCK.lock().unwrap();
        let root = hermetic_root("list-static");
        std::fs::write(
            root.join("hi/models.json"),
            config_with_models(r#"[{"id": "glm-4.6"}, {"id": "glm-5.3"}]"#),
        )
        .unwrap();
        point_xdg_at(&root);
        let cli = Cli::try_parse_from(["hi", "--list-models"]).unwrap();
        assert_eq!(dispatch(cli), ExitCode::SUCCESS);
    }

    #[test]
    fn resolve_target_rejects_an_unknown_tool() {
        let _guard = ENV_LOCK.lock().unwrap();
        let root = hermetic_root("unknown-tool");
        std::fs::write(
            root.join("hi/models.json"),
            config_with_models(r#"[{"id": "glm-4.6"}]"#),
        )
        .unwrap();
        point_xdg_at(&root);
        assert!(resolve_target(None, None, Some("nonexistent")).is_err());
    }

    #[test]
    fn subagents_off_drops_the_tool() {
        // AD-0043 ruling 2: off = absent from the registry — the model
        // never sees the tool, no schema entry, no call to reject.
        let _guard = ENV_LOCK.lock().unwrap();
        let root = hermetic_root("subagents-off");
        std::fs::write(
            root.join("hi/models.json"),
            config_with_models(r#"[{"id": "glm-4.6"}]"#),
        )
        .unwrap();
        std::fs::write(root.join("hi/settings.json"), r#"{"subagents": false}"#).unwrap();
        point_xdg_at(&root);
        let target = resolve_target(None, None, None).unwrap();
        assert!(target.tools.get("subagent").is_none());
    }

    #[test]
    fn subagents_default_on() {
        // Ruling 3 default: no settings key at all — the tool ships.
        let _guard = ENV_LOCK.lock().unwrap();
        let root = hermetic_root("subagents-default");
        std::fs::write(
            root.join("hi/models.json"),
            config_with_models(r#"[{"id": "glm-4.6"}]"#),
        )
        .unwrap();
        point_xdg_at(&root);
        let target = resolve_target(None, None, None).unwrap();
        assert!(target.tools.get("subagent").is_some());
    }

    #[test]
    fn fail_reports_and_fails() {
        assert_eq!(fail("boom"), ExitCode::FAILURE);
    }

    #[test]
    fn session_selection_flags_follow_pi_conflict_rules() {
        // The selectors are mutually exclusive.
        assert!(Cli::try_parse_from(["hi", "--continue", "--session", "a"]).is_err());
        assert!(Cli::try_parse_from(["hi", "--resume", "--fork", "a"]).is_err());
        assert!(Cli::try_parse_from(["hi", "--session", "a", "--fork", "b"]).is_err());
        assert!(Cli::try_parse_from(["hi", "--no-session", "--resume"]).is_err());
        assert!(Cli::try_parse_from(["hi", "--mock", "--continue"]).is_err());
        // --name pairs with fresh starts and forks, not with reopening.
        assert!(Cli::try_parse_from(["hi", "--name", "n", "--session", "a"]).is_err());
        assert!(Cli::try_parse_from(["hi", "--name", "n", "--continue"]).is_err());
        assert!(Cli::try_parse_from(["hi", "--fork", "a", "--name", "n"]).is_ok());
        // Headless selection: --session/--fork parse; --resume parses
        // too but is refused at dispatch (ruling: the error belongs to
        // open_session, where it can carry context).
        assert!(Cli::try_parse_from(["hi", "--session", "a", "--print", "q"]).is_ok());
        assert!(Cli::try_parse_from(["hi", "--resume", "--print", "q"]).is_ok());
    }
    #[test]
    fn the_tui_opens_only_when_both_streams_are_terminals() {
        // #19 ruling (a): any redirect is a script's context — only a
        // terminal on BOTH streams opens the interface.
        assert!(streams_look_interactive(true, true));
        assert!(!streams_look_interactive(true, false)); // stdout redirected
        assert!(!streams_look_interactive(false, true)); // piped stdin
        assert!(!streams_look_interactive(false, false)); // full pipe
    }
}
