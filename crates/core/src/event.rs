//! The event stream — how a turn in progress talks to the UI.

use serde::{Deserialize, Serialize};

use crate::tool::{ToolCall, ToolResult};

/// One heartbeat of an agent turn.
///
/// A provider never returns a finished reply in one piece; it *streams*
/// `Event`s instead. `TurnStart` opens the turn, `Token`s arrive in
/// order, `TurnEnd` closes it. Like `Message`, this is an enum so the
/// payload lives in the variant — tool-call activity rides the same
/// stream, and no consumer can look away from it.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// A turn is starting. Consumers show a spinner or a live area.
    TurnStart,
    /// One fragment of the assistant's reply, in order. Concatenating
    /// every `Token` of a turn reproduces the full reply exactly.
    Token(String),
    /// The model asked to run a tool. Arrives after any `Token`s that
    /// preceded it in the same round of the agent loop; the answering
    /// `ToolResult` always follows before the turn can end.
    ToolCall(ToolCall),
    /// The harness executed (or refused) a tool call. Pairing is
    /// guaranteed: every `ToolCall` event is answered by exactly one
    /// `ToolResult` carrying the same `call_id`.
    ToolResult(ToolResult),
    /// The round's token economics, from the stream's final `usage`
    /// chunk (AD-0026). Arrives once per round, after any tool events
    /// and before `TurnEnd` — the bill for the round just closed.
    /// Providers that never report usage simply never send this.
    Usage(Usage),
    /// The turn finished. Whatever accumulated is the final reply.
    TurnEnd,
    /// The turn failed (network, HTTP status, malformed response…).
    /// Any `Token`s sent before it are the partial output. A
    /// `TurnEnd` still follows, so consumers keep a single
    /// finalization path and just mark the reply as failed.
    Error(String),
}

/// Token economics of one round, raw from the wire. Raw counts —
/// formatting is a front-end policy. The detail halves are `Option`
/// because providers differ: an absent detail means "not reported",
/// not zero, and every display hides what its input lacks.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    /// Everything re-sent to the model this round — the context.
    pub prompt_tokens: u64,
    /// What the model generated this round.
    pub completion_tokens: u64,
    /// The share of `prompt_tokens` served from the provider's cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u64>,
    /// Reasoning tokens the model spent, inside `completion_tokens`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
}

impl Usage {
    /// Cumulative merge across rounds: counts sum; a detail stays
    /// absent until some round reports it (`None` is not zero).
    pub fn merge(&mut self, other: Usage) {
        self.prompt_tokens += other.prompt_tokens;
        self.completion_tokens += other.completion_tokens;
        self.cached_tokens = sum_details(self.cached_tokens, other.cached_tokens);
        self.reasoning_tokens = sum_details(self.reasoning_tokens, other.reasoning_tokens);
    }
}

/// Sum two optional counts: two absents are absent, anything else adds.
fn sum_details(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_sums_counts_and_keeps_absent_details_absent() {
        let mut total = Usage {
            prompt_tokens: 100,
            completion_tokens: 20,
            cached_tokens: None,
            reasoning_tokens: Some(5),
        };
        total.merge(Usage {
            prompt_tokens: 50,
            completion_tokens: 10,
            cached_tokens: Some(30),
            reasoning_tokens: None,
        });
        assert_eq!(
            total,
            Usage {
                prompt_tokens: 150,
                completion_tokens: 30,
                // Reported once, remembered: absent is not zero.
                cached_tokens: Some(30),
                // Reported once, remembered the other way round too.
                reasoning_tokens: Some(5),
            }
        );
    }
}
