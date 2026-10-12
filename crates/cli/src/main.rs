//! The `hi` binary: flags, dispatch, exit codes.
//!
//! Thin on purpose. It decides *what* runs — mock, TUI, model listing —
//! while hi-core does the resolving and hi-tui does the drawing. A
//! Thin on purpose. It decides *what* runs — mock, TUI, headless
//! (`--print`), model listing — while hi-core resolves and hi-tui draws.

use std::io::Read;
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;

use hi_core::{
    Message, MockProvider, ModelsConfig, OpenAiCompatProvider, Provider, Session, Settings,
    ThinkingLevel, ToolRegistry, headless,
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

    /// Use the built-in mock provider — no config, no network
    #[arg(long, conflicts_with_all = ["provider", "model", "list_models"])]
    mock: bool,

    /// Print the selected provider's models and exit
    #[arg(long)]
    list_models: bool,

    /// Resume the most recent session for this directory
    #[arg(long = "continue", conflicts_with = "mock")]
    resume: bool,
    /// Run one prompt headlessly: print the final reply and exit
    #[arg(short = 'p', long)]
    print: bool,

    /// The prompt for --print (piped stdin is prepended)
    #[arg(requires = "print")]
    prompt: Option<String>,

    /// Run without loading or saving a session file
    #[arg(long, conflicts_with = "resume")]
    no_session: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

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

    if cli.print {
        return print_mode(&cli);
    }

    if cli.mock {
        return launch(
            Arc::new(MockProvider::default()),
            Status {
                provider: "mock".into(),
                model: "mock".into(),
            },
            Catalog::default(),
            ModelsConfig::default(),
            // The mock ignores system prompts — nothing to resolve.
            SessionInit {
                system_prompt: None,
                thinking: None,
                tools: Arc::new(ToolRegistry::new()),
                messages: Vec::new(),
                session: None,
            },
        );
    }

    let target = match resolve_target(cli.provider.as_deref(), cli.model.as_deref()) {
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
    // Session persistence: every run writes a session file; --continue
    // reopens the newest one for this directory; --no-session skips the
    // disk entirely. Failures degrade to a warning — chat never depends
    // on the disk.
    let (messages, session) = open_session(
        cli.no_session,
        cli.resume,
        &cwd,
        &target.status.provider,
        &target.status.model,
        target.thinking.as_ref().map(|level| level.name()),
    );
    let init = SessionInit {
        system_prompt: target.system_prompt,
        thinking: target.thinking,
        tools: Arc::new(ToolRegistry::new()),
        messages,
        session,
    };
    launch(target.provider, target.status, catalog, target.config, init)
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
}

fn resolve_target(provider_flag: Option<&str>, model_flag: Option<&str>) -> Result<Target, String> {
    let config = ModelsConfig::load()
        .map_err(|err| format!("{err}\n  create one (see the README) or run with --mock"))?;
    let settings = Settings::load()?;
    let selected = hi_core::selection::resolve(
        &config,
        provider_flag,
        model_flag,
        settings.default_model.as_deref(),
    )?;
    let entry = &config.providers[&selected.provider];
    let system_prompt = Some(hi_core::system_prompt::resolve());
    let thinking = settings.thinking;
    // Attach here, not just in the /model picker: a fresh session's
    // first turn must honor SYSTEM.md and settings.thinking too.
    let provider = Arc::new(
        OpenAiCompatProvider::from_config(entry, selected.model.clone())
            .with_system_prompt(system_prompt.clone())
            .with_thinking_level(thinking),
    );
    Ok(Target {
        provider,
        status: Status {
            provider: selected.provider,
            model: selected.model,
        },
        config,
        system_prompt,
        thinking,
    })
}

/// `--print`: one prompt in, final assistant text out, no interface.
/// Errors go to stderr with stdout left empty (pi's contract), so
/// `out=$(hi -p …)` never captures a half-answer.
fn print_mode(cli: &Cli) -> ExitCode {
    let prompt = match headless::combine_prompt(read_piped_stdin(), cli.prompt.clone()) {
        Ok(prompt) => prompt,
        Err(err) => return fail(&err),
    };

    let (provider, status, thinking) = if cli.mock {
        (
            Arc::new(MockProvider::default()) as Arc<dyn Provider>,
            Status {
                provider: "mock".into(),
                model: "mock".into(),
            },
            None,
        )
    } else {
        match resolve_target(cli.provider.as_deref(), cli.model.as_deref()) {
            Ok(target) => (target.provider, target.status, target.thinking),
            Err(err) => return fail(&err),
        }
    };

    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(err) => return fail(&format!("cannot determine current directory: {err}")),
    };
    let (mut history, session) = open_session(
        cli.no_session,
        cli.resume,
        &cwd,
        &status.provider,
        &status.model,
        thinking.as_ref().map(|level| level.name()),
    );

    match headless::run(
        provider,
        Arc::new(ToolRegistry::new()),
        None, // gate: permission UX arrives with the extension host
        &mut history,
        &prompt,
        session,
    ) {
        Ok(text) => {
            println!("{text}");
            ExitCode::SUCCESS
        }
        Err(reason) => fail(&reason),
    }
}

/// Stdin contents when piped or redirected; `None` on a terminal or on
/// a read error (an unusable stdin must not hang waiting for bytes).
fn read_piped_stdin() -> Option<String> {
    use std::io::IsTerminal;
    if std::io::stdin().is_terminal() {
        return None;
    }
    let mut piped = String::new();
    std::io::stdin().read_to_string(&mut piped).ok()?;
    Some(piped)
}

/// Open (or skip) the session file for this run — shared by the TUI and
/// headless paths. `no_session` wins; failures degrade to a warning and
/// an in-memory-only run.
fn open_session(
    no_session: bool,
    resume: bool,
    cwd: &Path,
    provider: &str,
    model: &str,
    thinking: Option<&str>,
) -> (Vec<Message>, Option<Session>) {
    if no_session {
        return (Vec::new(), None);
    }
    let opened = if resume {
        open_resumed(cwd)
    } else {
        Session::start(provider, model, thinking).map(|session| (Vec::new(), Some(session)))
    };
    opened.unwrap_or_else(|warning| {
        eprintln!("hi: {warning}");
        (Vec::new(), None)
    })
}

/// Reopen the newest session file for `cwd` and prepare it for
/// continued appending. A system note marks the seam in the transcript.
fn open_resumed(cwd: &std::path::Path) -> Result<(Vec<Message>, Option<Session>), String> {
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
    Ok((messages, Some(session)))
}

/// Hand the session to the interactive interface.
fn launch(
    provider: Arc<dyn Provider>,
    status: Status,
    catalog: Catalog,
    config: ModelsConfig,
    init: SessionInit,
) -> ExitCode {
    match run(provider, status, catalog, config, init) {
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
