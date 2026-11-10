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
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::ToolGate;
use crate::agent::spawn_agent_turn;
use crate::config::ModelsConfig;
use crate::event::{Event, Usage};
use crate::message::{AssistantBlock, Message};
use crate::openai::OpenAiCompatProvider;
use crate::selection;
use crate::session::Session;
use crate::system_prompt;
use crate::thinking::ThinkingLevel;
use crate::tools::{ImageGate, ToolBundle};

/// Best-effort transcript flush (#116): append whatever the mirror
/// collected to the run's session file. Errors are swallowed — chat
/// never depends on the disk.
fn flush_transcript(transcript: &mut Option<Session>, buf: &mut [Message]) {
    if let Some(session) = transcript.as_mut().filter(|_| !buf.is_empty()) {
        let _ = session.append(buf);
    }
}

/// How often the runner probes the parent's liveness (#170): on every
/// quiet interval, and at least this often under a steady event stream.
const PROBE_INTERVAL: Duration = Duration::from_millis(250);

/// What [`next_event`] found.
#[derive(Debug, PartialEq)]
enum Next {
    Event(Event),
    /// The subagent's loop finished and closed its channel.
    Closed,
    /// The probe failed: the parent's consumer is gone.
    Cancelled,
}

/// Receive the next event, probing `alive` on every quiet `interval`
/// and — so a fast stream cannot starve the check — at least once per
/// `interval` while events keep arriving (#170).
fn next_event(
    rx: &Receiver<Event>,
    interval: Duration,
    last_probe: &mut Instant,
    alive: &dyn Fn() -> bool,
) -> Next {
    loop {
        match rx.recv_timeout(interval) {
            Ok(event) => {
                if last_probe.elapsed() >= interval {
                    *last_probe = Instant::now();
                    if !alive() {
                        return Next::Cancelled;
                    }
                }
                return Next::Event(event);
            }
            Err(RecvTimeoutError::Timeout) => {
                *last_probe = Instant::now();
                if !alive() {
                    return Next::Cancelled;
                }
            }
            Err(RecvTimeoutError::Disconnected) => return Next::Closed,
        }
    }
}

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
    /// Provider rounds that ended in tool calls — parallel calls in one
    /// round count once (#197).
    pub rounds: usize,
    /// The session file this run recorded to (#116), when the
    /// `subagentTranscripts` opt-in is on and the file opened. `None`
    /// when recording is off or the best-effort open failed. Surfaced
    /// so the tool's result line can name the artifact (#125) — in the
    /// field run the parent agent hunted for recordings and concluded
    /// none happen, because nothing said so.
    pub recorded: Option<std::path::PathBuf>,
}

/// The handle the `subagent` tool holds: config to resolve overrides
/// against, the session cwd its tools share, and the *current*
/// provider identity, restamped by the front end on every provider
/// rebuild (`/model`, `/resume` — the telemetry-stamp pattern).
/// Cloneable; every clone sees the same stamped identity.
#[derive(Clone)]
pub struct SubagentRunner {
    /// The models config overrides and entries resolve against. Behind
    /// the lock so `/reload` can restamp it live (#186); a run takes an
    /// `Arc` snapshot when it starts, so a restamp mid-run never
    /// changes that run halfway.
    config: Arc<Mutex<Arc<ModelsConfig>>>,
    cwd: Arc<PathBuf>,
    telemetry: bool,
    /// Consent gate for the subagent's own tool loop. `None` today —
    /// permission UX arrives with the extension host (app.rs's own
    /// gate is `None` for the same reason).
    gate: Option<Arc<ToolGate>>,
    current: Arc<Mutex<Option<(String, String)>>>,
    /// The session-id parent slot, restamped alongside the identity on
    /// session open and `/resume` — the transcript header's lineage
    /// (#116).
    parent: Arc<Mutex<Option<String>>>,
    /// When set (the `settings.subagentTranscripts` opt-in), every run
    /// records its own ordinary session file under this sessions root.
    /// `None` records nothing — the default. Behind the lock so
    /// `/reload` can restamp it live (AD-0054).
    transcripts: Arc<Mutex<Option<PathBuf>>>,
}

