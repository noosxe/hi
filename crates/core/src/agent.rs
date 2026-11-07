//! The agent loop: rounds of model turns and tool executions until the
//! model settles.
//!
//! The loop is the reason `hi` is a harness and not a chat client. One
//! call to [`spawn_agent_turn`] covers the whole life of a user message:
//! the model streams a reply, possibly asks for tools, the harness runs
//! them (through the [`ToolGate`] seam), feeds the results back, and
//! repeats — until the model produces a round with no tool calls.
//!
//! The event contract is *preserved, not extended*: the consumer still
//! sees exactly one `TurnStart`, the interleaved happenings, and exactly
//! one `TurnEnd` (with any `Event::Error` immediately before it). The
//! per-round `TurnStart`/`TurnEnd` pairs coming from the provider are an
//! implementation detail, swallowed here. Everything else — dropped
//! receiver as cancellation, tokens in order — works exactly as
//! documented in `docs/turn-lifecycle.md`.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;

use crate::event::Event;
use crate::message::{AssistantBlock, Message};
use crate::provider::{Provider, spawn_turn};
use crate::tool::{ToolCall, ToolDecision, ToolGate, ToolRegistry, ToolResult};

/// Cap on model↔tool rounds per user turn.
///
/// A runaway model that keeps demanding tools must not spin forever;
/// hitting the cap is a loud `Event::Error`, not a silent stop. The
/// number is a safety belt, not a feature — it exists so an unattended
/// turn cannot burn the night, not because 32 is a magic threshold.
pub const DEFAULT_MAX_TOOL_ROUNDS: usize = 32;

/// The round-cap failure fragment. Shared with the subagent runner
/// (#4) so it can recognize its own loop's cap error and enrich it
/// for the parent model; the human-facing sentence is built around it.
pub const ROUND_CAP_MARKER: &str = "did not settle within";

/// Run one full agent turn on a background thread; events come back
/// through the returned `Receiver`.
///
/// This is [`spawn_turn`]'s big sibling: same shape, same cancellation
/// contract (dropping the `Receiver` stops the loop at the next send),
/// but the turn may span many provider rounds and tool executions.
/// The loop never mutates the caller's `history` — consumers mirror the
/// event stream into their own transcript (token accumulation with a
/// flush at each `ToolCall`/`ToolResult` event reproduces the history
/// the loop built internally).
pub fn spawn_agent_turn(
    provider: Arc<dyn Provider>,
    registry: Arc<ToolRegistry>,
    gate: Option<Arc<ToolGate>>,
    history: &[Message],
    max_tool_rounds: usize,
) -> Receiver<Event> {
    let history = history.to_vec();
    let (tx, rx) = channel();
    thread::spawn(move || {
        run_agent_turn(
            provider,
            registry,
            gate.as_deref(),
            history,
            max_tool_rounds,
            tx,
        );
    });
    rx
}

