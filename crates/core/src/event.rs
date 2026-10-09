//! The event stream — how a turn in progress talks to the UI.

/// One heartbeat of an agent turn.
///
/// A provider never returns a finished reply in one piece; it *streams*
/// `Event`s instead. `TurnStart` opens the turn, `Token`s arrive in
/// order, `TurnEnd` closes it. Like `Message`, this is an enum so the
/// payload lives in the variant — and so a future variant (say,
/// `ToolCall`) forces every consumer to handle it at compile time.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// A turn is starting. Consumers show a spinner or a live area.
    TurnStart,
    /// One fragment of the assistant's reply, in order. Concatenating
    /// every `Token` of a turn reproduces the full reply exactly.
    Token(String),
    /// The turn finished. Whatever accumulated is the final reply.
    TurnEnd,
    /// The turn failed (network, HTTP status, malformed response…).
    /// Any `Token`s sent before it are the partial output. A
    /// `TurnEnd` still follows, so consumers keep a single
    /// finalization path and just mark the reply as failed.
    Error(String),
}
