//! The brain of the `hi` agent harness.
//!
//! Provider traits, tool traits, and the agent event loop belong here.
//! The one rule for this crate: it never touches the terminal. That
//! keeps the harness usable headless (tests, CLI, daemons) later.
pub mod agent;
pub mod config;
pub mod event;
pub mod headless;
pub mod image_util;
pub mod message;
pub mod mock;
pub mod openai;
pub(crate) mod process;
pub mod provider;
pub mod selection;
pub mod session;
pub mod settings;
pub mod subagent;
pub mod system_prompt;
pub mod tasks;
pub mod thinking;
pub mod tool;
pub mod tools;

#[cfg(test)]
pub(crate) mod test_support;

pub use agent::{DEFAULT_MAX_TOOL_ROUNDS, spawn_agent_turn};
pub use config::{ModelConfig, ModelsConfig, ProviderConfig};
pub use event::Event;
pub use message::Message;
pub use mock::MockProvider;
pub use openai::OpenAiCompatProvider;
pub use provider::{Provider, spawn_turn};
pub use session::{LoadedSession, Session, SessionHeader};
pub use settings::Settings;
pub use thinking::ThinkingLevel;

pub use tool::{Tool, ToolAnnotations, ToolCall, ToolDecision, ToolGate, ToolRegistry, ToolResult};
pub use tools::{ImageGate, ToolBundle};
/// The program's name, shared by every crate in the workspace.
pub const NAME: &str = "hi";