/// The loop itself, on the worker thread. `working` is the loop's own
/// copy of the conversation, grown with each round's calls and results;
/// it is what the next round's provider request replays.
fn run_agent_turn(
    provider: Arc<dyn Provider>,
    registry: Arc<ToolRegistry>,
    gate: Option<&ToolGate>,
    mut working: Vec<Message>,
    max_tool_rounds: usize,
    tx: Sender<Event>,
) {
    // The one and only TurnStart for this user turn.
    if tx.send(Event::TurnStart).is_err() {
        return;
    }
    for _round in 0..max_tool_rounds {
        let rx = spawn_turn(Arc::clone(&provider), &working);
        let mut round_blocks: Vec<AssistantBlock> = Vec::new();
        let mut calls: Vec<ToolCall> = Vec::new();
        let mut failed = false;
        // Did the round's own `TurnEnd` arrive? A provider thread that
        // panics drops its sender mid-round: the channel closes with no
        // `TurnEnd` and, without this flag, the loop would read "no
        // calls" as "settled" and finish cleanly (#172).
        let mut round_ended = false;
        for event in rx {
            match event {
                // Round opener — the consumer already got the turn's
                // single TurnStart.
                Event::TurnStart => {}
                Event::Token(token) => {
                    AssistantBlock::push_text(&mut round_blocks, &token);
                    if tx.send(Event::Token(token)).is_err() {
                        return;
                    }
                }
                Event::Thinking { delta, field } => {
                    AssistantBlock::push_thinking(&mut round_blocks, &delta, field);
                    if tx.send(Event::Thinking { delta, field }).is_err() {
                        return;
                    }
                }
                Event::ToolCall(call) => {
                    // Forward live (the UI shows the request as soon as
                    // the model makes it) and remember for dispatch.
                    if tx.send(Event::ToolCall(call.clone())).is_err() {
                        return;
                    }
                    calls.push(call);
                }
                // Providers do not emit results today; if one ever
                // does, it rides through untouched rather than being
                // dropped silently.
                result @ Event::ToolResult(_) => {
                    if tx.send(result).is_err() {
                        return;
                    }
                }
                // The round's bill rides through untouched, same rule.
                usage @ Event::Usage(_) => {
                    if tx.send(usage).is_err() {
                        return;
                    }
                }
                Event::Error(err) => {
                    failed = true;
                    if tx.send(Event::Error(err)).is_err() {
                        return;
                    }
                }
                // Round closer — swallowed. Only this loop knows when
                // the turn truly ends: after a settled round, after a
                // failed round, or at the round cap.
                Event::TurnEnd => round_ended = true,
                // Providers never send probes; if one did, it is not
                // content and is not forwarded.
                Event::Heartbeat => {}
            }
        }
        if !round_ended && !failed {
            // The stream closed without `TurnEnd` or `Error`: the
            // provider thread died (panic). Report it on the single
            // finalization path; whatever calls were collected are
            // incomplete and never dispatched.
            let _ = tx.send(Event::Error(PROVIDER_DIED.into()));
            let _ = tx.send(Event::TurnEnd);
            return;
        }
        if failed {
            // The round itself broke (network, HTTP status, malformed
            // stream). The contract already sent `Error`; close the
            // turn. Tool calls from a broken round are incomplete by
            // definition — never dispatch them.
            let _ = tx.send(Event::TurnEnd);
            return;
        }
        if calls.is_empty() {
            // Settled: the model produced text and asked for nothing.
            let _ = tx.send(Event::TurnEnd);
            return;
        }
        // Feed the next round everything it needs to replay: the
        // assistant text that preceded the calls (possibly none), every
        // call, then one result per call — the exact grouping the wire
        // layer maps onto OpenAI's message shapes.
        if !round_blocks.is_empty() {
            working.push(Message::Assistant(round_blocks));
        }
        for call in &calls {
            working.push(Message::ToolCall(call.clone()));
        }
        // Sequential execution, in the order the model proposed them.
        // Deterministic beats clever: results land in a stable order,
        // and parallel execution is background-task territory (#5).
        for call in &calls {
            let result =
                decide_and_execute(gate, &registry, call, &|| tx.send(Event::Heartbeat).is_ok());
            working.push(Message::ToolResult(result.clone()));
            if tx.send(Event::ToolResult(result)).is_err() {
                return;
            }
        }
    }
    // Round cap hit without a settled round: fail the turn loudly, on
    // the one finalization path.
    let _ = tx.send(Event::Error(format!(
        "tool loop {ROUND_CAP_MARKER} {max_tool_rounds} rounds"
    )));
    let _ = tx.send(Event::TurnEnd);
}

/// The error for a round whose provider thread vanished mid-stream.
const PROVIDER_DIED: &str = "provider thread ended without finishing the round (it panicked)";

/// Best-effort text of a caught panic payload: `panic!("literal")`
/// carries a `&str`, `panic!("{x}")` and `panic_any(String)` a `String`.
pub(crate) fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_string()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "non-string panic payload".into()
    }
}

