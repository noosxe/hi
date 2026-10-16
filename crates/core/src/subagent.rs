//! The subagent runner (#4): nested agent sessions with their own
//! history, model, thinking level, and (restricted) toolset, returning
//! a shaped result to the caller. A scoping function over
//! [`spawn_agent_turn`] — the same machinery as the main loop, never a
//! second implementation.
//!
//! Rulings (AD-0040): history is built, never taken — the parent's
//! transcript never leaks in; the toolset excludes `subagent` itself,
//! so recursion is impossible by construction; the slot stamps
//! provider *identity* (names, not Arcs) so every run builds its own
//! provider wired to its own registry; parallelism is not provided —
//! one tool call runs one subagent to completion, and background
//! parallelism stays task_run territory (#5).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::ToolGate;
use crate::agent::spawn_agent_turn;
use crate::config::ModelsConfig;
use crate::event::{Event, Usage};
use crate::message::Message;
use crate::openai::OpenAiCompatProvider;
use crate::selection;
use crate::system_prompt;
use crate::thinking::ThinkingLevel;
use crate::tools::{ImageGate, ToolBundle};

/// The default round cap for a subagent: half the main loop's belt.
/// A safety belt, not a feature — the caller can lower or raise it
/// per run, and hitting the cap is a loud error, not a silent stop.
pub const SUBAGENT_MAX_TOOL_ROUNDS: usize = 8;

/// Soft budget marker (#119): when a run's cumulative input crosses
/// this, the result line says so. Input tokens re-send every round, so
/// long explorations grow quadratically — this makes the outlier
/// visible at the moment the parent decides what to do next. A hard
/// budget (abort mid-run) is deliberately out of scope.
pub const SUBAGENT_HIGH_TOKEN_USE: u64 = 100_000;

/// One nested run, requested by the caller. `provider` / `model` /
/// `thinking` name what the subagent should run (`None` = inherit the
/// stamped identity); `tools` picks its bundle (`None` = `all`,
/// minus `subagent`); `max_tool_rounds` caps the loop (`None` =
/// [`SUBAGENT_MAX_TOOL_ROUNDS`]).
#[derive(Debug, Clone)]
pub struct SubagentSpec {
    pub prompt: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub thinking: Option<ThinkingLevel>,
    pub tools: Option<ToolBundle>,
    pub max_tool_rounds: Option<usize>,
}

/// What came back: the subagent's final reply text, its own token
/// bill (detail halves not aggregated in v1), and how many tool
/// rounds it spent.
#[derive(Debug, Clone, PartialEq)]
pub struct SubagentResult {
    pub text: String,
    pub usage: Usage,
    pub rounds: usize,
}

/// The handle the `subagent` tool holds: config to resolve overrides
/// against, the session cwd its tools share, and the *current*
/// provider identity, restamped by the front end on every provider
/// rebuild (`/model`, `/resume` — the telemetry-stamp pattern).
/// Cloneable; every clone sees the same stamped identity.
#[derive(Clone)]
pub struct SubagentRunner {
    config: Arc<ModelsConfig>,
    cwd: Arc<PathBuf>,
    telemetry: bool,
    /// Consent gate for the subagent's own tool loop. `None` today —
    /// permission UX arrives with the extension host (app.rs's own
    /// gate is `None` for the same reason).
    gate: Option<Arc<ToolGate>>,
    current: Arc<Mutex<Option<(String, String)>>>,
}

impl SubagentRunner {
    pub fn new(config: ModelsConfig, cwd: PathBuf, telemetry: bool) -> Self {
        Self {
            config: Arc::new(config),
            cwd: Arc::new(cwd),
            telemetry,
            gate: None,
            current: Arc::new(Mutex::new(None)),
        }
    }

    /// Record the current provider identity. Called at startup and on
    /// every provider rebuild, so inherits follow `/model` switches.
    pub fn stamp(&self, provider: impl Into<String>, model: impl Into<String>) {
        *self.current.lock().unwrap() = Some((provider.into(), model.into()));
    }

    /// The stamped identity, for introspection and tests.
    pub fn current(&self) -> Option<(String, String)> {
        self.current.lock().unwrap().clone()
    }

