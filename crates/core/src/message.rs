//! The conversation message type.

use crate::tool::{ToolCall, ToolResult};

/// One piece of an assistant message (AD-0048): plain prose or the
/// model's reasoning. Blocks keep stream order, so thinking that
/// interleaves with text stays in position — and the wire replay can
/// put both back the way the model produced them.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AssistantBlock {
    /// Reply prose.
    Text(String),
    /// The model's reasoning. `signature` records, opaquely, the wire
    /// field the reasoning arrived on (`reasoning_content`, `reasoning`,
    /// `reasoning_text`) — replay writes it back to the same field.
    /// `redacted` marks a provider's encrypted-only block: the payload
    /// rides in `signature`, the text is a placeholder.
    Thinking {
        thinking: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
        #[serde(default)]
        redacted: bool,
    },
}

/// The wire field replay uses when a thinking block carries no record
/// — the convention for every server streaming reasoning on the
/// chat-completions wire today.
pub const DEFAULT_REASONING_FIELD: &str = "reasoning_content";

/// Every wire field reasoning is known to stream under, in probe
/// order (pi's): ingest tries them front to back, replay writes back
/// to the recorded one.
pub const REASONING_FIELDS: [&str; 3] = ["reasoning_content", "reasoning", "reasoning_text"];

impl AssistantBlock {
    /// Append a text delta: a trailing `Text` block extends, anything
    /// else appends. Keeps interleaved stream order without forking a
    /// block per fragment.
    pub fn push_text(blocks: &mut Vec<AssistantBlock>, delta: &str) {
        match blocks.last_mut() {
            Some(AssistantBlock::Text(text)) => text.push_str(delta),
            _ => blocks.push(AssistantBlock::Text(delta.to_string())),
        }
    }

    /// Append a reasoning delta, same extend-or-append rule, stamping
    /// the wire field on the block's first fragment.
    pub fn push_thinking(blocks: &mut Vec<AssistantBlock>, delta: &str, field: &'static str) {
        match blocks.last_mut() {
            Some(AssistantBlock::Thinking {
                thinking,
                signature,
                ..
            }) => {
                thinking.push_str(delta);
                if signature.is_none() {
                    *signature = Some(field.to_string());
                }
            }
            _ => blocks.push(AssistantBlock::Thinking {
                thinking: delta.to_string(),
                signature: Some(field.to_string()),
                redacted: false,
            }),
        }
    }

    /// The prose content of a block run: `Text` blocks joined in
    /// order. Reasoning is not prose — it never appears here.
    pub fn text_of(blocks: &[AssistantBlock]) -> String {
        blocks
            .iter()
            .filter_map(|block| match block {
                AssistantBlock::Text(text) => Some(text.as_str()),
                AssistantBlock::Thinking { .. } => None,
            })
            .collect()
    }

    /// The replay field for a block run: the first thinking block's
    /// recorded wire field, defaulting to [`DEFAULT_REASONING_FIELD`].
    pub fn replay_field(blocks: &[AssistantBlock]) -> &str {
        blocks
            .iter()
            .filter_map(|block| match block {
                AssistantBlock::Thinking { signature, .. } => signature.as_deref(),
                AssistantBlock::Text(_) => None,
            })
            .next()
            .unwrap_or(DEFAULT_REASONING_FIELD)
    }
}

