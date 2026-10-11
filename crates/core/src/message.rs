//! The conversation message type.

/// One entry in the conversation transcript.
///
/// An enum rather than a struct-with-a-role-field because the payload
/// lives in the variant: the compiler guarantees you can never read
/// one speaker's data out of another's variant, and `match` over the
/// variants is exhaustive — adding a kind (say, `ToolUse`) will force
/// every consumer to handle it at compile time.
#[derive(Debug, Clone)]
pub enum Message {
    /// Something the human typed.
    User(String),
    /// Something the model produced.
    Assistant(String),
    /// A turn-level failure, shown in the transcript.
    Error(String),
    /// A harness announcement (e.g. "default model saved: …"), shown
    /// dim in the transcript.
    System(String),
}

impl Message {
    /// The text content, whoever said it.
    pub fn text(&self) -> &str {
        match self {
            Message::User(text)
            | Message::Assistant(text)
            | Message::Error(text)
            | Message::System(text) => text,
        }
    }
}