/// [`run_call`] behind `catch_unwind`: a panicking tool (or gate) is an
/// error *result* — the model sees the message and the turn goes on —
/// not a dead agent thread with no `ToolResult` and no `TurnEnd` (#172).
/// The gate runs inside the wrapper on purpose: a gate panic kills the
/// thread exactly as a tool panic does. `AssertUnwindSafe` is sound
/// because nothing the closure borrows is observed half-updated: the
/// tool `&self` state is the tool's own problem (std `Mutex` poisoning
/// still guards it) and the rest is read-only.
fn decide_and_execute(
    gate: Option<&ToolGate>,
    registry: &ToolRegistry,
    call: &ToolCall,
    alive: &dyn Fn() -> bool,
) -> ToolResult {
    let run = std::panic::AssertUnwindSafe(|| run_call(gate, registry, call, alive));
    match std::panic::catch_unwind(run) {
        Ok(result) => result,
        Err(payload) => ToolResult {
            call_id: call.id.clone(),
            name: call.name.clone(),
            content: format!(
                "tool `{}` panicked: {}",
                call.name,
                panic_message(&*payload)
            ),
            is_error: true,
            images: Vec::new(),
        },
    }
}

/// Consult the gate (if any), validate, execute — and always produce
/// exactly one result. Every path through here is a `ToolResult`, never
/// an `Event::Error`: a refused or failed call is a *conversation*
/// step (the model sees why and can adapt), not a broken turn.
fn run_call(
    gate: Option<&ToolGate>,
    registry: &ToolRegistry,
    call: &ToolCall,
    alive: &dyn Fn() -> bool,
) -> ToolResult {
    let denied = |content: String| ToolResult {
        call_id: call.id.clone(),
        name: call.name.clone(),
        content,
        is_error: true,
        images: Vec::new(),
    };
    let arguments = match gate {
        Some(gate) => match gate(call) {
            ToolDecision::Allow => call.arguments.clone(),
            ToolDecision::Mutate(arguments) => arguments,
            ToolDecision::Deny(reason) => {
                return denied(format!("tool call denied: {reason}"));
            }
        },
        None => call.arguments.clone(),
    };
    // Arguments must be a JSON object — that is what the wire format
    // promises the tool's `parameters` schema describes. Checking here
    // means a malformed call never reaches tool code.
    let parsed: serde_json::Value = match serde_json::from_str(&arguments) {
        Ok(value) => value,
        Err(err) => return denied(format!("tool arguments are not valid JSON: {err}")),
    };
    if !parsed.is_object() {
        return denied("tool arguments must be a JSON object".into());
    }
    let Some(tool) = registry.get(&call.name) else {
        return denied(format!("unknown tool: {}", call.name));
    };
    match tool.execute_with_probe(&arguments, alive) {
        Ok(output) => ToolResult {
            call_id: call.id.clone(),
            name: call.name.clone(),
            content: output.content,
            is_error: false,
            images: output.images,
        },
        Err(err) => denied(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{EchoTool, PanicProvider, PanicTool, ScriptedProvider};
    use crate::tool::{ImageBlock, Tool, ToolOutput};

    fn registry_with(tool: Arc<dyn Tool>) -> Arc<ToolRegistry> {
        let mut registry = ToolRegistry::new();
        registry.register(tool).unwrap();
        Arc::new(registry)
    }

    fn call(id: &str, name: &str, arguments: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: arguments.into(),
        }
    }

    fn collect(rx: Receiver<Event>) -> Vec<Event> {
        rx.iter().collect()
    }

    fn tool_results(events: &[Event]) -> Vec<ToolResult> {
        events
            .iter()
            .filter_map(|event| match event {
                Event::ToolResult(result) => Some(result.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn background_task_tools_run_through_the_loop() {
        // The model spawns a task and polls it in the next round —
        // the loop itself never blocks on the task (#5, AD-0019).
        let tasks = crate::tasks::TaskManager::new(std::env::temp_dir().join("hi-agent-tasks"));
        let mut registry = crate::tool::ToolRegistry::new();
        for tool in [
            Arc::new(crate::tools::TaskRunTool::new(
                tasks.clone(),
                std::env::temp_dir(),
            )) as Arc<dyn Tool>,
            Arc::new(crate::tools::TaskStatusTool::new(tasks.clone())),
            Arc::new(crate::tools::TaskOutputTool::new(tasks.clone())),
            Arc::new(crate::tools::TaskCancelTool::new(tasks.clone())),
        ] {
            registry.register(tool).unwrap();
        }
        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                Event::TurnStart,
                Event::ToolCall(call("c1", "task_run", r#"{"command":"echo spawn-ok"}"#)),
                Event::TurnEnd,
            ],
            vec![
                Event::TurnStart,
                // The echo text is not model-visible timing: task_status
                // with no id lists everything, whatever the state.
                Event::ToolCall(call("c2", "task_status", "{}")),
                Event::TurnEnd,
            ],
            vec![
                Event::TurnStart,
                Event::Token("settled".into()),
                Event::TurnEnd,
            ],
        ]));
        let events = collect(spawn_agent_turn(
            provider,
            Arc::new(registry),
            None,
            &[Message::User("spawn something".into())],
            DEFAULT_MAX_TOOL_ROUNDS,
        ));
        let results = tool_results(&events);
        assert_eq!(results.len(), 2);
        assert!(!results[0].is_error, "{}", results[0].content);
        assert!(
            results[0].content.contains("background task #1"),
            "{}",
            results[0].content
        );
        assert!(!results[1].is_error, "{}", results[1].content);
        assert!(
            results[1].content.contains("'echo spawn-ok'"),
            "{}",
            results[1].content
        );
        // The spawned task finished with the expected output — wait on
        // the fact, never on timing (the reader thread lands the bytes).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let tail = loop {
            let tail = tasks.output_tail(1, 4096).unwrap_or_default();
            if tail.contains("spawn-ok") {
                break tail;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "spawn-ok never reached the output ring: {tail}"
            );
            std::thread::sleep(std::time::Duration::from_millis(25));
        };
        let _ = tail;
    }

    fn panicking_tool_rounds() -> Arc<ScriptedProvider> {
        Arc::new(ScriptedProvider::new(vec![
            vec![
                Event::TurnStart,
                Event::ToolCall(call("c1", "boom", "{}")),
                Event::TurnEnd,
            ],
            vec![
                Event::TurnStart,
                Event::Token("recovered".into()),
                Event::TurnEnd,
            ],
        ]))
    }

    #[test]
    fn a_panicking_tool_becomes_an_error_result_and_the_turn_continues() {
        // #172, both payload types (`&str` and `String`).
        for (owned, message) in [(false, "kaboom literal"), (true, "kaboom owned")] {
            let provider = panicking_tool_rounds();
            let events = collect(spawn_agent_turn(
                provider.clone(),
                registry_with(Arc::new(PanicTool { owned })),
                None,
                &[Message::User("go".into())],
                DEFAULT_MAX_TOOL_ROUNDS,
            ));
            let results = tool_results(&events);
            assert_eq!(results.len(), 1, "{events:?}");
            assert!(results[0].is_error);
            assert_eq!(results[0].call_id, "c1");
            assert!(
                results[0].content.contains(message),
                "{}",
                results[0].content
            );
            assert!(
                results[0].content.contains("panicked"),
                "{}",
                results[0].content
            );
            // The next round ran and saw the error result.
            assert_eq!(provider.received.lock().unwrap().len(), 2);
            assert!(events.contains(&Event::Token("recovered".into())));
            let ends = events.iter().filter(|e| **e == Event::TurnEnd).count();
            assert_eq!(ends, 1);
            assert_eq!(events.last(), Some(&Event::TurnEnd));
            assert!(!events.iter().any(|e| matches!(e, Event::Error(_))));
        }
    }

    #[test]
    fn a_panicking_gate_becomes_an_error_result_too() {
        let gate: Arc<ToolGate> = Arc::new(|_call: &ToolCall| -> ToolDecision {
            panic!("gate exploded");
        });
        let events = collect(spawn_agent_turn(
            panicking_tool_rounds(),
            registry_with(Arc::new(PanicTool { owned: false })),
            Some(gate),
            &[Message::User("go".into())],
            DEFAULT_MAX_TOOL_ROUNDS,
        ));
        let results = tool_results(&events);
        assert_eq!(results.len(), 1, "{events:?}");
        assert!(results[0].is_error);
        assert!(results[0].content.contains("gate exploded"));
        assert_eq!(events.last(), Some(&Event::TurnEnd));
    }

    #[test]
    fn a_panicking_provider_reports_an_error_then_one_turn_end() {
        // #172: the provider thread dies mid-round, with and without
        // partial output and a collected (incomplete) call.
        for before in [
            vec![Event::TurnStart],
            vec![
                Event::TurnStart,
                Event::Token("part".into()),
                Event::ToolCall(call("c1", "echo", "{}")),
            ],
        ] {
            let had_call = before.iter().any(|e| matches!(e, Event::ToolCall(_)));
            let events = collect(spawn_agent_turn(
                Arc::new(PanicProvider { before }),
                registry_with(Arc::new(EchoTool)),
                None,
                &[Message::User("go".into())],
                DEFAULT_MAX_TOOL_ROUNDS,
            ));
            let n = events.len();
            assert!(n >= 3, "{events:?}");
            assert!(
                matches!(&events[n - 2], Event::Error(e) if e.contains("provider")),
                "{events:?}"
            );
            assert_eq!(events[n - 1], Event::TurnEnd);
            assert_eq!(events.iter().filter(|e| **e == Event::TurnEnd).count(), 1);
            assert_eq!(events.iter().filter(|e| **e == Event::TurnStart).count(), 1);
            // An incomplete round never dispatches its calls.
            assert!(tool_results(&events).is_empty(), "had_call={had_call}");
        }
    }

    #[test]
    fn provider_without_tool_calls_passes_straight_through() {
        let provider = Arc::new(ScriptedProvider::new(vec![vec![
            Event::TurnStart,
            Event::Token("hello ".into()),
            Event::Token("world".into()),
            Event::TurnEnd,
        ]]));
        let events = collect(spawn_agent_turn(
            provider,
            Arc::new(ToolRegistry::new()),
            None,
            &[Message::User("hi".into())],
            DEFAULT_MAX_TOOL_ROUNDS,
        ));
        // Exactly the scripted round, re-framed by the loop's own
        // TurnStart/TurnEnd — no duplicate framing from swallowed round
        // markers.
        assert_eq!(
            events,
            vec![
                Event::TurnStart,
                Event::Token("hello ".into()),
                Event::Token("world".into()),
                Event::TurnEnd,
            ]
        );
    }

    #[test]
    fn executes_tool_then_settles_with_result_fed_back() {
        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                Event::TurnStart,
                Event::Token("Checking. ".into()),
                Event::ToolCall(call("c1", "echo", r#"{"msg":"hey"}"#)),
                Event::TurnEnd,
            ],
            vec![
                Event::TurnStart,
                Event::Token("All done.".into()),
                Event::TurnEnd,
            ],
        ]));
        let registry = registry_with(Arc::new(EchoTool));
        let events = collect(spawn_agent_turn(
            provider.clone(),
            registry,
            None,
            &[Message::User("run echo".into())],
            DEFAULT_MAX_TOOL_ROUNDS,
        ));

        // Unified stream: text, the call, its result, then round two.
        assert_eq!(events.first(), Some(&Event::TurnStart));
        assert_eq!(events.last(), Some(&Event::TurnEnd));
        assert_eq!(
            events,
            vec![
                Event::TurnStart,
                Event::Token("Checking. ".into()),
                Event::ToolCall(call("c1", "echo", r#"{"msg":"hey"}"#)),
                Event::ToolResult(ToolResult {
                    call_id: "c1".into(),
                    name: "echo".into(),
                    content: r#"echo{"msg":"hey"}"#.into(),
                    is_error: false,
                    images: Vec::new(),
                }),
                Event::Token("All done.".into()),
                Event::TurnEnd,
            ]
        );

        // The second round's request carried the full pairing: assistant
        // text, the call, the result — in order.
        let received = provider.received.lock().unwrap();
        assert_eq!(received.len(), 2);
        let replayed = &received[1];
        assert_eq!(
            replayed,
            &vec![
                Message::User("run echo".into()),
                Message::assistant("Checking. "),
                Message::ToolCall(call("c1", "echo", r#"{"msg":"hey"}"#)),
                Message::ToolResult(ToolResult {
                    call_id: "c1".into(),
                    name: "echo".into(),
                    content: r#"echo{"msg":"hey"}"#.into(),
                    is_error: false,
                    images: Vec::new(),
                }),
            ]
        );
    }

    #[test]
    fn deny_produces_error_result_and_the_loop_continues() {
        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                Event::TurnStart,
                Event::ToolCall(call("c1", "echo", "{}")),
                Event::TurnEnd,
            ],
            vec![
                Event::TurnStart,
                Event::Token("understood".into()),
                Event::TurnEnd,
            ],
        ]));
        let registry = registry_with(Arc::new(EchoTool));
        let gate: Arc<ToolGate> = Arc::new(|_call| ToolDecision::Deny("not allowed".into()));
        let events = collect(spawn_agent_turn(
            provider,
            registry,
            Some(gate),
            &[],
            DEFAULT_MAX_TOOL_ROUNDS,
        ));
        let results = tool_results(&events);
        assert_eq!(results.len(), 1);
        assert!(results[0].is_error);
        assert_eq!(results[0].content, "tool call denied: not allowed");
        // The model still got to speak again after the refusal.
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::Token(text) if text == "understood"))
        );
        assert_eq!(events.last(), Some(&Event::TurnEnd));
    }

    #[test]
    fn mutate_replaces_arguments_before_execution() {
        let provider = Arc::new(ScriptedProvider::new(vec![vec![
            Event::TurnStart,
            Event::ToolCall(call("c1", "echo", r#"{"msg":"original"}"#)),
            Event::TurnEnd,
        ]]));
        let registry = registry_with(Arc::new(EchoTool));
        let gate: Arc<ToolGate> =
            Arc::new(|_call| ToolDecision::Mutate(r#"{"msg":"mutated"}"#.into()));
        let events = collect(spawn_agent_turn(
            provider,
            registry,
            Some(gate),
            &[],
            DEFAULT_MAX_TOOL_ROUNDS,
        ));
        let results = tool_results(&events);
        assert_eq!(results.len(), 1);
        assert!(!results[0].is_error);
        assert_eq!(results[0].content, r#"echo{"msg":"mutated"}"#);
    }

    #[test]
    fn unknown_tool_is_an_error_result_not_a_broken_turn() {
        let provider = Arc::new(ScriptedProvider::new(vec![vec![
            Event::TurnStart,
            Event::ToolCall(call("c1", "ghost", "{}")),
            Event::TurnEnd,
        ]]));
        let events = collect(spawn_agent_turn(
            provider,
            Arc::new(ToolRegistry::new()),
            None,
            &[],
            DEFAULT_MAX_TOOL_ROUNDS,
        ));
        let results = tool_results(&events);
        assert_eq!(results.len(), 1);
        assert!(results[0].is_error);
        assert_eq!(results[0].content, "unknown tool: ghost");
        assert_eq!(events.last(), Some(&Event::TurnEnd));
    }

    #[test]
    fn non_object_arguments_never_reach_the_tool() {
        let provider = Arc::new(ScriptedProvider::new(vec![vec![
            Event::TurnStart,
            Event::ToolCall(call("c1", "echo", "[1,2,3]")),
            Event::TurnEnd,
        ]]));
        let registry = registry_with(Arc::new(EchoTool));
        let events = collect(spawn_agent_turn(
            provider,
            registry,
            None,
            &[],
            DEFAULT_MAX_TOOL_ROUNDS,
        ));
        let results = tool_results(&events);
        assert!(results[0].is_error);
        assert_eq!(results[0].content, "tool arguments must be a JSON object");
    }

    #[test]
    fn malformed_json_arguments_are_reported_verbatim() {
        let provider = Arc::new(ScriptedProvider::new(vec![vec![
            Event::TurnStart,
            Event::ToolCall(call("c1", "echo", "{not json")),
            Event::TurnEnd,
        ]]));
        let registry = registry_with(Arc::new(EchoTool));
        let events = collect(spawn_agent_turn(
            provider,
            registry,
            None,
            &[],
            DEFAULT_MAX_TOOL_ROUNDS,
        ));
        let results = tool_results(&events);
        assert!(results[0].is_error);
        assert!(
            results[0]
                .content
                .starts_with("tool arguments are not valid JSON")
        );
    }

    #[test]
    fn round_cap_fails_the_turn_on_the_single_path() {
        let round = || {
            vec![
                Event::TurnStart,
                Event::ToolCall(call("c1", "echo", "{}")),
                Event::TurnEnd,
            ]
        };
        let provider = Arc::new(ScriptedProvider::new(vec![round(), round()]));
        let registry = registry_with(Arc::new(EchoTool));
        let events = collect(spawn_agent_turn(
            provider,
            registry,
            None,
            &[],
            2, // exactly the two scripted rounds, both unsettled
        ));
        assert_eq!(events.last(), Some(&Event::TurnEnd));
        let error = events.iter().find_map(|event| match event {
            Event::Error(msg) => Some(msg.clone()),
            _ => None,
        });
        assert_eq!(
            error.as_deref(),
            Some("tool loop did not settle within 2 rounds")
        );
    }

    #[test]
    fn failed_round_aborts_without_dispatching() {
        let provider = Arc::new(ScriptedProvider::new(vec![vec![
            Event::TurnStart,
            Event::Error("connection reset".into()),
            Event::TurnEnd,
        ]]));
        let registry = registry_with(Arc::new(EchoTool));
        let events = collect(spawn_agent_turn(
            provider.clone(),
            registry,
            None,
            &[],
            DEFAULT_MAX_TOOL_ROUNDS,
        ));
        // Error passes through, TurnEnd closes, and — crucially — the
        // loop ran exactly one round (no second scripted round consumed).
        let received = provider.received.lock().unwrap();
        assert_eq!(received.len(), 1);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::Error(msg) if msg == "connection reset"))
        );
        assert_eq!(events.last(), Some(&Event::TurnEnd));
    }
    #[test]
    fn image_tool_outputs_flow_into_results() {
        // A tool overriding `execute_with_images` lands its images on
        // the ToolResult — the loop's only job is to carry them.
        struct ImageEcho;
        impl Tool for ImageEcho {
            fn name(&self) -> &str {
                "imgecho"
            }
            fn description(&self) -> &str {
                "attaches"
            }
            fn parameters(&self) -> serde_json::Value {
                serde_json::json!({"type": "object", "properties": {}})
            }
            fn execute(&self, _arguments: &str) -> Result<String, String> {
                unreachable!("the loop calls execute_with_images only")
            }
            fn execute_with_images(&self, _arguments: &str) -> Result<ToolOutput, String> {
                Ok(ToolOutput {
                    content: "Read image file [image/png] (attached)".into(),
                    images: vec![ImageBlock {
                        data: "aW1n".into(),
                        mime_type: "image/png".into(),
                    }],
                })
            }
        }

        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                Event::TurnStart,
                Event::ToolCall(call("c1", "imgecho", "{}")),
                Event::TurnEnd,
            ],
            vec![
                Event::TurnStart,
                Event::Token("seen".into()),
                Event::TurnEnd,
            ],
        ]));
        let events = collect(spawn_agent_turn(
            provider.clone(),
            registry_with(Arc::new(ImageEcho)),
            None,
            &[Message::User("look".into())],
            DEFAULT_MAX_TOOL_ROUNDS,
        ));
        let results = tool_results(&events);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].images.len(), 1);
        assert_eq!(results[0].images[0].mime_type, "image/png");
        // The replayed history carries the images too — sessions keep
        // the facts even though the wire re-derives the carrier.
        let received = provider.received.lock().unwrap();
        let replayed = &received[1];
        let Message::ToolResult(result) = &replayed[2] else {
            panic!("expected the tool result in the replay");
        };
        assert_eq!(result.images.len(), 1);
    }

    fn probe_round() -> Vec<Vec<Event>> {
        vec![
            vec![
                Event::TurnStart,
                Event::ToolCall(call("p1", "probe", "{}")),
                Event::TurnEnd,
            ],
            vec![Event::TurnStart, Event::Token("ok".into()), Event::TurnEnd],
        ]
    }

    #[test]
    fn a_tool_probe_sends_heartbeats_and_sees_a_live_consumer() {
        // #170: the loop builds the probe from its own `tx`.
        let provider = Arc::new(ScriptedProvider::new(probe_round()));
        let registry = registry_with(Arc::new(crate::test_support::ProbeTool { probes: 3 }));
        let events = collect(spawn_agent_turn(provider, registry, None, &[], 4));
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, Event::Heartbeat))
                .count(),
            3
        );
        assert_eq!(tool_results(&events)[0].content, "alive 3/3");
        // One TurnStart, one TurnEnd, still.
        assert_eq!(events.first(), Some(&Event::TurnStart));
        assert_eq!(events.last(), Some(&Event::TurnEnd));
    }

    #[test]
    fn a_probe_fails_once_the_receiver_is_dropped() {
        // The tool blocks until the consumer is gone, then probes.
        struct Gate(
            std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
            std::sync::Mutex<std::sync::mpsc::Sender<bool>>,
        );
        impl Tool for Gate {
            fn name(&self) -> &str {
                "probe"
            }
            fn description(&self) -> &str {
                "waits, then probes"
            }
            fn parameters(&self) -> serde_json::Value {
                serde_json::json!({"type": "object", "properties": {}})
            }
            fn execute(&self, _: &str) -> Result<String, String> {
                unreachable!()
            }
            fn execute_with_probe(
                &self,
                _: &str,
                alive: &dyn Fn() -> bool,
            ) -> Result<ToolOutput, String> {
                self.0.lock().unwrap().recv().unwrap();
                self.1.lock().unwrap().send(alive()).unwrap();
                Ok("x".to_string().into())
            }
        }
        let (go_tx, go_rx) = std::sync::mpsc::channel();
        let (seen_tx, seen_rx) = std::sync::mpsc::channel();
        let provider = Arc::new(ScriptedProvider::new(probe_round()));
        let registry = registry_with(Arc::new(Gate(
            std::sync::Mutex::new(go_rx),
            std::sync::Mutex::new(seen_tx),
        )));
        let rx = spawn_agent_turn(provider, registry, None, &[], 4);
        for event in rx.iter() {
            if matches!(event, Event::ToolCall(_)) {
                break;
            }
        }
        drop(rx);
        go_tx.send(()).unwrap();
        assert!(
            !seen_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap(),
            "the probe must report the dropped receiver"
        );
    }

    #[test]
    fn a_provider_heartbeat_is_swallowed_not_forwarded() {
        let provider = Arc::new(ScriptedProvider::new(vec![vec![
            Event::TurnStart,
            Event::Heartbeat,
            Event::Token("hi".into()),
            Event::Heartbeat,
            Event::TurnEnd,
        ]]));
        let events = collect(spawn_agent_turn(
            provider,
            Arc::new(ToolRegistry::new()),
            None,
            &[],
            4,
        ));
        assert_eq!(
            events,
            vec![Event::TurnStart, Event::Token("hi".into()), Event::TurnEnd]
        );
    }
}