/// An enum rather than a struct-with-a-role-field because the payload
/// lives in the variant: the compiler guarantees you can never read
/// one speaker's data out of another's variant, and `match` over the
/// variants is exhaustive — the tool-call kinds leaned on exactly that:
/// every consumer must handle them at compile time.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(rename_all = "camelCase", from = "MessagePayload")]
pub enum Message {
    /// Something the human typed.
    User(String),
    /// Something hi itself spoke to the model as a user turn — no
    /// human wrote it (AD-0053): the task-completion mandate, the
    /// subagent's brief. On the wire it IS a user turn (same role,
    /// same text — the prefix inside the text is how the model
    /// knows); the session record carries the record-level
    /// `origin: "harness"` marker so old binaries read the line
    /// unchanged, and the front ends treat it as not-yours (no
    /// prompt history, dim render).
    Harness(String),
    /// Something the model produced, as a block list (AD-0048): prose
    /// and reasoning in stream order.
    Assistant(Vec<AssistantBlock>),
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

/// The serialization mirror of [`Message`]. `Harness` shares the
/// `user` payload on purpose: on the wire and in session payloads a
/// harness turn *is* a user turn, and a distinct payload key would
/// break old binaries on an unknown externally-tagged variant. The
/// harness-ness rides the session record's `origin` marker instead
/// (`session::message_line`).
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
enum MessagePayloadOut<'a> {
    User(&'a str),
    Assistant(&'a [AssistantBlock]),
    ToolCall(&'a ToolCall),
    ToolResult(&'a ToolResult),
    Error(&'a str),
    System(&'a str),
}

impl serde::Serialize for Message {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let payload = match self {
            Message::User(text) | Message::Harness(text) => MessagePayloadOut::User(text),
            Message::Assistant(blocks) => MessagePayloadOut::Assistant(blocks),
            Message::ToolCall(call) => MessagePayloadOut::ToolCall(call),
            Message::ToolResult(result) => MessagePayloadOut::ToolResult(result),
            Message::Error(text) => MessagePayloadOut::Error(text),
            Message::System(text) => MessagePayloadOut::System(text),
        };
        payload.serialize(serializer)
    }
}

/// Deserialization shim: pre-thinking session files carry the
/// assistant payload as a plain string, current files as a block
/// array. Untagged tries the array first, then upgrades the legacy
/// string to a single `Text` block.
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum AssistantContent {
    Blocks(Vec<AssistantBlock>),
    Legacy(String),
}

/// The externally tagged mirror of [`Message`], for `from` — the
/// wire form is unchanged (`{"Assistant": …}`), only the assistant
/// payload gained a second shape.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
enum MessagePayload {
    User(String),
    Assistant(AssistantContent),
    ToolCall(ToolCall),
    ToolResult(ToolResult),
    Error(String),
    System(String),
}

impl From<MessagePayload> for Message {
    fn from(payload: MessagePayload) -> Self {
        match payload {
            MessagePayload::User(text) => Message::User(text),
            MessagePayload::Assistant(AssistantContent::Blocks(blocks)) => {
                Message::Assistant(blocks)
            }
            MessagePayload::Assistant(AssistantContent::Legacy(text)) => {
                Message::Assistant(vec![AssistantBlock::Text(text)])
            }
            MessagePayload::ToolCall(call) => Message::ToolCall(call),
            MessagePayload::ToolResult(result) => Message::ToolResult(result),
            MessagePayload::Error(text) => Message::Error(text),
            MessagePayload::System(text) => Message::System(text),
        }
    }
}

impl Message {
    /// A plain-prose assistant message — the shape every
    /// pre-thinking call site and test wants.
    pub fn assistant(text: impl Into<String>) -> Message {
        Message::Assistant(vec![AssistantBlock::Text(text.into())])
    }

    /// The prose content, if this variant carries any: `Text` blocks
    /// joined in order. A thinking-only assistant message carries no
    /// prose, so it is `None`. `ToolCall` and `ToolResult` carry
    /// structured payloads instead of prose — also `None`; render
    /// those from their fields.
    pub fn text(&self) -> Option<String> {
        match self {
            Message::User(text)
            | Message::Harness(text)
            | Message::Error(text)
            | Message::System(text) => Some(text.clone()),
            Message::Assistant(blocks) => {
                let has_prose = blocks
                    .iter()
                    .any(|block| matches!(block, AssistantBlock::Text(_)));
                has_prose.then(|| AssistantBlock::text_of(blocks))
            }
            Message::ToolCall(_) | Message::ToolResult(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harness_serializes_with_the_user_payload() {
        // The session payload cannot distinguish harness turns (an
        // unknown externally-tagged variant would break old binaries);
        // the marker lives at the record level instead (AD-0053).
        let payload = serde_json::to_value(Message::Harness("go".into())).unwrap();
        assert_eq!(payload, serde_json::json!({ "user": "go" }));
        assert_eq!(
            serde_json::to_value(Message::User("go".into())).unwrap(),
            payload
        );
    }

    #[test]
    fn legacy_string_assistant_lines_upgrade_to_a_text_block() {
        // The exact bytes a pre-thinking session file carries.
        let line = r#"{"assistant":"checking"}"#;
        let message: Message = serde_json::from_str(line).unwrap();
        assert_eq!(message, Message::assistant("checking"));
    }

    #[test]
    fn block_assistant_messages_round_trip_and_thinking_keeps_its_field() {
        let message = Message::Assistant(vec![
            AssistantBlock::Thinking {
                thinking: "hmm".into(),
                signature: Some("reasoning_content".into()),
                redacted: false,
            },
            AssistantBlock::Text("done".into()),
        ]);
        let line = serde_json::to_string(&message).unwrap();
        let back: Message = serde_json::from_str(&line).unwrap();
        assert_eq!(back, message);
    }

    #[test]
    fn text_joins_prose_and_skips_thinking() {
        let message = Message::Assistant(vec![
            AssistantBlock::Text("a".into()),
            AssistantBlock::Thinking {
                thinking: "h".into(),
                signature: None,
                redacted: false,
            },
            AssistantBlock::Text("b".into()),
        ]);
        assert_eq!(Message::text(&message).as_deref(), Some("ab"));
        let thinking_only = Message::Assistant(vec![AssistantBlock::Thinking {
            thinking: "h".into(),
            signature: None,
            redacted: false,
        }]);
        assert_eq!(Message::text(&thinking_only), None);
    }

    #[test]
    fn push_extends_the_trailing_block_and_stamps_the_field_once() {
        let mut blocks = Vec::new();
        AssistantBlock::push_thinking(&mut blocks, "h", "reasoning");
        AssistantBlock::push_thinking(&mut blocks, "m", "reasoning");
        AssistantBlock::push_text(&mut blocks, "he");
        AssistantBlock::push_thinking(&mut blocks, "!", "reasoning");
        AssistantBlock::push_text(&mut blocks, "llo");
        // Interleaving is the point: each flip of kind starts a new
        // block, in stream order.
        assert_eq!(
            blocks,
            vec![
                AssistantBlock::Thinking {
                    thinking: "hm".into(),
                    signature: Some("reasoning".into()),
                    redacted: false,
                },
                AssistantBlock::Text("he".into()),
                AssistantBlock::Thinking {
                    thinking: "!".into(),
                    signature: Some("reasoning".into()),
                    redacted: false,
                },
                AssistantBlock::Text("llo".into()),
            ]
        );
        assert_eq!(AssistantBlock::replay_field(&blocks), "reasoning");
    }
}