impl SubagentRunner {
    pub fn new(
        config: ModelsConfig,
        cwd: PathBuf,
        telemetry: bool,
        transcripts: Option<PathBuf>,
    ) -> Self {
        Self {
            config: Arc::new(Mutex::new(Arc::new(config))),
            cwd: Arc::new(cwd),
            telemetry,
            gate: None,
            current: Arc::new(Mutex::new(None)),
            parent: Arc::new(Mutex::new(None)),
            transcripts: Arc::new(Mutex::new(transcripts)),
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

    /// Record the current parent session id — called wherever the
    /// identity is restamped and a session is open (startup open,
    /// `/resume`). The transcript header's lineage.
    pub fn stamp_session(&self, parent: Option<String>) {
        *self.parent.lock().unwrap() = parent;
    }

    /// Replace the config subagent runs resolve against — `/reload`
    /// restamps it whenever it adopts a re-read `models.json`, so the
    /// runner's config always equals the app's (#186). Every clone sees
    /// it; runs already in flight keep the snapshot they started with.
    pub fn stamp_config(&self, config: ModelsConfig) {
        *self.config.lock().unwrap() = Arc::new(config);
    }

    /// The current config snapshot, for introspection and tests.
    pub fn config(&self) -> Arc<ModelsConfig> {
        Arc::clone(&self.config.lock().unwrap())
    }

    /// Record the transcripts root — `/reload` restamps it when the
    /// re-read `settings.json` speaks about `subagentTranscripts`
    /// (AD-0054).
    pub fn stamp_transcripts(&self, transcripts: Option<PathBuf>) {
        *self.transcripts.lock().unwrap() = transcripts;
    }

    /// The transcripts root, for introspection and tests.
    pub fn transcripts_dir(&self) -> Option<PathBuf> {
        self.transcripts.lock().unwrap().clone()
    }

    fn stamped_parent(&self) -> Option<String> {
        self.parent.lock().unwrap().clone()
    }

    /// Run one subagent to completion with no cancellation probe.
    /// See [`Self::run_probed`].
    pub fn run(&self, spec: &SubagentSpec) -> Result<SubagentResult, String> {
        self.run_probed(spec, &|| true)
    }

    /// Run one subagent to completion. Blocking by ruling: the tool
    /// loop is sequential, and the shaped [`SubagentResult`] is all
    /// the caller ever sees — intermediate events are drained, not
    /// surfaced. An `Event::Error` (network, HTTP, the round cap)
    /// becomes `Err`; the events contract (TurnStart…TurnEnd,
    /// dropped-`Receiver` cancellation) is untouched.
    ///
    /// Cancellation (#170): the parent loop is blocked in this call
    /// and sends nothing, and this runner owns (and drains) the
    /// subagent's `Receiver`, so the subagent's own sends keep
    /// succeeding — it would never notice a cancelled parent. So the
    /// drain probes `alive` (a send of `Event::Heartbeat` on the
    /// parent's channel) every [`PROBE_INTERVAL`]. When it fails, the
    /// completed transcript is flushed, the `Receiver` is dropped
    /// (the subagent's loop stops at its next send — no further
    /// provider round, no further tool call) and `Err` is returned. A
    /// tool already running inside the subagent is abandoned, not
    /// killed, exactly like a parent `bash`.
    pub fn run_probed(
        &self,
        spec: &SubagentSpec,
        alive: &dyn Fn() -> bool,
    ) -> Result<SubagentResult, String> {
        // One snapshot for the whole run: a `/reload` restamp landing
        // mid-run must not change it halfway.
        let config = self.config();
        // Identity: inherit the stamp, or resolve the override. The
        // parent's provider Arc is never reused — the subagent gets a
        // provider wired to *its* registry, not the parent's toolset.
        let (provider_name, model) = match (&spec.provider, &spec.model) {
            (None, None) => self.current().ok_or_else(|| {
                "no provider available for the subagent: the runner was never stamped".to_string()
            })?,
            _ => {
                let selected = selection::resolve(
                    &config,
                    spec.provider.as_deref(),
                    spec.model.as_deref(),
                    None,
                )?;
                (selected.provider, selected.model)
            }
        };
        let entry = config
            .providers
            .get(&provider_name)
            .ok_or_else(|| format!("provider `{provider_name}` is not in the config"))?;
        // Transcript recording (#116, opt-in): an ordinary session file
        // under the parent project, marked kind "subagent" with the
        // stamped parent id. Best-effort like every session write — a
        // full disk never fails the run. The first message is the
        // prompt; the drain below mirrors the loop's own working
        // history event by event.
        let mut transcript = self.transcripts_dir().and_then(|root| {
            Session::start_subagent_in(
                &root,
                &self.cwd,
                &provider_name,
                &model,
                spec.thinking.map(|t| t.name()),
                self.stamped_parent().as_deref(),
            )
            .ok()
        });
        // Materialize NOW or not at all: an open failure (root gone
        // unwritable mid-flight, path occupied) must be visible here —
        // `recorded: None` — not surface later as a silently lost
        // transcript on first append.
        let opened = transcript
            .as_mut()
            .is_none_or(|session| session.materialize().is_ok());
        if !opened {
            transcript = None;
        }
        if let Some(session) = transcript.as_mut() {
            let _ = session.append(&[Message::Harness(spec.prompt.clone())]);
        }
        // Name the artifact on the result (#125): capture the path
        // while the handle exists, before the drain takes ownership.
        let recorded = transcript.as_ref().map(|s| s.path().to_path_buf());
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
        let history = vec![Message::Harness(spec.prompt.clone())];
        let max_rounds = spec.max_tool_rounds.unwrap_or(SUBAGENT_MAX_TOOL_ROUNDS);
        let rx = spawn_agent_turn(
            provider,
            Arc::clone(&registry),
            self.gate.clone(),
            &history,
            max_rounds,
        );
        drain(&rx, alive, transcript, recorded)
    }
}

/// Drain a subagent's event stream into its shaped result. Split out of
/// [`SubagentRunner::run_probed`] so the closed-channel path is testable
/// without a live agent loop (#172).
fn drain(
    rx: &Receiver<Event>,
    alive: &dyn Fn() -> bool,
    mut transcript: Option<Session>,
    recorded: Option<PathBuf>,
) -> Result<SubagentResult, String> {
    let mut blocks: Vec<AssistantBlock> = Vec::new();
    let mut narration = String::new();
    let mut usage = Usage::default();
    let mut rounds = 0usize;
    // Calls of one round arrive back to back; its results close the batch.
    let mut in_call_batch = false;
    let mut failure = None;
    let mut ended = false;
    // The transcript mirror: the same reconstruction rule the loop
    // documents for consumers — accumulate tokens, flush them as an
    // Assistant message at each ToolCall, record ToolResults in
    // order. `load()` of the file reproduces the run's working
    // history exactly.
    let mut tbuf: Vec<Message> = Vec::new();
    let mut last_probe = Instant::now();
    loop {
        let event = match next_event(rx, PROBE_INTERVAL, &mut last_probe, alive) {
            Next::Event(event) => event,
            Next::Closed => break,
            Next::Cancelled => {
                flush_transcript(&mut transcript, &mut tbuf);
                failure = Some("subagent cancelled: the parent turn is gone".to_string());
                break;
            }
        };
        match event {
            Event::Token(t) => AssistantBlock::push_text(&mut blocks, &t),
            Event::Thinking { delta, field } => {
                AssistantBlock::push_thinking(&mut blocks, &delta, field);
            }
            Event::ToolCall(c) => {
                // A round is the batch of calls the model made at once,
                // not each call in it (#197).
                if !in_call_batch {
                    rounds += 1;
                    in_call_batch = true;
                }
                // The reply that matters is the one after the last
                // tool round; keep the pre-tool prose as the run's
                // narration so a cap failure can return it.
                let prose = AssistantBlock::text_of(&blocks);
                if !prose.trim().is_empty() {
                    narration = prose;
                }
                if transcript.is_some() {
                    if !blocks.is_empty() {
                        tbuf.push(Message::Assistant(blocks.clone()));
                    }
                    tbuf.push(Message::ToolCall(c.clone()));
                }
                blocks.clear();
            }
            Event::ToolResult(r) => {
                in_call_batch = false;
                if transcript.is_some() {
                    tbuf.push(Message::ToolResult(r));
                }
            }
            Event::Usage(u) => {
                // Saturating: wire counts are unchecked (#242).
                usage.prompt_tokens = usage.prompt_tokens.saturating_add(u.prompt_tokens);
                usage.completion_tokens =
                    usage.completion_tokens.saturating_add(u.completion_tokens);
                if let Some(session) = transcript.as_mut() {
                    let _ = session.append_usage(&u);
                }
            }
            Event::Error(e) => {
                // Record the rounds that completed; the pre-call
                // prose is already in the buffer as Assistant
                // messages, so nothing is lost to the cap.
                flush_transcript(&mut transcript, &mut tbuf);
                failure = Some(e);
                break; // dropping the receiver stops the loop at its next send
            }
            Event::TurnEnd => {
                ended = true;
                // The settled reply is the buffer's last message.
                if !blocks.is_empty() {
                    tbuf.push(Message::Assistant(blocks.clone()));
                }
                flush_transcript(&mut transcript, &mut tbuf);
            }
            _ => {}
        }
    }
    if failure.is_none() && !ended {
        // Closed with no `TurnEnd` (#172): the subagent's agent thread
        // died. An empty success would hand the parent model a blank
        // answer as if the subagent had finished.
        flush_transcript(&mut transcript, &mut tbuf);
        failure = Some(
            "subagent ended unexpectedly: its event stream closed without finishing".to_string(),
        );
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
            let partial = {
                let prose = AssistantBlock::text_of(&blocks);
                if !prose.trim().is_empty() {
                    prose.trim().to_string()
                } else {
                    narration.trim().to_string()
                }
            };
            if partial.is_empty() {
                msg.push_str(" (no partial output)");
            } else {
                msg.push_str("\n\nPartial output before the cap:\n");
                msg.push_str(&partial);
            }
            return Err(msg);
        }
        return Err(e);
    }
    Ok(SubagentResult {
        text: AssistantBlock::text_of(&blocks),
        usage,
        rounds,
        recorded,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{FakeApi, sse_response};

    #[test]
    fn transcripts_restamp_follows_the_setting() {
        // /reload restamps the transcripts root when the file's
        // subagentTranscripts key speaks (AD-0054).
        let runner =
            SubagentRunner::new(ModelsConfig::default(), std::env::temp_dir(), false, None);
        assert!(runner.transcripts_dir().is_none());
        runner.stamp_transcripts(Some(std::path::PathBuf::from("/tmp/roots")));
        assert_eq!(
            runner.transcripts_dir().as_deref(),
            Some(std::path::Path::new("/tmp/roots"))
        );
        runner.stamp_transcripts(None);
        assert!(runner.transcripts_dir().is_none());
    }

    #[test]
    fn transcripts_record_when_enabled() {
        let dir = std::env::temp_dir().join(format!("hi-sub-t1-{}", std::process::id()));
        let root = dir.join("sessions");
        let _ = std::fs::remove_dir_all(&dir);
        let narrate = sse_response(&[
            "{\"choices\":[{\"delta\":{\"content\":\"checking\"}}]}",
            "{\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"nope\",\"arguments\":\"{}\"}}]}}]}",
            "[DONE]",
        ]);
        let settle = sse_response(&[
            "{\"choices\":[{\"delta\":{\"content\":\"final reply\"}}]}",
            "{\"choices\":[],\"usage\":{\"prompt_tokens\":90,\"completion_tokens\":7}}",
            "[DONE]",
        ]);
        let api = FakeApi::start_sequence(vec![narrate, settle]);
        let runner = runner_with(&api.base_url, &dir, Some(&root));
        runner.stamp("prov", "m");
        runner.stamp_session(Some("parent-9".into()));
        let result = runner
            .run(&SubagentSpec {
                prompt: "dig".into(),
                provider: None,
                model: None,
                thinking: None,
                tools: None,
                max_tool_rounds: None,
            })
            .unwrap();
        assert_eq!(result.text, "final reply");
        assert!(result.recorded.is_some(), "the run names its recording");
        // Exactly one session file, under the parent's project key.
        let mut entries: Vec<_> = std::fs::read_dir(&root).unwrap().collect();
        assert_eq!(entries.len(), 1, "one project dir");
        let project = entries.pop().unwrap().unwrap().path();
        let files: Vec<_> = std::fs::read_dir(&project).unwrap().collect();
        assert_eq!(files.len(), 1, "one transcript file");
        let file_path = files.into_iter().next().unwrap().unwrap().path();
        assert_eq!(result.recorded.as_deref(), Some(file_path.as_path()));
        let loaded = crate::session::load(&file_path).unwrap();
        assert_eq!(loaded.header.kind.as_deref(), Some("subagent"));
        assert_eq!(loaded.header.parent.as_deref(), Some("parent-9"));
        // The transcript mirrors the loop's working history.
        assert_eq!(loaded.messages[0], Message::Harness("dig".into()));
        assert_eq!(loaded.messages[1], Message::assistant("checking"));
        assert!(matches!(loaded.messages[2], Message::ToolCall(_)));
        assert!(matches!(loaded.messages[3], Message::ToolResult(_)));
        assert_eq!(loaded.messages[4], Message::assistant("final reply"));
        assert_eq!(loaded.messages.len(), 5);
        assert_eq!(loaded.usage.prompt_tokens, 90);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn transcript_open_failure_stays_silent() {
        // Best-effort contract: when the transcript file cannot even be
        // created (root is a regular file here), the run must still
        // succeed and the result names no recording.
        let dir = std::env::temp_dir().join(format!("hi-sub-t3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let root = dir.join("sessions");
        std::fs::write(&root, b"not a directory").unwrap();
        let frame = sse_response(&[
            "{\"choices\":[{\"delta\":{\"content\":\"done\"}}]}",
            "[DONE]",
        ]);
        let api = FakeApi::start(frame);
        let runner = runner_with(&api.base_url, &dir, Some(&root));
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
        assert!(result.recorded.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn transcripts_off_record_nothing() {
        let dir = std::env::temp_dir().join(format!("hi-sub-t2-{}", std::process::id()));
        let root = dir.join("sessions");
        let _ = std::fs::remove_dir_all(&dir);
        let frame = sse_response(&[
            "{\"choices\":[{\"delta\":{\"content\":\"done\"}}]}",
            "[DONE]",
        ]);
        let api = FakeApi::start(frame);
        let runner = runner_with(&api.base_url, &dir, None);
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
        assert!(result.recorded.is_none());
        assert!(!root.exists(), "no session dir is created");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn runner_with(
        base_url: &str,
        cwd: &std::path::Path,
        transcripts: Option<&std::path::Path>,
    ) -> SubagentRunner {
        let raw = format!(r#"{{"providers":{{"prov":{{"baseUrl":"{base_url}"}}}}}}"#);
        let config: ModelsConfig = serde_json::from_str(&raw).unwrap();
        SubagentRunner::new(
            config,
            cwd.to_path_buf(),
            false,
            transcripts.map(|p| p.to_path_buf()),
        )
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
        let runner = runner_with(&api.base_url, &dir, None);
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
        let runner = runner_with("http://unused.invalid/v1", &dir, None);
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
        let runner = SubagentRunner::new(config, dir, false, None);
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
    fn stamp_config_swaps_the_config_for_every_clone() {
        // #186: /reload adds a provider; the runner (and clones made
        // before the stamp) must resolve against the new config.
        let dir = std::env::temp_dir().join(format!("hi-sub-stampcfg-{}", std::process::id()));
        let api = FakeApi::start(reply("added reply"));
        let runner = runner_with("http://unused.invalid/v1", &dir, None);
        runner.stamp("prov", "m");
        let earlier_clone = runner.clone();
        let spec = SubagentSpec {
            prompt: "go".into(),
            provider: Some("added".into()),
            model: Some("m".into()),
            thinking: None,
            tools: None,
            max_tool_rounds: None,
        };
        assert!(runner.run(&spec).is_err(), "unknown before the stamp");
        let raw = format!(
            r#"{{"providers":{{"prov":{{"baseUrl":"http://unused.invalid/v1"}},"added":{{"baseUrl":"{}"}}}}}}"#,
            api.base_url
        );
        runner.stamp_config(serde_json::from_str(&raw).unwrap());
        assert!(earlier_clone.config().providers.contains_key("added"));
        assert_eq!(earlier_clone.run(&spec).unwrap().text, "added reply");
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
        let runner = runner_with(&api.base_url, &dir, None);
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
    fn parallel_calls_in_one_round_count_as_one_round() {
        // #197: three calls in one provider round reported rounds=3.
        let dir = std::env::temp_dir().join(format!("hi-sub-rounds-{}", std::process::id()));
        let call = |i: usize| {
            format!(
                "{{\"index\":{i},\"id\":\"call_{i}\",\"function\":{{\"name\":\"ghost\",\"arguments\":\"{{}}\"}}}}"
            )
        };
        let parallel = sse_response(&[
            &format!(
                "{{\"choices\":[{{\"delta\":{{\"tool_calls\":[{},{},{}]}}}}]}}",
                call(0),
                call(1),
                call(2)
            ),
            "[DONE]",
        ]);
        let settle = sse_response(&[
            "{\"choices\":[{\"delta\":{\"content\":\"settled\"}}]}",
            "[DONE]",
        ]);
        let api = FakeApi::start_sequence(vec![parallel, settle]);
        let runner = runner_with(&api.base_url, &dir, None);
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
        assert_eq!(result.text, "settled");
        assert_eq!(result.rounds, 1, "one round of three calls");
    }

    #[test]
    fn rounds_cap_is_a_loud_error() {
        let dir = std::env::temp_dir().join(format!("hi-sub-cap-{}", std::process::id()));
        let looping = sse_response(&[
            "{\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_x\",\"function\":{\"name\":\"nope\",\"arguments\":\"{}\"}}]}}]}",
            "[DONE]",
        ]);
        let api = FakeApi::start_sequence(vec![looping; 4]);
        let runner = runner_with(&api.base_url, &dir, None);
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
        let runner = runner_with(&api.base_url, &dir, None);
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
        let runner = runner_with(&api.base_url, &dir, None);
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
        let runner = runner_with(&api.base_url, &dir, None);
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
        let runner = runner_with(&api.base_url, &dir, None);
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
    fn a_huge_reported_bill_saturates_instead_of_overflowing() {
        // #242: two rounds each reporting u64::MAX sum to u64::MAX.
        let dir = std::env::temp_dir().join(format!("hi-sub-huge-{}", std::process::id()));
        let huge = format!(
            "{{\"choices\":[],\"usage\":{{\"prompt_tokens\":{m},\"completion_tokens\":{m}}}}}",
            m = u64::MAX
        );
        let tool_call = sse_response(&[
            "{\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"nope\",\"arguments\":\"{}\"}}]}}]}",
            &huge,
            "[DONE]",
        ]);
        let settle = sse_response(&[
            "{\"choices\":[{\"delta\":{\"content\":\"done\"}}]}",
            &huge,
            "[DONE]",
        ]);
        let api = FakeApi::start_sequence(vec![tool_call, settle]);
        let runner = runner_with(&api.base_url, &dir, None);
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
        assert_eq!(result.usage.prompt_tokens, u64::MAX);
        assert_eq!(result.usage.completion_tokens, u64::MAX);
    }

    #[test]
    fn event_error_becomes_err() {
        let dir = std::env::temp_dir().join(format!("hi-sub-err-{}", std::process::id()));
        let api = FakeApi::start("this is not sse at all");
        let runner = runner_with(&api.base_url, &dir, None);
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

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc::channel;

    #[test]
    fn a_closed_stream_with_no_turn_end_is_an_error_not_an_empty_success() {
        // #172: the subagent's agent thread died mid-run.
        let (tx, rx) = channel::<Event>();
        tx.send(Event::Token("half".into())).unwrap();
        drop(tx);
        let err = drain(&rx, &|| true, None, None).unwrap_err();
        assert!(err.contains("ended unexpectedly"), "{err}");

        // The healthy shape still succeeds.
        let (tx, rx) = channel::<Event>();
        tx.send(Event::Token("whole".into())).unwrap();
        tx.send(Event::TurnEnd).unwrap();
        drop(tx);
        assert_eq!(drain(&rx, &|| true, None, None).unwrap().text, "whole");
    }

    #[test]
    fn next_event_probes_on_a_quiet_timeout() {
        // #170: no events at all — only the timeout branch can notice.
        let (_tx, rx) = channel::<Event>();
        let probes = AtomicUsize::new(0);
        let mut last = Instant::now();
        let next = next_event(&rx, Duration::from_millis(10), &mut last, &|| {
            probes.fetch_add(1, Ordering::SeqCst) < 2
        });
        assert_eq!(next, Next::Cancelled);
        assert_eq!(probes.load(Ordering::SeqCst), 3, "kept probing while quiet");
    }

    #[test]
    fn next_event_probes_under_a_steady_stream() {
        // #170: events arrive faster than the interval, so the timeout
        // never fires; the elapsed check must still probe.
        let (tx, rx) = channel();
        let sender = std::thread::spawn(move || {
            for _ in 0..200 {
                if tx.send(Event::Token("x".into())).is_err() {
                    return true; // receiver dropped: cancelled in time
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            false
        });
        let mut last = Instant::now();
        let interval = Duration::from_millis(200);
        let mut seen = 0;
        let end = loop {
            match next_event(&rx, interval, &mut last, &|| false) {
                Next::Event(_) => seen += 1,
                other => break other,
            }
        };
        assert_eq!(end, Next::Cancelled);
        assert!(seen < 100, "events flowed, then the probe cut in");
        drop(rx);
        assert!(sender.join().unwrap(), "the stream saw the receiver drop");
    }

    #[test]
    fn next_event_passes_events_and_reports_close_when_alive() {
        let (tx, rx) = channel();
        tx.send(Event::Token("a".into())).unwrap();
        drop(tx);
        let mut last = Instant::now();
        let alive = || true;
        let step = Duration::from_millis(10);
        assert_eq!(
            next_event(&rx, step, &mut last, &alive),
            Next::Event(Event::Token("a".into()))
        );
        assert_eq!(next_event(&rx, step, &mut last, &alive), Next::Closed);
    }

    fn tc(id: &str, name: &str, args: &str) -> String {
        let frag = serde_json::json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"id":id,"function":{"name":name,"arguments":args}}]}}]});
        sse_response(&[&frag.to_string(), "[DONE]"])
    }

    fn spec(prompt: &str) -> SubagentSpec {
        SubagentSpec {
            prompt: prompt.into(),
            provider: None,
            model: None,
            thinking: None,
            tools: None,
            max_tool_rounds: None,
        }
    }

    #[test]
    fn a_cancelled_run_flushes_the_completed_transcript_and_stops() {
        // #170: round 1 completes (`true`); round 2 marks itself and
        // sleeps. The probe fails once the marker exists, i.e. after
        // round 1's events were sent. The completed rounds must reach
        // the transcript file, run returns Err, and no third round runs.
        let dir = std::env::temp_dir().join(format!("hi-sub-cancel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let root = dir.join("sessions");
        let marker = dir.join("marker");
        let api = FakeApi::start_sequence(vec![
            tc("s1", "bash", r#"{"command":"true"}"#),
            tc(
                "s2",
                "bash",
                &format!(r#"{{"command":"touch {}; sleep 1"}}"#, marker.display()),
            ),
            sse_response(&[
                "{\"choices\":[{\"delta\":{\"content\":\"late\"}}]}",
                "[DONE]",
            ]),
        ]);
        let runner = runner_with(&api.base_url, &dir, Some(&root));
        runner.stamp("prov", "m");
        let err = runner
            .run_probed(&spec("dig"), &|| !marker.exists())
            .unwrap_err();
        assert!(err.contains("cancelled"), "{err}");
        // The transcript holds the completed round.
        let project = std::fs::read_dir(&root)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let file = std::fs::read_dir(project)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let loaded = crate::session::load(&file).unwrap();
        assert_eq!(loaded.messages[0], Message::Harness("dig".into()));
        assert!(
            loaded
                .messages
                .iter()
                .any(|m| matches!(m, Message::ToolResult(r) if r.call_id == "s1")),
            "completed round flushed: {:?}",
            loaded.messages
        );
        // Let the abandoned bash finish plus a margin: no third round.
        std::thread::sleep(Duration::from_millis(1500));
        assert_eq!(api.request_count(), 2, "no provider round after cancel");
    }

    #[test]
    fn an_uncancelled_probed_run_matches_a_plain_run() {
        let reply = |text: &str| {
            sse_response(&[
                &format!("{{\"choices\":[{{\"delta\":{{\"content\":\"{text}\"}}}}]}}"),
                "{\"choices\":[],\"usage\":{\"prompt_tokens\":9,\"completion_tokens\":2}}",
                "[DONE]",
            ])
        };
        let run = |probed: bool| {
            let api = FakeApi::start_sequence(vec![tc("c", "nope", "{}"), reply("fine")]);
            let runner = runner_with(&api.base_url, &std::env::temp_dir(), None);
            runner.stamp("prov", "m");
            let result = if probed {
                runner.run_probed(&spec("x"), &|| true)
            } else {
                runner.run(&spec("x"))
            };
            result.unwrap()
        };
        assert_eq!(run(true), run(false));
    }
}
