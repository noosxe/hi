//! The `hi` binary: flags, dispatch, exit codes.
//!
//! Thin on purpose. It decides *what* runs — mock, TUI, model listing —
//! while hi-core does the resolving and hi-tui does the drawing. A
//! future headless mode (`--print`, or subcommands) branches here
//! without touching either crate.

use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;

use hi_core::{MockProvider, ModelsConfig, OpenAiCompatProvider, Provider, Settings};
use hi_tui::{Status, run};

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

    let settings = match Settings::load() {
        Ok(settings) => settings,
        Err(err) => return fail(&err),
    };
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
    launch(provider, status)
}

/// Hand the session to the interactive interface.
fn launch(provider: Arc<dyn Provider>, status: Status) -> ExitCode {
    match run(provider, status) {
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
