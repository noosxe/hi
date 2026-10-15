//! The `subagent` tool: one nested agent run per call (the #4 surface
//! over [`crate::subagent`]). The nested agent starts with a fresh
//! history — it sees none of this conversation — and its final reply
//! comes back as this tool's result.

use crate::subagent::{SubagentRunner, SubagentSpec};
use crate::thinking::ThinkingLevel;
use crate::tool::{Tool, ToolAnnotations};

use super::{args_object, opt_str, opt_usize};

pub struct SubagentTool {
    runner: SubagentRunner,
}

impl SubagentTool {
    pub fn new(runner: SubagentRunner) -> Self {
        Self { runner }
    }
}

impl Tool for SubagentTool {
    fn name(&self) -> &str {
        "subagent"
    }

    fn description(&self) -> &str {
        "Run a nested agent session with its own fresh history, provider/model, \
         thinking level, and toolset; it works autonomously and its final reply \
         comes back as this tool's result. The nested agent has no `subagent` \
         tool, so it cannot recurse. Use it for self-contained exploration or \
         bulk work that would flood this conversation — give it everything it \
         needs, because it sees none of this transcript."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "The full task for the nested agent — it starts with no other context"
                },
                "provider": {
                    "type": "string",
                    "description": "Provider name (default: this session's provider)"
                },
                "model": {
                    "type": "string",
                    "description": "Model id (default: this session's model)"
                },
                "thinking": {
                    "type": "string",
                    "description": "Reasoning effort: off | minimal | low | medium | high"
                },
                "tools": {
                    "type": "string",
                    "description": "Tool bundle for the nested agent: read-only | coding | all (default: all)"
                },
                "max_rounds": {
                    "type": "number",
                    "description": "Tool-round cap for the nested agent (default: 8)"
                }
            },
            "required": ["prompt"]
        })
    }

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: false,
            destructive: false,
        }
    }

    fn prompt_note(&self) -> Option<String> {
        Some(
            "subagent runs a nested agent for self-contained work; hand it the \
             full task and every fact it needs — it sees none of this \
             conversation."
                .into(),
        )
    }

    fn execute(&self, arguments: &str) -> Result<String, String> {
        let args = args_object(arguments)?;
        let Some(prompt) = opt_str(&args, "prompt")? else {
            return Err("subagent needs `prompt`".into());
        };
        let thinking = match opt_str(&args, "thinking")? {
            None => None,
            Some(raw) => Some(ThinkingLevel::parse(&raw).ok_or_else(|| {
                format!("thinking must be off|minimal|low|medium|high, got {raw}")
            })?),
        };
        let tools = match opt_str(&args, "tools")? {
            None => None,
            Some(raw) => Some(super::parse_bundle(&raw)?),
        };
        let spec = SubagentSpec {
            prompt,
            provider: opt_str(&args, "provider")?,
            model: opt_str(&args, "model")?,
            thinking,
            tools,
            max_tool_rounds: opt_usize(&args, "max_rounds")?,
        };
        let result = self.runner.run(&spec)?;
        Ok(format!(
            "{}\n\n(subagent: ↑{} ↓{} tokens, {} tool round(s))",
            result.text, result.usage.prompt_tokens, result.usage.completion_tokens, result.rounds
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ModelsConfig;
    use crate::test_support::{FakeApi, sse_response};

    fn reply(text: &str) -> String {
        sse_response(&[
            &format!("{{\"choices\":[{{\"delta\":{{\"content\":\"{text}\"}}}}]}}"),
            "[DONE]",
        ])
    }

    fn runner(base_url: &str) -> SubagentRunner {
        let raw = format!(r#"{{"providers":{{"prov":{{"baseUrl":"{base_url}"}}}}}}"#);
        let config: ModelsConfig = serde_json::from_str(&raw).unwrap();
        SubagentRunner::new(config, std::env::temp_dir(), false)
    }

    #[test]
    fn missing_prompt_is_an_error() {
        let tool = SubagentTool::new(runner("http://unused.invalid/v1"));
        let err = tool.execute("{}").unwrap_err();
        assert!(err.contains("prompt"), "{err}");
    }

    #[test]
    fn bad_thinking_name_is_an_error() {
        let tool = SubagentTool::new(runner("http://unused.invalid/v1"));
        let err = tool
            .execute(r#"{"prompt":"x","thinking":"maximum"}"#)
            .unwrap_err();
        assert!(err.contains("thinking"), "{err}");
    }

    #[test]
    fn full_schema_parses_and_runs() {
        let api = FakeApi::start(reply("nested ok"));
        let tool = SubagentTool::new(runner(&api.base_url));
        tool.runner.stamp("prov", "m");
        let out = tool
            .execute(r#"{"prompt":"do it","tools":"read-only","max_rounds":3}"#)
            .unwrap();
        assert!(out.starts_with("nested ok"), "{out}");
        assert!(out.contains("0 tool round(s)"), "{out}");
    }

    #[test]
    fn unknown_provider_names_what_was_available() {
        let tool = SubagentTool::new(runner("http://unused.invalid/v1"));
        let err = tool
            .execute(r#"{"prompt":"x","provider":"ghost"}"#)
            .unwrap_err();
        assert!(err.contains("ghost"), "{err}");
    }
}
