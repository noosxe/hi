//! One-shot, non-interactive turns: `hi --print`.
//!
//! Headless runs share everything interactive runs use — the same agent
//! loop, tool registry, provider selection, and session files — but skip
//! the interface entirely: a prompt goes in, the final assistant text
//! comes back, the process exits. This module owns the drain and never
//! touches the terminal; the CLI prints what [`run`] returns.

use std::sync::Arc;

use crate::agent::spawn_agent_turn;
use crate::event::{Event, Usage};
use crate::message::{AssistantBlock, Message};
use crate::provider::Provider;
use crate::session::{Session, SessionHeader};
use crate::tool::{ToolGate, ToolRegistry};
use serde::Serialize;

/// Assemble the headless prompt from piped stdin and the positional
/// argument. Stdin comes first (pi's contract: pipe contents are
/// prepended to the prompt), the argument follows, joined by one newline
/// when both are present. Empty halves count as absent; two absent
/// halves are a usage error.
pub fn combine_prompt(stdin: Option<String>, arg: Option<String>) -> Result<String, String> {
    let piped = stdin.filter(|text| !text.is_empty());
    let typed = arg.filter(|text| !text.is_empty());
    match (piped, typed) {
        (Some(piped), Some(typed)) => Ok(format!("{piped}\n{typed}")),
        (Some(piped), None) => Ok(piped),
        (None, Some(typed)) => Ok(typed),
        (None, None) => Err("no prompt: pass one as an argument or pipe it into stdin".into()),
    }
}

/// Run one headless turn: push `prompt` onto `history`, drain the agent
/// loop to completion, and return the final assistant text — the
/// text-mode entry point; nothing streams (see `run_with`).
///
/// Every finalized message — the user prompt, tool traffic, the reply,
/// and on failure the partial text plus a `Message::Error` — lands in
/// `history` and, when `session` is open, in the session file. An append
/// failure is tolerated (persistence is best-effort, same contract as
/// the TUI): the session is dropped, the turn still succeeds.
///
/// The error contract matches pi's print mode: `Err` means the CLI
/// prints the reason to stderr and exits non-zero with stdout left
/// empty; partial output survives only in the transcript and session.
pub fn run(
    provider: Arc<dyn Provider>,
    registry: Arc<ToolRegistry>,
    gate: Option<Arc<ToolGate>>,
    history: &mut Vec<Message>,
    prompt: &str,
    session: Option<Session>,
    max_tool_rounds: usize,
) -> Result<String, String> {
    run_with(
        provider,
        registry,
        gate,
        history,
        prompt,
        session,
        max_tool_rounds,
        &mut |_| {},
    )
}

/// One JSONL record of `--mode json` (#18, AD-0038): hi's own
/// vocabulary, one tag per thing the agent loop actually does. The
/// `Message` payload serializes exactly like a session-file line, so
/// a json run and its transcript agree byte for byte.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JsonRecord {
    /// The session header — the first record, only when a session
    /// file is open (`--no-session` starts at the user message).
    Session {
        #[serde(flatten)]
        header: SessionHeader,
    },
    /// A streaming fragment of the assistant's reply, in order.
    /// Concatenating every delta reproduces the full reply exactly.
    Token { delta: String },
    /// A streaming fragment of the assistant's reasoning (AD-0048);
    /// `field` is the wire field it arrived on.
    Thinking { delta: String, field: &'static str },
    /// A finalized transcript message — the user prompt, narration,
    /// tool traffic, partial output, the error line.
    Message { message: Message },
    /// The round's bill, once reported (AD-0026).
    Usage { usage: Usage },
    /// Success: the final assistant text. The last record.
    Done { text: String },
    /// Failure: the reason, after the partial output and the error
    /// line. The last record; the process exits non-zero.
    Failed { reason: String },
}

impl JsonRecord {
    /// The record as one JSONL line, no trailing newline.
    /// Serialization of these plain shapes cannot fail.
    pub fn to_line(&self) -> String {
        serde_json::to_string(self).expect("JsonRecord serialization cannot fail")
    }
}

