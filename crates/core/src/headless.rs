//! One-shot, non-interactive turns: `hi --print`.
//!
//! Headless runs share everything interactive runs use — the same agent
//! loop, tool registry, provider selection, and session files — but skip
//! the interface entirely: a prompt goes in, the final assistant text
//! comes back, the process exits. This module owns the drain and never
//! touches the terminal; the CLI prints what [`run`] returns.

use std::sync::Arc;

use crate::agent::{DEFAULT_MAX_TOOL_ROUNDS, spawn_agent_turn};
use crate::event::{Event, Usage};
use crate::message::Message;
use crate::provider::Provider;
use crate::session::Session;
use crate::tool::{ToolGate, ToolRegistry};

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
/// loop to completion, and return the final assistant text.
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
    mut session: Option<Session>,
) -> Result<String, String> {
    let added_from = history.len();
    history.push(Message::User(prompt.to_string()));
    let rx = spawn_agent_turn(provider, registry, gate, history, DEFAULT_MAX_TOOL_ROUNDS);

    // Mirrors the TUI pump's reconstruction rules (app.rs): tokens
    // accumulate; narration before a tool call is its own assistant
    // message; `Error` marks the reply failed but `TurnEnd` still
    // closes the turn — one finalization path for both outcomes.
    let mut text = String::new();
    let mut transcript: Vec<Message> = Vec::new();
    let mut error: Option<String> = None;
    // The round's bill rides the session file even though headless
    // prints nothing (#85) — the front end that resumes shows totals.
    let mut bill: Option<Usage> = None;
    for event in rx {
        match event {
            Event::TurnStart => {}
            Event::Token(token) => text.push_str(&token),
            Event::ToolCall(call) => {
                if !text.is_empty() {
                    transcript.push(Message::Assistant(std::mem::take(&mut text)));
                }
                transcript.push(Message::ToolCall(call));
            }
            Event::ToolResult(result) => transcript.push(Message::ToolResult(result)),
            // Headless prints the transcript, not the bill — but it
            // records the bill (AD-0026, #85).
            Event::Usage(recorded) => bill = Some(recorded),
            Event::Error(message) => error = Some(message),
            Event::TurnEnd => break,
        }
    }
    // Defensive: a closed channel without `TurnEnd` still finalizes.
    // Whatever accumulated since the last tool call is the final reply.
    let reply = std::mem::take(&mut text);
    if !reply.is_empty() {
        transcript.push(Message::Assistant(reply.clone()));
    }
    if let Some(reason) = error.clone() {
        transcript.push(Message::Error(reason));
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
        None => Ok(reply),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
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
        )
        .unwrap();

        assert_eq!(reply, "hello");
        assert_eq!(
            history,
            vec![
                Message::User("hi".into()),
                Message::Assistant("hello".into()),
            ]
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

        let reply = run(provider, registry(), None, &mut history, "go", None).unwrap();

        assert_eq!(reply, "done");
        assert_eq!(
            history,
            vec![
                Message::User("go".into()),
                Message::Assistant("checking".into()),
                Message::ToolCall(call("c1", "{\"x\":1}")),
                Message::ToolResult(result("c1", "{\"x\":1}")),
                Message::Assistant("done".into()),
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
        )
        .unwrap_err();

        assert_eq!(reason, "boom");
        assert_eq!(
            history,
            vec![
                Message::User("hi".into()),
                Message::Assistant("par".into()),
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
        )
        .unwrap();

        let loaded = crate::session::load(&path).unwrap();
        assert_eq!(loaded.usage.completion_tokens, 250);
        assert_eq!(loaded.usage.cached_tokens, Some(700));
        assert_eq!(loaded.usage_last_prompt, 1000);
        let _ = std::fs::remove_dir_all(&root);
    }
}
