//! The context report (AD-0047): the outgoing wire request bucketed
//! by class, estimated in tokens, anchored to the provider's bill.

#[cfg(test)]
mod tests {
    use super::{ContextClass, ContextInput, context_report};
    use crate::message::Message;
    use crate::tool::ImageBlock;
    use crate::tool::{Tool, ToolCall, ToolRegistry, ToolResult};
    use std::sync::Arc;

    struct Probe;
    impl Tool for Probe {
        fn name(&self) -> &str {
            "probe"
        }
        fn description(&self) -> &str {
            "a probe"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {"x": {"type": "string"}}})
        }
        fn execute(&self, _arguments: &str) -> Result<String, String> {
            Ok("ok".into())
        }
    }

    fn sample_history() -> Vec<Message> {
        vec![
            Message::User("hello there".into()),
            Message::Assistant("hi".into()),
            Message::ToolCall(ToolCall {
                id: "call_1".into(),
                name: "probe".into(),
                arguments: "{\"x\":\"y\"}".into(),
            }),
            Message::ToolResult(ToolResult {
                call_id: "call_1".into(),
                name: "probe".into(),
                content: "the result text".into(),
                is_error: false,
                images: vec![ImageBlock {
                    data: "aGVsbG8=".into(),
                    mime_type: "image/png".into(),
                }],
            }),
        ]
    }

    fn sample_registry() -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(Probe)).unwrap();
        registry
    }

    fn sample_input<'a>(history: &'a [Message], tools: &'a ToolRegistry) -> ContextInput<'a> {
        ContextInput {
            system_prompt: Some("You are terse."),
            tools,
            history,
        }
    }

    #[test]
    fn the_report_buckets_the_wire_request_by_class() {
        let history = sample_history();
        let registry = sample_registry();
        let report = context_report(sample_input(&history, &registry), None);
        let tokens = |class: ContextClass| {
            report
                .classes
                .iter()
                .find(|(c, _)| *c == class)
                .map(|(_, t)| *t)
                .unwrap_or(0)
        };
        for class in [
            ContextClass::SystemPrompt,
            ContextClass::ToolSchemas,
            ContextClass::User,
            ContextClass::Assistant,
            ContextClass::ToolCalls,
            ContextClass::ToolResults,
            ContextClass::Images,
        ] {
            assert!(tokens(class) > 0, "{class:?} must be counted");
        }
        let sum: u64 = report.classes.iter().map(|(_, t)| t).sum();
        assert_eq!(report.raw_total, sum);
    }

    #[test]
    fn the_report_anchors_to_the_providers_bill() {
        let history = sample_history();
        let registry = sample_registry();
        let unscaled = context_report(sample_input(&history, &registry), None);
        let anchor = unscaled.raw_total * 2;
        let report = context_report(sample_input(&history, &registry), Some(anchor));
        let sum: u64 = report.anchored().iter().map(|(_, t)| t).sum();
        assert_eq!(sum, anchor, "anchored classes must sum to the bill exactly");
    }

    #[test]
    fn transcript_notes_never_reach_the_report() {
        let registry = sample_registry();
        let mut history = sample_history();
        let without = context_report(sample_input(&history, &registry), None).raw_total;
        history.push(Message::Error("boom".into()));
        history.push(Message::System("a note".into()));
        let with = context_report(sample_input(&history, &registry), None).raw_total;
        assert_eq!(with, without, "Error/System never reach the wire");
    }
}

use crate::message::Message;
use crate::openai::{WireMessage, wire_messages};
use crate::tool::ToolRegistry;

/// One bucket of the outgoing request (AD-0047 ruling 1). The
/// transcript's `Error`/`System` notes are absent by construction:
/// `wire_messages` never sends them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextClass {
    SystemPrompt,
    ToolSchemas,
    User,
    Assistant,
    ToolCalls,
    ToolResults,
    Images,
}

impl ContextClass {
    /// The row label in the `/context` panel.
    pub fn label(self) -> &'static str {
        match self {
            ContextClass::SystemPrompt => "system prompt",
            ContextClass::ToolSchemas => "tool schemas",
            ContextClass::User => "user",
            ContextClass::Assistant => "assistant",
            ContextClass::ToolCalls => "tool calls",
            ContextClass::ToolResults => "tool results",
            ContextClass::Images => "images",
        }
    }
}

/// Everything the report needs, in the shape the provider already
/// holds it — the same parts `stream` assembles per turn.
pub struct ContextInput<'a> {
    pub system_prompt: Option<&'a str>,
    pub tools: &'a ToolRegistry,
    pub history: &'a [Message],
}

