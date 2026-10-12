//! The conversation message type.

use crate::tool::{ToolCall, ToolResult};
/// An enum rather than a struct-with-a-role-field because the payload
/// lives in the variant: the compiler guarantees you can never read
/// one speaker's data out of another's variant, and `match` over the
/// variants is exhaustive — the tool-call kinds leaned on exactly that:
/// every consumer must handle them at compile time.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    /// Something the human typed.
    User(String),
    /// Something the model produced.
    Assistant(String),
    /// The model asked to run a tool. Sits in the transcript between
    /// the assistant text that preceded it and the `ToolResult` that
    /// answers it — the agent loop always produces that pairing.
    ToolCall(ToolCall),
    /// The harness's answer to a `ToolCall` — a successful result, a
    /// tool failure, or a gate refusal (`is_error` distinguishes them
    /// for rendering; the model just sees content).
    ToolResult(ToolResult),
    /// A turn-level failure, shown in the transcript.
    Error(String),
    /// A harness announcement (e.g. "default model saved: …"), shown
    /// dim in the transcript.
    System(String),
}

impl Message {
    /// The text content, if this variant carries any. `ToolCall` and
    /// `ToolResult` carry structured payloads instead of prose, so they
    /// are `None` — render those from their fields.
    pub fn text(&self) -> Option<&str> {
        match self {
            Message::User(text)
            | Message::Assistant(text)
            | Message::Error(text)
            | Message::System(text) => Some(text),
            Message::ToolCall(_) | Message::ToolResult(_) => None,
        }
    }
}
