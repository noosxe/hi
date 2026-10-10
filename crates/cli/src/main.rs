//! The `hi` binary: flags, dispatch, exit codes.
//!
//! Thin on purpose. It decides *what* runs — mock, TUI, model listing —
//! while hi-core does the resolving and hi-tui does the drawing. A
//! future headless mode (`--print`, or subcommands) branches here
//! without touching either crate.

use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;

use hi_core::{
    MockProvider, ModelsConfig, OpenAiCompatProvider, Provider, Settings, ThinkingLevel,
};
use hi_tui::{Catalog, Status, run};

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
}

fn main() -> ExitCode {
    let cli = Cli::parse();

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
            None,
            None,
        );
    }

    let config = match ModelsConfig::load() {
        Ok(config) => config,
        Err(err) => {
            return fail(&format!("{err}\n  create one (see the README) or run with --mock"));
        }
    };

    if cli.list_models {
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

    // The `/model` picker's menu, gathered once at startup (each
    // provider bounded by the 10s discovery timeout). Providers that
    // fail or list nothing simply don't appear; their errors are
    // visible through --list-models.
    let catalog = Catalog {
        entries: config
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

    let settings = match Settings::load() {
        Ok(settings) => settings,
        Err(err) => return fail(&err),
    };

    // The system prompt: built-in default, replaced/appended by
    // SYSTEM.md / APPEND_SYSTEM.md at the user and project levels.
    let system_prompt = Some(hi_core::system_prompt::resolve());
    let thinking = settings.thinking;
    let selected = match hi_core::selection::resolve(
        &config,
        cli.provider.as_deref(),
        cli.model.as_deref(),
        settings.default_model.as_deref(),
    ) {
        Ok(selected) => selected,
        Err(err) => return fail(&err),
    };
    let entry = &config.providers[&selected.provider];
    let provider = Arc::new(OpenAiCompatProvider::from_config(entry, selected.model.clone()));
    let status = Status {
        provider: selected.provider,
        model: selected.model,
    };
    launch(provider, status, catalog, config, system_prompt, thinking)
}

/// Hand the session to the interactive interface.
fn launch(
    provider: Arc<dyn Provider>,
    status: Status,
    catalog: Catalog,
    config: ModelsConfig,
    system_prompt: Option<String>,
    thinking: Option<ThinkingLevel>,
) -> ExitCode {
    match run(provider, status, catalog, config, system_prompt, thinking) {
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