/// The estimate for one snapshot of the request. `raw_total` is the
/// bytes-over-four sum; `prompt_tokens` is the provider's last real
/// bill when known (AD-0047 ruling 3 — the anchor).
pub struct ContextReport {
    /// Non-zero classes only, wire order.
    pub classes: Vec<(ContextClass, u64)>,
    pub raw_total: u64,
    pub prompt_tokens: Option<u64>,
}

impl ContextReport {
    /// The per-class numbers to display: scaled by largest remainder
    /// so they sum to the provider's exact bill when one exists
    /// (ruling 3), the raw estimate otherwise.
    pub fn anchored(&self) -> Vec<(ContextClass, u64)> {
        let Some(anchor) = self.prompt_tokens else {
            return self.classes.clone();
        };
        if self.raw_total == 0 {
            return Vec::new();
        }
        let exact: Vec<(ContextClass, f64)> = self
            .classes
            .iter()
            .map(|(c, t)| (*c, *t as f64 * anchor as f64 / self.raw_total as f64))
            .collect();
        let mut floors: Vec<(ContextClass, u64, f64)> = exact
            .iter()
            .map(|(c, v)| (*c, v.floor() as u64, v - v.floor()))
            .collect();
        let mut left = anchor - floors.iter().map(|(_, f, _)| f).sum::<u64>();
        // Hand the leftover units to the largest fractional parts.
        floors.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
        for row in floors.iter_mut() {
            if left == 0 {
                break;
            }
            row.1 += 1;
            left -= 1;
        }
        floors.into_iter().map(|(c, t, _)| (c, t)).collect()
    }
}

/// Estimate the request's size per class (AD-0047 ruling 2): the
/// actual wire serialization, bucketed by structure, bytes over four.
/// `prompt_tokens` — the last round's bill — anchors the display.
pub fn context_report(input: ContextInput, prompt_tokens: Option<u64>) -> ContextReport {
    let mut buckets: Vec<(ContextClass, u64)> = Vec::new();
    let mut add = |class: ContextClass, bytes: usize| {
        if bytes == 0 {
            return;
        }
        let tokens = bytes.div_ceil(4) as u64;
        if let Some(row) = buckets.iter_mut().find(|(c, _)| *c == class) {
            row.1 += tokens;
        } else {
            buckets.push((class, tokens));
        }
    };

    if let Some(system) = input.system_prompt {
        let wire = WireMessage::text("system", system);
        add(
            ContextClass::SystemPrompt,
            serde_json::to_vec(&wire).map(|v| v.len()).unwrap_or(0),
        );
    }
    for definition in input.tools.definitions() {
        add(
            ContextClass::ToolSchemas,
            serde_json::to_vec(&definition)
                .map(|v| v.len())
                .unwrap_or(0),
        );
    }
    for wire in wire_messages(input.history) {
        let Ok(value) = serde_json::to_value(&wire) else {
            continue;
        };
        // An assistant message carrying calls splits: the `tool_calls`
        // array bills as ToolCalls, the rest (content, scaffolding) as
        // Assistant.
        if value.get("tool_calls").is_some_and(|c| c.is_array()) {
            if let Some(calls) = value.get("tool_calls") {
                add(
                    ContextClass::ToolCalls,
                    serde_json::to_vec(calls).map(|v| v.len()).unwrap_or(0),
                );
            }
            let mut rest = value.clone();
            rest.as_object_mut().unwrap().remove("tool_calls");
            add(
                ContextClass::Assistant,
                serde_json::to_vec(&rest).map(|v| v.len()).unwrap_or(0),
            );
            continue;
        }
        // An image carrier is content parts holding `image_url`s —
        // the whole message bills as Images.
        let has_images = value
            .get("content")
            .and_then(|c| c.as_array())
            .is_some_and(|parts| parts.iter().any(|p| p.get("image_url").is_some()));
        if has_images {
            add(
                ContextClass::Images,
                serde_json::to_vec(&value).map(|v| v.len()).unwrap_or(0),
            );
            continue;
        }
        let class = match value.get("role").and_then(|r| r.as_str()) {
            Some("user") => ContextClass::User,
            Some("tool") => ContextClass::ToolResults,
            _ => ContextClass::Assistant,
        };
        add(
            class,
            serde_json::to_vec(&value).map(|v| v.len()).unwrap_or(0),
        );
    }

    let raw_total = buckets.iter().map(|(_, t)| t).sum();
    ContextReport {
        classes: buckets,
        raw_total,
        prompt_tokens,
    }
}