    /// Run one subagent to completion. Blocking by ruling: the tool
    /// loop is sequential, and the shaped [`SubagentResult`] is all
    /// the caller ever sees — intermediate events are drained, not
    /// surfaced. An `Event::Error` (network, HTTP, the round cap)
    /// becomes `Err`; the events contract (TurnStart…TurnEnd,
    /// dropped-`Receiver` cancellation) is untouched — a cancelled
    /// parent abandons the in-flight tool thread exactly as it
    /// abandons a running `bash`, and the abandoned subagent
    /// self-stops at its own next send.
    pub fn run(&self, spec: &SubagentSpec) -> Result<SubagentResult, String> {
        // Identity: inherit the stamp, or resolve the override. The
        // parent's provider Arc is never reused — the subagent gets a
        // provider wired to *its* registry, not the parent's toolset.
        let (provider_name, model) = match (&spec.provider, &spec.model) {
            (None, None) => self.current().ok_or_else(|| {
                "no provider available for the subagent: the runner was never stamped".to_string()
            })?,
            _ => {
                let selected = selection::resolve(
                    &self.config,
                    spec.provider.as_deref(),
                    spec.model.as_deref(),
                    None,
                )?;
                (selected.provider, selected.model)
            }
        };
        let entry = self
            .config
            .providers
            .get(&provider_name)
            .ok_or_else(|| format!("provider `{provider_name}` is not in the config"))?;
        // R3 by construction: `subagent: None` means the nested
        // registry has no `subagent` tool — there is no path to
        // recursion, so no depth counter exists. `tasks: None` keeps
        // background-task ownership from outliving the runner's scope.
        let bundle = spec.tools.unwrap_or(ToolBundle::All);
        let registry = Arc::new(bundle.registry(&self.cwd, ImageGate::new(false), None, None));
        let mut system = system_prompt::resolve();
        let notes = registry.prompt_notes();
        if !notes.is_empty() {
            system.push_str("\n\n# Tool notes\n");
            system.push_str(notes.trim_end());
        }
        let provider = Arc::new(
            OpenAiCompatProvider::from_config(entry, model)
                .with_system_prompt(Some(system))
                .with_thinking_level(spec.thinking)
                .with_tools(Arc::clone(&registry))
                .with_telemetry(self.telemetry),
        );
        let history = vec![Message::User(spec.prompt.clone())];
        let max_rounds = spec.max_tool_rounds.unwrap_or(SUBAGENT_MAX_TOOL_ROUNDS);
        let rx = spawn_agent_turn(
            provider,
            Arc::clone(&registry),
            self.gate.clone(),
            &history,
            max_rounds,
        );
        let mut text = String::new();
        let mut narration = String::new();
        let mut usage = Usage::default();
        let mut rounds = 0usize;
        let mut failure = None;
        for event in rx {
            match event {
                Event::Token(t) => text.push_str(&t),
                Event::ToolCall(_) => {
                    rounds += 1;
                    // The reply that matters is the one after the last
                    // tool round; keep the pre-tool prose as the run's
                    // narration so a cap failure can return it.
                    if !text.trim().is_empty() {
                        narration = text.clone();
                    }
                    text.clear();
                }
                Event::Usage(u) => {
                    usage.prompt_tokens += u.prompt_tokens;
                    usage.completion_tokens += u.completion_tokens;
                }
                Event::Error(e) => {
                    failure = Some(e);
                    break; // dropping the receiver stops the loop at its next send
                }
                _ => {}
            }
        }
        if let Some(e) = failure {
            if e.contains(crate::agent::ROUND_CAP_MARKER) {
                // Model-actionable, not opaque: name the cap, name the
                // remedy, and hand back whatever the subagent had said.
                let mut msg = format!(
                    "subagent hit its tool-round cap: {e}. Retry with a \
                     higher max_rounds, or split the task into smaller \
                     subagent runs."
                );
                let partial = if !text.trim().is_empty() {
                    text.trim()
                } else {
                    narration.trim()
                };
                if partial.is_empty() {
                    msg.push_str(" (no partial output)");
                } else {
                    msg.push_str("\n\nPartial output before the cap:\n");
                    msg.push_str(partial);
                }
                return Err(msg);
            }
            return Err(e);
        }
        Ok(SubagentResult {
            text,
            usage,
            rounds,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{FakeApi, sse_response};

    fn runner_with(base_url: &str, cwd: &std::path::Path) -> SubagentRunner {
        let raw = format!(r#"{{"providers":{{"prov":{{"baseUrl":"{base_url}"}}}}}}"#);
        let config: ModelsConfig = serde_json::from_str(&raw).unwrap();
        SubagentRunner::new(config, cwd.to_path_buf(), false)
    }

    /// A settled one-frame SSE reply carrying `text` as the content.
    fn reply(text: &str) -> String {
        sse_response(&[
            &format!("{{\"choices\":[{{\"delta\":{{\"content\":\"{text}\"}}}}]}}"),
            "[DONE]",
        ])
    }

    #[test]
    fn run_inherits_the_stamped_identity() {
        let dir = std::env::temp_dir().join(format!("hi-sub-{}", std::process::id()));
        let api = FakeApi::start(reply("inherited reply"));
        let runner = runner_with(&api.base_url, &dir);
        runner.stamp("prov", "m");
        let result = runner
            .run(&SubagentSpec {
                prompt: "inner prompt".into(),
                provider: None,
                model: None,
                thinking: None,
                tools: None,
                max_tool_rounds: None,
            })
            .unwrap();
        assert_eq!(result.text, "inherited reply");
        assert_eq!(result.rounds, 0);
        // One round, no follow-up request.
        assert!(api.request(1).is_none());
    }

    #[test]
    fn run_requires_a_stamped_identity() {
        let dir = std::env::temp_dir().join(format!("hi-sub-unstamped-{}", std::process::id()));
        let runner = runner_with("http://unused.invalid/v1", &dir);
        let err = runner
            .run(&SubagentSpec {
                prompt: "x".into(),
                provider: None,
                model: None,
                thinking: None,
                tools: None,
                max_tool_rounds: None,
            })
            .unwrap_err();
        assert!(err.contains("provider"), "{err}");
    }

    #[test]
    fn run_resolves_an_override_identity_from_config() {
        let dir = std::env::temp_dir().join(format!("hi-sub-ovr-{}", std::process::id()));
        let inherited = FakeApi::start(reply("inherited must not win"));
        let override_api = FakeApi::start(reply("override reply"));
        let raw = format!(
            r#"{{"providers":{{"main":{{"baseUrl":"{}"}},"prov":{{"baseUrl":"{}"}}}}}}"#,
            inherited.base_url, override_api.base_url
        );
        let config: ModelsConfig = serde_json::from_str(&raw).unwrap();
        let runner = SubagentRunner::new(config, dir, false);
        runner.stamp("main", "m");
        let result = runner
            .run(&SubagentSpec {
                prompt: "go".into(),
                provider: Some("prov".into()),
                model: Some("m".into()),
                thinking: None,
                tools: None,
                max_tool_rounds: None,
            })
            .unwrap();
        assert_eq!(result.text, "override reply");
    }

    #[test]
    fn subagents_cannot_spawn_subagents() {
        let dir = std::env::temp_dir().join(format!("hi-sub-recur-{}", std::process::id()));
        let tool_call = sse_response(&[
            "{\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"subagent\",\"arguments\":\"{\\\"prompt\\\":\\\"nested\\\"}\"}}]}}]}",
            "[DONE]",
        ]);
        let settle = sse_response(&[
            "{\"choices\":[{\"delta\":{\"content\":\"settled\"}}]}",
            "[DONE]",
        ]);
        let api = FakeApi::start_sequence(vec![tool_call, settle]);
        let runner = runner_with(&api.base_url, &dir);
        runner.stamp("prov", "m");
        let result = runner
            .run(&SubagentSpec {
                prompt: "go".into(),
                provider: None,
                model: None,
                thinking: None,
                tools: None,
                max_tool_rounds: None,
            })
            .unwrap();
        // The nested call was refused as unknown; the run still settled.
        assert_eq!(result.text, "settled");
        assert_eq!(result.rounds, 1);
    }

    #[test]
    fn rounds_cap_is_a_loud_error() {
        let dir = std::env::temp_dir().join(format!("hi-sub-cap-{}", std::process::id()));
        let looping = sse_response(&[
            "{\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_x\",\"function\":{\"name\":\"nope\",\"arguments\":\"{}\"}}]}}]}",
            "[DONE]",
        ]);
        let api = FakeApi::start_sequence(vec![looping; 4]);
        let runner = runner_with(&api.base_url, &dir);
        runner.stamp("prov", "m");
        let err = runner
            .run(&SubagentSpec {
                prompt: "go".into(),
                provider: None,
                model: None,
                thinking: None,
                tools: None,
                max_tool_rounds: Some(2),
            })
            .unwrap_err();
        assert!(err.contains("round"), "{err}");
    }

    #[test]
    fn cap_error_names_the_remedy_and_keeps_narration() {
        let dir = std::env::temp_dir().join(format!("hi-sub-cap2-{}", std::process::id()));
        let narrate = sse_response(&[
            "{\"choices\":[{\"delta\":{\"content\":\"checking the manifest\"}}]}",
            "{\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"nope\",\"arguments\":\"{}\"}}]}}]}",
            "[DONE]",
        ]);
        let again = sse_response(&[
            "{\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_2\",\"function\":{\"name\":\"nope\",\"arguments\":\"{}\"}}]}}]}",
            "[DONE]",
        ]);
        let api = FakeApi::start_sequence(vec![narrate, again]);
        let runner = runner_with(&api.base_url, &dir);
        runner.stamp("prov", "m");
        let err = runner
            .run(&SubagentSpec {
                prompt: "go".into(),
                provider: None,
                model: None,
                thinking: None,
                tools: None,
                max_tool_rounds: Some(2),
            })
            .unwrap_err();
        // The original cap fact survives, the remedy is named, and the
        // subagent's last narration is not discarded.
        assert!(err.contains("did not settle within 2 rounds"), "{err}");
        assert!(err.contains("Retry with a higher max_rounds"), "{err}");
        assert!(err.contains("checking the manifest"), "{err}");
    }

    #[test]
    fn cap_error_without_narration_still_names_the_remedy() {
        let dir = std::env::temp_dir().join(format!("hi-sub-cap3-{}", std::process::id()));
        let looping = sse_response(&[
            "{\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_x\",\"function\":{\"name\":\"nope\",\"arguments\":\"{}\"}}]}}]}",
            "[DONE]",
        ]);
        let api = FakeApi::start_sequence(vec![looping; 2]);
        let runner = runner_with(&api.base_url, &dir);
        runner.stamp("prov", "m");
        let err = runner
            .run(&SubagentSpec {
                prompt: "go".into(),
                provider: None,
                model: None,
                thinking: None,
                tools: None,
                max_tool_rounds: Some(2),
            })
            .unwrap_err();
        assert!(err.contains("Retry with a higher max_rounds"), "{err}");
        assert!(err.contains("no partial"), "{err}");
    }

    #[test]
    fn caller_history_never_leaks() {
        let dir = std::env::temp_dir().join(format!("hi-sub-leak-{}", std::process::id()));
        let api = FakeApi::start(reply("ok"));
        let runner = runner_with(&api.base_url, &dir);
        runner.stamp("prov", "m");
        runner
            .run(&SubagentSpec {
                prompt: "the inner task".into(),
                provider: None,
                model: None,
                thinking: None,
                tools: None,
                max_tool_rounds: None,
            })
            .unwrap();
        let request = api.first_request();
        assert!(request.contains("the inner task"), "{request}");
    }

    #[test]
    fn multi_round_bill_accumulates() {
        let dir = std::env::temp_dir().join(format!("hi-sub-bill-{}", std::process::id()));
        let tool_call = sse_response(&[
            "{\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"nope\",\"arguments\":\"{}\"}}]}}]}",
            "{\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":4}}",
            "[DONE]",
        ]);
        let settle = sse_response(&[
            "{\"choices\":[{\"delta\":{\"content\":\"done\"}}]}",
            "{\"choices\":[],\"usage\":{\"prompt_tokens\":30,\"completion_tokens\":6}}",
            "[DONE]",
        ]);
        let api = FakeApi::start_sequence(vec![tool_call, settle]);
        let runner = runner_with(&api.base_url, &dir);
        runner.stamp("prov", "m");
        let result = runner
            .run(&SubagentSpec {
                prompt: "go".into(),
                provider: None,
                model: None,
                thinking: None,
                tools: None,
                max_tool_rounds: None,
            })
            .unwrap();
        assert_eq!(result.rounds, 1);
        assert_eq!(result.usage.prompt_tokens, 40);
        assert_eq!(result.usage.completion_tokens, 10);
        assert_eq!(result.text, "done");
    }

    #[test]
    fn event_error_becomes_err() {
        let dir = std::env::temp_dir().join(format!("hi-sub-err-{}", std::process::id()));
        let api = FakeApi::start("this is not sse at all");
        let runner = runner_with(&api.base_url, &dir);
        runner.stamp("prov", "m");
        let err = runner
            .run(&SubagentSpec {
                prompt: "go".into(),
                provider: None,
                model: None,
                thinking: None,
                tools: None,
                max_tool_rounds: None,
            })
            .unwrap_err();
        assert!(!err.is_empty());
    }
}
