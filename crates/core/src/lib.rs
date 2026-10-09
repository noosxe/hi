//! The brain of the `hi` agent harness.
//!
//! Provider traits, tool traits, and the agent event loop belong here.
//! The one rule for this crate: it never touches the terminal. That
//! keeps the harness usable headless (tests, CLI, daemons) later.
pub mod config;
pub mod event;
pub mod message;
pub mod mock;
pub mod openai;
pub mod provider;

pub use config::{ModelConfig, ModelsConfig, ProviderConfig};
pub use event::Event;
pub use message::Message;
pub use mock::MockProvider;
pub use openai::OpenAiCompatProvider;
pub use provider::{Provider, spawn_turn};

/// The program's name, shared by every crate in the workspace.
pub const NAME: &str = "hi";
