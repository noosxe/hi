//! The brain of the `hi` agent harness.
//!
//! Provider traits, tool traits, and the agent event loop will live here
//! eventually. The one rule for this crate: it never touches the terminal.
//! That keeps the harness usable headless (tests, CLI, daemons) later.
pub mod message;
pub use message::Message;

/// The program's name, shared by every crate in the workspace.
pub const NAME: &str = "hi";