/// Run one headless turn, emitting a [`JsonRecord`] for everything
/// that happens — `--mode json`'s engine, with `run` as the no-op
/// sink.
///
/// Record order: the session header (when a session is open), the
/// user message, then the turn's events in arrival order — token
/// deltas interleaved with the transcript messages as they finalize —
/// and exactly one terminal record: `Done` with the reply, or
/// `Failed` with the reason after the partial output and the error
/// line. The session-file contract is unchanged: appends stay
/// best-effort, the bill still rides the file.
/// One headless turn. The parameter count mirrors the shape of the
/// thing: the full tool-loop surface plus the record sink.
#[allow(clippy::too_many_arguments)]
pub fn run_with(
    provider: Arc<dyn Provider>,
    registry: Arc<ToolRegistry>,
    gate: Option<Arc<ToolGate>>,
    history: &mut Vec<Message>,
    prompt: &str,
    mut session: Option<Session>,
    max_tool_rounds: usize,
    sink: &mut dyn FnMut(&JsonRecord),
) -> Result<String, String> {
    if let Some(session) = session.as_ref() {
        sink(&JsonRecord::Session {
            header: session.header().clone(),
        });
    }
    let added_from = history.len();
    history.push(Message::User(prompt.to_string()));
    sink(&JsonRecord::Message {
        message: Message::User(prompt.to_string()),
    });
    let rx = spawn_agent_turn(provider, registry, gate, history, max_tool_rounds);

    // Mirrors the TUI pump's reconstruction rules (app.rs): tokens
    // accumulate; narration before a tool call is its own assistant
    // message; `Error` marks the reply failed but `TurnEnd` still
    // closes the turn — one finalization path for both outcomes.
    let mut blocks: Vec<AssistantBlock> = Vec::new();
    let mut transcript: Vec<Message> = Vec::new();
    let mut error: Option<String> = None;
    // The round's bill rides the session file even though headless
    // prints nothing (#85) — the front end that resumes shows totals.
    let mut bill: Option<Usage> = None;
    for event in rx {
        match event {
            Event::TurnStart => {}
            Event::Token(token) => {
                sink(&JsonRecord::Token {
                    delta: token.clone(),
                });
                AssistantBlock::push_text(&mut blocks, &token);
            }
            Event::Thinking { delta, field } => {
                sink(&JsonRecord::Thinking {
                    delta: delta.clone(),
                    field,
                });
                AssistantBlock::push_thinking(&mut blocks, &delta, field);
            }
            Event::ToolCall(call) => {
                if !blocks.is_empty() {
                    let message = Message::Assistant(std::mem::take(&mut blocks));
                    sink(&JsonRecord::Message {
                        message: message.clone(),
                    });
                    transcript.push(message);
                }
                let message = Message::ToolCall(call);
                sink(&JsonRecord::Message {
                    message: message.clone(),
                });
                transcript.push(message);
            }
            Event::ToolResult(result) => {
                let message = Message::ToolResult(result);
                sink(&JsonRecord::Message {
                    message: message.clone(),
                });
                transcript.push(message);
            }
            // Headless prints the transcript, not the bill — but it
            // records the bill (AD-0026, #85).
            Event::Usage(recorded) => {
                sink(&JsonRecord::Usage { usage: recorded });
                bill = Some(recorded);
            }
            Event::Error(message) => error = Some(message),
            Event::TurnEnd => break,
        }
    }
    // Defensive: a closed channel without `TurnEnd` still finalizes.
    // Whatever accumulated since the last tool call is the final reply.
    let blocks = std::mem::take(&mut blocks);
    if !blocks.is_empty() {
        let message = Message::Assistant(blocks.clone());
        sink(&JsonRecord::Message {
            message: message.clone(),
        });
        transcript.push(message);
    }
    if let Some(reason) = error.clone() {
        let message = Message::Error(reason.clone());
        sink(&JsonRecord::Message {
            message: message.clone(),
        });
        transcript.push(message);
        sink(&JsonRecord::Failed { reason });
    } else {
        sink(&JsonRecord::Done {
            text: AssistantBlock::text_of(&blocks),
        });
    }

    history.extend(transcript);
    if let Some(session) = session.as_mut() {
        let _ = session.append(&history[added_from..]);
        if let Some(bill) = bill {
            let _ = session.append_usage(&bill);
        }
    }

    match error {
        Some(reason) => Err(reason),
        None => Ok(AssistantBlock::text_of(&blocks)),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::agent::DEFAULT_MAX_TOOL_ROUNDS;
    use crate::test_support::{EchoTool, ScriptedProvider};
    use crate::tool::{ToolCall, ToolResult};

    fn call(id: &str, arguments: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "echo".into(),
            arguments: arguments.into(),
        }
    }

    fn result(id: &str, arguments: &str) -> ToolResult {
        ToolResult {
            call_id: id.into(),
            name: "echo".into(),
            content: format!("echo{arguments}"),
            is_error: false,
            images: Vec::new(),
        }
    }

    fn registry() -> Arc<ToolRegistry> {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(EchoTool)).unwrap();
        Arc::new(reg)
    }

    #[test]
    fn combine_prompt_joins_stdin_and_argument() {
        let joined = combine_prompt(Some("diff body".into()), Some("review this".into())).unwrap();
        assert_eq!(joined, "diff body\nreview this");

        assert_eq!(combine_prompt(Some("piped".into()), None).unwrap(), "piped");
        assert_eq!(combine_prompt(None, Some("typed".into())).unwrap(), "typed");

        // Empty halves count as absent.
        assert_eq!(
            combine_prompt(Some(String::new()), Some("x".into())).unwrap(),
            "x"
        );
        assert!(combine_prompt(None, None).is_err());
        assert!(combine_prompt(Some(String::new()), Some(String::new())).is_err());
    }

    #[test]
    fn run_returns_final_text_and_updates_history() {
        let provider = Arc::new(ScriptedProvider::new(vec![vec![
            Event::Token("hel".into()),
            Event::Token("lo".into()),
            Event::TurnEnd,
        ]]));
        let mut history = Vec::new();

        let reply = run(
            provider,
            Arc::new(ToolRegistry::new()),
            None,
            &mut history,
            "hi",
            None,
            DEFAULT_MAX_TOOL_ROUNDS,
        )
        .unwrap();

        assert_eq!(reply, "hello");
        assert_eq!(
            history,
            vec![Message::User("hi".into()), Message::assistant("hello"),]
        );
    }

    #[test]
    fn run_flushes_narration_before_tool_calls() {
        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                Event::Token("checking".into()),
                Event::ToolCall(call("c1", "{\"x\":1}")),
            ],
            vec![Event::Token("done".into()), Event::TurnEnd],
        ]));
        let mut history = Vec::new();

        let reply = run(
            provider,
            registry(),
            None,
            &mut history,
            "go",
            None,
            DEFAULT_MAX_TOOL_ROUNDS,
        )
        .unwrap();

        assert_eq!(reply, "done");
        assert_eq!(
            history,
            vec![
                Message::User("go".into()),
                Message::assistant("checking"),
                Message::ToolCall(call("c1", "{\"x\":1}")),
                Message::ToolResult(result("c1", "{\"x\":1}")),
                Message::assistant("done"),
            ]
        );
    }

    #[test]
    fn run_reports_error_and_records_partial_output() {
        let provider = Arc::new(ScriptedProvider::new(vec![vec![
            Event::Token("par".into()),
            Event::Error("boom".into()),
            Event::TurnEnd,
        ]]));
        let mut history = Vec::new();

        let reason = run(
            provider,
            Arc::new(ToolRegistry::new()),
            None,
            &mut history,
            "hi",
            None,
            DEFAULT_MAX_TOOL_ROUNDS,
        )
        .unwrap_err();

        assert_eq!(reason, "boom");
        assert_eq!(
            history,
            vec![
                Message::User("hi".into()),
                Message::assistant("par"),
                Message::Error("boom".into()),
            ]
        );
    }

    #[test]
    fn run_appends_the_turn_to_an_open_session() {
        let root = std::env::temp_dir().join(format!("hi-headless-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let session = Session::start_in(&root, Path::new("/p"), "prov", "mod", None, None).unwrap();
        let path = session.path().to_path_buf();

        let provider = Arc::new(ScriptedProvider::new(vec![vec![
            Event::Token("reply".into()),
            Event::TurnEnd,
        ]]));
        let mut history = Vec::new();
        run(
            provider,
            Arc::new(ToolRegistry::new()),
            None,
            &mut history,
            "question",
            Some(session),
            DEFAULT_MAX_TOOL_ROUNDS,
        )
        .unwrap();

        let loaded = crate::session::load(&path).unwrap();
        assert_eq!(loaded.messages, history);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn run_persists_the_bill_for_the_next_front_end() {
        // Headless prints no bill, but the record rides the session
        // file — the TUI that resumes this session shows the totals.
        let root = std::env::temp_dir().join(format!("hi-headless-bill-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let session = Session::start_in(&root, Path::new("/p"), "prov", "mod", None, None).unwrap();
        let path = session.path().to_path_buf();

        let provider = Arc::new(ScriptedProvider::new(vec![vec![
            Event::Token("reply".into()),
            Event::Usage(crate::event::Usage {
                prompt_tokens: 1000,
                completion_tokens: 250,
                cached_tokens: Some(700),
                reasoning_tokens: Some(100),
            }),
            Event::TurnEnd,
        ]]));
        let mut history = Vec::new();
        run(
            provider,
            Arc::new(ToolRegistry::new()),
            None,
            &mut history,
            "question",
            Some(session),
            DEFAULT_MAX_TOOL_ROUNDS,
        )
        .unwrap();

        let loaded = crate::session::load(&path).unwrap();
        assert_eq!(loaded.usage.completion_tokens, 250);
        assert_eq!(loaded.usage.cached_tokens, Some(700));
        assert_eq!(loaded.usage_last_prompt, 1000);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn configured_round_cap_is_honored() {
        let round = || {
            vec![
                Event::TurnStart,
                Event::ToolCall(ToolCall {
                    id: "c1".into(),
                    name: "echo".into(),
                    arguments: "{}".into(),
                }),
                Event::TurnEnd,
            ]
        };
        let provider = Arc::new(ScriptedProvider::new(vec![round(), round()]));
        let mut history = Vec::new();
        let err = run_with(
            provider,
            Arc::new(ToolRegistry::new()),
            None,
            &mut history,
            "hi",
            None,
            2,
            &mut |_| {},
        )
        .unwrap_err();
        assert!(err.contains("did not settle within 2 rounds"), "{err}");
    }

    // ── --mode json (#18, AD-0038) ────────────────────────────────

    /// Drain with a collecting sink; returns (result, records).
    fn json_run(
        provider: Arc<dyn Provider>,
        registry: Arc<ToolRegistry>,
        history: &mut Vec<Message>,
        prompt: &str,
        session: Option<Session>,
        max_tool_rounds: usize,
    ) -> (Result<String, String>, Vec<JsonRecord>) {
        let mut records = Vec::new();
        let result = run_with(
            provider,
            registry,
            None,
            history,
            prompt,
            session,
            max_tool_rounds,
            &mut |record| records.push(record.clone()),
        );
        (result, records)
    }

    fn kind(record: &JsonRecord) -> &'static str {
        match record {
            JsonRecord::Session { .. } => "session",
            JsonRecord::Token { .. } => "token",
            JsonRecord::Thinking { .. } => "thinking",
            JsonRecord::Message { .. } => "message",
            JsonRecord::Usage { .. } => "usage",
            JsonRecord::Done { .. } => "done",
            JsonRecord::Failed { .. } => "failed",
        }
    }

    #[test]
    fn json_records_follow_the_transcript_and_end_with_done() {
        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                Event::Token("checking".into()),
                Event::ToolCall(call("c1", "{\"x\":1}")),
            ],
            vec![Event::Token("done".into()), Event::TurnEnd],
        ]));
        let mut history = Vec::new();

        let (reply, records) = json_run(
            provider,
            registry(),
            &mut history,
            "go",
            None,
            DEFAULT_MAX_TOOL_ROUNDS,
        );

        assert_eq!(reply.unwrap(), "done");
        let kinds: Vec<&str> = records.iter().map(kind).collect();
        assert_eq!(
            kinds,
            vec![
                "message", // the user prompt
                "token",   // "checking"
                "message", // narration flushes as its own assistant message
                "message", // the tool call
                "message", // the tool result
                "token",   // "done"
                "message", // the final assistant message
                "done",    // terminal record carries the reply
            ]
        );
        assert_eq!(
            records[0],
            JsonRecord::Message {
                message: Message::User("go".into())
            }
        );
        assert_eq!(
            records[2],
            JsonRecord::Message {
                message: Message::assistant("checking")
            }
        );
        assert_eq!(
            records.last().unwrap(),
            &JsonRecord::Done {
                text: "done".into()
            }
        );
        // `Token` deltas concatenate to the full reply — the stream
        // and the transcript agree.
        let deltas: String = records
            .iter()
            .filter_map(|r| match r {
                JsonRecord::Token { delta } => Some(delta.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, "checkingdone");
    }

    #[test]
    fn json_mode_failure_ends_with_failed_after_the_partial_output() {
        let provider = Arc::new(ScriptedProvider::new(vec![vec![
            Event::Token("par".into()),
            Event::Error("boom".into()),
            Event::TurnEnd,
        ]]));
        let mut history = Vec::new();

        let (result, records) = json_run(
            provider,
            registry(),
            &mut history,
            "hi",
            None,
            DEFAULT_MAX_TOOL_ROUNDS,
        );

        assert_eq!(result.unwrap_err(), "boom");
        let kinds: Vec<&str> = records.iter().map(kind).collect();
        assert_eq!(
            kinds,
            vec!["message", "token", "message", "message", "failed"]
        );
        assert_eq!(
            records[2],
            JsonRecord::Message {
                message: Message::assistant("par")
            }
        );
        assert_eq!(
            records[3],
            JsonRecord::Message {
                message: Message::Error("boom".into())
            }
        );
        assert_eq!(
            records.last().unwrap(),
            &JsonRecord::Failed {
                reason: "boom".into()
            }
        );
    }

    #[test]
    fn json_mode_opens_with_the_session_header_when_a_session_is_open() {
        let root = std::env::temp_dir().join(format!("hi-json-header-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let session = Session::start_in(&root, Path::new("/p"), "prov", "mod", None, None).unwrap();
        let header = session.header().clone();

        let provider = Arc::new(ScriptedProvider::new(vec![vec![
            Event::Token("reply".into()),
            Event::TurnEnd,
        ]]));
        let mut history = Vec::new();
        let (_, records) = json_run(
            provider,
            registry(),
            &mut history,
            "question",
            Some(session),
            DEFAULT_MAX_TOOL_ROUNDS,
        );

        assert_eq!(records.first().unwrap(), &JsonRecord::Session { header });
        assert_eq!(kind(&records[1]), "message");
        assert_eq!(kind(records.last().unwrap()), "done");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn json_mode_records_the_rounds_bill_when_reported() {
        let provider = Arc::new(ScriptedProvider::new(vec![vec![
            Event::Token("reply".into()),
            Event::Usage(crate::event::Usage {
                prompt_tokens: 1000,
                completion_tokens: 250,
                cached_tokens: Some(700),
                reasoning_tokens: Some(100),
            }),
            Event::TurnEnd,
        ]]));
        let mut history = Vec::new();

        let (_, records) = json_run(
            provider,
            registry(),
            &mut history,
            "q",
            None,
            DEFAULT_MAX_TOOL_ROUNDS,
        );

        let kinds: Vec<&str> = records.iter().map(kind).collect();
        assert_eq!(kinds, vec!["message", "token", "usage", "message", "done"]);
        assert_eq!(
            records[2],
            JsonRecord::Usage {
                usage: crate::event::Usage {
                    prompt_tokens: 1000,
                    completion_tokens: 250,
                    cached_tokens: Some(700),
                    reasoning_tokens: Some(100),
                },
            }
        );
    }
}
