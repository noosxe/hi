//! The one wire protocol hi speaks: an OpenAI-compatible endpoint.
//!
//! Chat turns are `POST {baseUrl}/chat/completions` with
//! `"stream": true`, read back as server-sent events. Model listing is
//! the hybrid policy: explicit `models` entries from `models.json` win
//! (only they can carry what discovery cannot know — context window,
//! reasoning flag); an empty list means *ask the server* via
//! `GET {baseUrl}/models`.
//!
//! Everything here is blocking I/O (`ureq`), by design: the turn runs
//! on its own thread, so a blocking HTTP client is the simplest tool
//! that keeps the UI responsive — no async runtime anywhere.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::{ModelConfig, ProviderConfig};
use crate::event::{Event, Usage};
use crate::message::Message;
use crate::provider::Provider;
use crate::thinking::ThinkingLevel;
use crate::tool::{ToolCall, ToolRegistry, ToolResult};

/// A provider backed by any OpenAI-compatible HTTP endpoint.
#[derive(Clone)]
pub struct OpenAiCompatProvider {
    /// Root of the API *including* the version path, e.g.
    /// `http://localhost:8080/v1`.
    base_url: String,
    /// Resolved bearer token; `None` sends no `Authorization` header.
    api_key: Option<String>,
    /// The model identifier sent with every chat request.
    model: String,
    /// Explicit `models` entries; non-empty means config wins over
    /// discovery (see `list_models`).
    configured_models: Vec<ModelConfig>,
    /// Shared HTTP agent: connection pooling plus the timeout policy,
    /// defined once in `bounded_agent`.
    agent: ureq::Agent,
    /// The resolved system prompt, sent as the leading system
    /// message. `None` sends none and the model's defaults rule.
    system_prompt: Option<String>,
    /// The session's thinking level; kwargs merge only when the
    /// current model's `thinkingLevelMap` has an entry for it.
    thinking_level: Option<ThinkingLevel>,
    /// Tools the model may call. An empty registry omits the request's
    /// `tools` field entirely (see `ChatRequest`).
    tools: Arc<ToolRegistry>,
    /// The user's attribution opt-in (the `telemetry` setting). Gates
    /// the extra attribution headers only — the honest `User-Agent`
    /// rides on every request regardless.
    telemetry: bool,
    /// Prompt-cache routing key: the open session's id. Turns sharing
    /// a key and a stable prefix route to the same server-side cache.
    /// `None` (no session) sends nothing.
    cache_key: Option<String>,
}

impl std::fmt::Debug for OpenAiCompatProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Hand-written so the dump can summarize the agent and, more
        // importantly, never print the credential's value.
        f.debug_struct("OpenAiCompatProvider")
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "[set]"))
            .field("model", &self.model)
            .field("configured_models", &self.configured_models.len())
            .field(
                "system_prompt",
                &self.system_prompt.as_ref().map(|_| "[set]"),
            )
            .field("thinking_level", &self.thinking_level.map(|l| l.name()))
            .field("tools", &self.tools.len())
            .finish()
    }
}

impl OpenAiCompatProvider {
    /// Build from raw parts. Intended for tests; real callers go
    /// through [`from_config`].
    pub fn new(
        base_url: impl Into<String>,
        api_key: Option<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            api_key,
            model: model.into(),
            configured_models: Vec::new(),
            agent: bounded_agent(),
            system_prompt: None,
            thinking_level: None,
            tools: Arc::new(ToolRegistry::new()),
            telemetry: false,
            cache_key: None,
        }
    }

    /// Build from a parsed `models.json` provider entry.
    ///
    /// The `$ENV` interpolation of the API key happens here, *once*:
    /// the environment at launch time is what the session uses —
    /// changing variables mid-session does nothing, which keeps
    /// behavior predictable.
    pub fn from_config(cfg: &ProviderConfig, model: impl Into<String>) -> Self {
        Self {
            base_url: cfg.base_url.clone(),
            api_key: cfg.resolved_api_key().filter(|key| !key.is_empty()),
            model: model.into(),
            configured_models: cfg.models.clone(),
            agent: bounded_agent(),
            system_prompt: None,
            thinking_level: None,
            tools: Arc::new(ToolRegistry::new()),
            telemetry: false,
            cache_key: None,
        }
    }

    /// Attach the resolved system prompt. `None` sends no system
    /// message and the model's own defaults rule.
    pub fn with_system_prompt(mut self, system_prompt: Option<String>) -> Self {
        self.system_prompt = system_prompt;
        self
    }

    /// Attach the session's thinking level.
    pub fn with_thinking_level(mut self, thinking_level: Option<ThinkingLevel>) -> Self {
        self.thinking_level = thinking_level;
        self
    }

    /// Attach the tool registry the model may call from. An empty
    /// registry (the default) sends no `tools` field — identical wire
    /// behavior to not calling this.
    pub fn with_tools(mut self, tools: Arc<ToolRegistry>) -> Self {
        self.tools = tools;
        self
    }

    /// Stamp the attribution opt-in (the `telemetry` setting). Only the
    /// extra attribution headers are gated — the `User-Agent` rides
    /// always. Defaults to off, so the honest identity never carries
    /// anything the user did not ask for.
    pub fn with_telemetry(mut self, telemetry: bool) -> Self {
        self.telemetry = telemetry;
        self
    }

    /// Attach the prompt-cache routing key — the open session's id.
    /// `None` (no session) sends no key and no affinity header.
    pub fn with_cache_key(mut self, cache_key: Option<String>) -> Self {
        self.cache_key = cache_key;
        self
    }
    /// The current model's `thinkingLevelMap`, if it has one.
    fn thinking_level_map(&self) -> Option<&std::collections::BTreeMap<String, serde_json::Value>> {
        self.configured_models
            .iter()
            .find(|model| model.id == self.model)
            .and_then(|model| model.thinking_level_map.as_ref())
    }

    /// Model ids this provider offers — the hybrid policy:
    /// configured entries when present, otherwise discovered live
    /// from `{baseUrl}/models`. Sorted and deduplicated so callers
    /// get a stable order.
    pub fn list_models(&self) -> Result<Vec<String>, String> {
        if !self.configured_models.is_empty() {
            return Ok(self
                .configured_models
                .iter()
                .map(|model| model.id.clone())
                .collect());
        }
        discover_model_ids(&self.base_url, self.api_key.as_deref())
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}/{}", self.base_url.trim_end_matches('/'), path)
    }

    /// Headers identifying hi on the wire. The `User-Agent` always
    /// rides — honest product/version/platform, never gated. The
    /// attribution pair only when telemetry is opted in *and* the
    /// endpoint displays it (OpenRouter); the session-affinity header
    /// only when a session is open. Both extras target OpenRouter
    /// only, matched by host the way pi matches attribution hosts.
    fn wire_headers(&self) -> Vec<(&'static str, String)> {
        let openrouter = is_openrouter(&self.base_url);
        let mut headers = vec![("User-Agent", user_agent())];
        if self.telemetry && openrouter {
            headers.push(("HTTP-Referer", "https://github.com/noosxe/hi".into()));
            headers.push(("X-Title", "hi".into()));
        }
        if openrouter && let Some(key) = &self.cache_key {
            headers.push(("x-session-id", key.clone()));
        }
        headers
    }

    /// Attach the bearer token when one is configured — and only
    /// then. Some local servers reject requests that carry an
    /// unexpected `Authorization` header. Generic over ureq's
    /// typestate (`WithBody`/`WithoutBody`), so both call sites share it.
    fn authorized<State>(
        &self,
        request: ureq::RequestBuilder<State>,
    ) -> ureq::RequestBuilder<State> {
        match &self.api_key {
            Some(key) => request.header("Authorization", format!("Bearer {key}")),
            None => request,
        }
    }

    /// Stream one turn: the blocking HTTP conversation behind
    /// `Provider::respond`.
    fn stream(&self, history: &[Message], tx: &Sender<Event>) -> Result<(), String> {
        let mut messages = Vec::new();
        if let Some(system) = &self.system_prompt {
            messages.push(WireMessage::text("system", system));
        }
        messages.extend(wire_messages(history));
        let chat_request = ChatRequest {
            model: &self.model,
            messages,
            stream: true,
            stream_options: StreamOptions {
                include_usage: true,
            },
            tools: self.tools.definitions(),
            prompt_cache_key: self.cache_key.as_deref(),
        };
        let mut payload = serde_json::to_value(&chat_request)
            .map_err(|err| format!("cannot serialize request: {err}"))?;
        // Merge the active thinking level's kwargs (from the model's
        // `thinkingLevelMap`) into the top-level body. Models without
        // a map, or levels without an entry, send nothing.
        if let Some(level) = &self.thinking_level
            && let Some(kwargs) = self
                .thinking_level_map()
                .and_then(|map| map.get(level.name()))
            && let (Some(body), Some(kwargs)) = (payload.as_object_mut(), kwargs.as_object())
        {
            for (key, value) in kwargs {
                body.insert(key.clone(), value.clone());
            }
        }
        let url = self.endpoint("chat/completions");
        let mut request = self.authorized(self.agent.post(&url));
        for (name, value) in self.wire_headers() {
            request = request.header(name, value);
        }
        let response = request
            .send_json(&payload)
            .map_err(|err| describe("chat request", err))?;

        // The body reader is a plain `Read`; buffering turns it into
        // lines, which is exactly what SSE frames are.
        let buffered = BufReader::new(response.into_body().into_reader());
        // Tool-call fragments stream in pieces and accumulate here,
        // surfacing as `Event::ToolCall`s at stream end — after the
        // text tokens, mirroring the protocol's own ordering (a
        // message's content precedes its tool_calls).
        let mut partials: BTreeMap<usize, PartialToolCall> = BTreeMap::new();
        // The round's bill, kept aside until finalization — the wire
        // sends usage once, after all content; deferring keeps a
        // chatty server's per-chunk usage from double counting.
        let mut round_usage: Option<Usage> = None;
        for line in buffered.lines() {
            let line = line.map_err(|err| format!("reading stream: {err}"))?;
            for datum in parse_data_line(&line) {
                match datum {
                    DataLine::Token(token) => {
                        if tx.send(Event::Token(token)).is_err() {
                            // Receiver dropped mid-stream: that is the
                            // cancellation signal. Stop quietly — nobody
                            // is listening for a final event.
                            return Ok(());
                        }
                    }
                    DataLine::ToolFragments(fragments) => {
                        for fragment in fragments {
                            accumulate_fragment(&mut partials, fragment);
                        }
                    }
                    DataLine::Usage(usage) => {
                        // Last-wins: a server that reports usage on
                        // every chunk gets only its final word counted.
                        round_usage = Some(usage);
                    }
                    DataLine::Done => {
                        emit_tool_calls(partials, tx);
                        emit_usage(round_usage, tx);
                        return Ok(());
                    }
                    DataLine::Malformed(why) => return Err(why),
                }
            }
        }
        // Server closed without `[DONE]`: tolerate it and treat the
        // stream as complete — same finalization as `[DONE]`.
        emit_tool_calls(partials, tx);
        emit_usage(round_usage, tx);
        Ok(())
    }
}

/// One agent per provider: connection pooling, plus the timeout
/// policy in exactly one place. Connect and DNS are *bounded* — an
/// unreachable server must not wedge a turn thread for minutes (Esc
/// frees the UI, not a blocked `connect()`; cooperative cancellation
/// only works if the worker reaches a checkpoint). The stream itself
/// stays unbounded on purpose — turns think for minutes — and rides
/// the dropped receiver for cancellation.
fn bounded_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_resolve(Some(Duration::from_secs(10)))
        .build()
        .new_agent()
}

/// `GET {baseUrl}/models` — the discovery half of the hybrid policy.
/// A free function because selection resolves providers *before* any
/// `OpenAiCompatProvider` exists. Overall 10s timeout: discovery runs
/// at startup/listing time, where hanging forever would be worse than
/// failing. (Turn streams get no timeout — Esc cancels those.)
/// The honest wire identity: product, version, platform. Providers
/// see `hi`, not a bare HTTP library.
fn user_agent() -> String {
    format!(
        "hi/{} ({}; {})",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

/// Hostname equality against OpenRouter: scheme stripped, everything
/// from the first path segment or port off. `openrouter.ai.evil.example`
/// does not match.
fn is_openrouter(base_url: &str) -> bool {
    let rest = base_url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(base_url);
    rest.split(['/', ':'])
        .next()
        .is_some_and(|host| host.eq_ignore_ascii_case("openrouter.ai"))
}

pub(crate) fn discover_model_ids(
    base_url: &str,
    api_key: Option<&str>,
) -> Result<Vec<String>, String> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let request = bounded_agent()
        .get(&url)
        .config()
        .timeout_global(Some(Duration::from_secs(10)))
        .build()
        .header("User-Agent", user_agent());
    let request = match api_key {
        Some(key) => request.header("Authorization", format!("Bearer {key}")),
        None => request,
    };
    let response = request
        .call()
        .map_err(|err| describe("model discovery", err))?;
    let value: serde_json::Value = response
        .into_body()
        .read_json()
        .map_err(|err| format!("model discovery: bad response body: {err}"))?;
    let mut ids = extract_model_ids(&value);
    if ids.is_empty() {
        // A 200 with no recognizable list is how a half-compatible
        // server says hello. Never pretend that means "no models" —
        // send the user to the escape hatch instead.
        return Err(
            "model discovery: the server responded, but with no recognizable \
             model list (looked for data[].id and models[].slug). List the \
             models explicitly in models.json instead."
                .to_string(),
        );
    }
    ids.sort();
    ids.dedup();
    Ok(ids)
}

impl Provider for OpenAiCompatProvider {
    fn respond(&self, history: &[Message], tx: Sender<Event>) {
        if tx.send(Event::TurnStart).is_err() {
            return;
        }
        match self.stream(history, &tx) {
            Ok(()) => {
                let _ = tx.send(Event::TurnEnd);
            }
            Err(err) => {
                // Contract (see `Event`): any partial `Token`s stand,
                // then one `Error`, then `TurnEnd` — consumers keep a
                // single finalization path.
                let _ = tx.send(Event::Error(err));
                let _ = tx.send(Event::TurnEnd);
            }
        }
    }
}

/// The round's bill closes after its work: content, then tools, then
/// usage, then `[DONE]` — the wire's own ordering, kept.
fn emit_usage(usage: Option<Usage>, tx: &Sender<Event>) {
    if let Some(usage) = usage {
        let _ = tx.send(Event::Usage(usage));
    }
}

/// Turn a provider error into something a human can act on. Status
/// codes are called out because "HTTP 401" is usually the whole
/// diagnosis (bad or missing key).
fn describe(action: &str, err: ureq::Error) -> String {
    match err {
        ureq::Error::StatusCode(code) => format!("{action}: server returned HTTP {code}"),
        err => format!("{action}: {err}"),
    }
}

/// The outcome of one SSE line. Frames are `data: <payload>`;
/// comments (`: ping`), event names, and blank lines are noise. One
/// chunk can carry a text delta, tool-call deltas, or both — the Vec
/// keeps that defensive parsing free of special cases.
#[derive(Debug, PartialEq)]
enum DataLine {
    Token(String),
    ToolFragments(Vec<ToolCallDelta>),
    Usage(Usage),
    Done,
    Malformed(String),
}

fn parse_data_line(line: &str) -> Vec<DataLine> {
    let Some(payload) = line.strip_prefix("data:") else {
        return Vec::new();
    };
    let payload = payload.trim();
    if payload == "[DONE]" {
        return vec![DataLine::Done];
    }
    let chunk: StreamChunk = match serde_json::from_str(payload) {
        Ok(chunk) => chunk,
        Err(err) => {
            return vec![DataLine::Malformed(format!(
                "malformed stream payload: {err}"
            ))];
        }
    };
    let mut out = Vec::new();
    // The usage chunk usually rides an empty-choices frame — parse it
    // before the choices bail-out, never inside it.
    if let Some(raw) = chunk.usage {
        out.push(DataLine::Usage(raw.into()));
    }
    // First choice only. Role-only and empty deltas happen (openers,
    // keep-alives) and are not tokens.
    let Some(choice) = chunk.choices.into_iter().next() else {
        return out;
    };
    if let Some(content) = choice.delta.content
        && !content.is_empty()
    {
        out.push(DataLine::Token(content));
    }
    if !choice.delta.tool_calls.is_empty() {
        out.push(DataLine::ToolFragments(choice.delta.tool_calls));
    }
    out
}

/// One OpenAI chat request. Borrowed strings (`&'a str`,
/// `Cow<'a, str>`) instead of clones — the payload is built, sent, and
/// dropped within one call.
#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<WireMessage<'a>>,
    stream: bool,
    /// Tool definitions offered to the model. Empty ⇒ the field is
    /// omitted entirely: servers vary on whether they accept an empty
    /// array, and a session without tools should look exactly like a
    /// session without tool support.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<serde_json::Value>,
    /// Prompt-cache routing key — the session id. Omitted without a
    /// session, so `--no-session` runs look exactly like before.
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<&'a str>,
    /// Ask for the stream's final usage chunk (AD-0026). Part of the
    /// OpenAI-compat contract; a server that rejects it is a
    /// per-provider bug to fix, not a knob.
    stream_options: StreamOptions,
}

/// The `stream_options` request field — one switch, sent every time.
#[derive(Serialize)]
struct StreamOptions {
    include_usage: bool,
}

/// One message on the wire. The optional fields make one struct cover
/// every role hi sends — `skip_serializing_if` keeps each role's JSON
/// exactly the shape its protocol role prescribes.
#[derive(Serialize)]
struct WireMessage<'a> {
    role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<WireContent<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<WireToolCall<'a>>>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "tool_call_id")]
    tool_call_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
}

/// A message's `content` in either legal shape: a plain string (every
/// text turn) or an array of typed parts (the only way to carry an
/// image). `untagged` serializes the inner value directly, which is
/// exactly the wire's "string or array" contract.
#[derive(Serialize)]
#[serde(untagged)]
enum WireContent<'a> {
    Text(Cow<'a, str>),
    Parts(Vec<WirePart<'a>>),
}

/// One content part. Tagged by `type`, snake-cased: `{"type":"text",
/// "text":…}` and `{"type":"image_url","image_url":{"url":"data:…"}}`.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WirePart<'a> {
    Text { text: Cow<'a, str> },
    ImageUrl { image_url: WireImageUrl<'a> },
}

#[derive(Serialize)]
struct WireImageUrl<'a> {
    url: Cow<'a, str>,
}

impl<'a> WireMessage<'a> {
    fn text(role: &'static str, content: &'a str) -> Self {
        Self {
            role,
            content: Some(WireContent::Text(Cow::Borrowed(content))),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }

    fn tool_result(result: &'a ToolResult) -> Self {
        // A result that is only images has no text to keep — pi's
        // placeholder tells the model where to look (the next user
        // message).
        let text: Cow<'a, str> = if result.content.is_empty() && !result.images.is_empty() {
            Cow::Borrowed("(see attached image)")
        } else {
            Cow::Borrowed(result.content.as_str())
        };
        Self {
            role: "tool",
            content: Some(WireContent::Text(text)),
            tool_calls: None,
            tool_call_id: Some(&result.call_id),
            name: Some(&result.name),
        }
    }

    /// The synthetic user message that carries a tool result's images.
    /// The OpenAI-compat `role: "tool"` message is text-only, so —
    /// exactly as pi-ai does — the tool message keeps its note and the
    /// images ride a follow-up user turn. Wire-only: history keeps the
    /// images on the result, and every turn re-derives this shape.
    fn image_carrier(result: &'a ToolResult) -> Self {
        let mut parts = Vec::with_capacity(result.images.len() + 1);
        parts.push(WirePart::Text {
            text: Cow::Owned(format!("Attached image(s) from {}:", result.name)),
        });
        parts.extend(result.images.iter().map(|image| WirePart::ImageUrl {
            image_url: WireImageUrl {
                url: Cow::Owned(format!("data:{};base64,{}", image.mime_type, image.data)),
            },
        }));
        Self {
            role: "user",
            content: Some(WireContent::Parts(parts)),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }
}

/// One entry of an assistant message's `tool_calls` array.
#[derive(Serialize)]
struct WireToolCall<'a> {
    id: &'a str,
    #[serde(rename = "type")]
    kind: &'static str,
    function: WireFunction<'a>,
}

#[derive(Serialize)]
struct WireFunction<'a> {
    name: &'a str,
    arguments: &'a str,
}

/// An assistant wire message under construction. Consecutive
/// `Message::Assistant` text and `Message::ToolCall` entries merge into
/// ONE assistant message, because that is the OpenAI shape — a
/// message's text and its tool_calls live together. Replaying them as
/// separate assistant turns would read as phantom messages.
struct AssistantGroup<'a> {
    text: Option<Cow<'a, str>>,
    calls: Vec<WireToolCall<'a>>,
}

impl<'a> AssistantGroup<'a> {
    fn flush(&mut self, out: &mut Vec<WireMessage<'a>>) {
        if self.text.is_none() && self.calls.is_empty() {
            return;
        }
        // `take` the pieces so the group can keep being reused within
        // one `wire_messages` scan.
        let text = self.text.take();
        let calls = std::mem::take(&mut self.calls);
        out.push(WireMessage {
            role: "assistant",
            content: text.map(WireContent::Text),
            tool_calls: if calls.is_empty() { None } else { Some(calls) },
            tool_call_id: None,
            name: None,
        });
    }
}

/// Map the harness's history onto the wire.
///
/// Grouping rules, chosen to round-trip exactly what the agent loop
/// built:
/// - `Message::Error` and `Message::System` entries are local
///   annotations — replaying them as conversation turns would be
///   meaningless, so they are dropped.
/// - `Assistant` text and `ToolCall`s merge into one assistant message
///   (see `AssistantGroup`).
/// - Each `ToolResult` follows as its own `role: "tool"` message,
///   pointing at its call via `tool_call_id`.
fn wire_messages(history: &[Message]) -> Vec<WireMessage<'_>> {
    let mut out: Vec<WireMessage<'_>> = Vec::new();
    let mut group = AssistantGroup {
        text: None,
        calls: Vec::new(),
    };
    for message in history {
        match message {
            Message::User(text) => {
                group.flush(&mut out);
                out.push(WireMessage::text("user", text));
            }
            Message::Error(_) | Message::System(_) => {}
            Message::Assistant(text) => match &mut group.text {
                // Merge consecutive assistant texts (rare, but history
                // is caller-built — never lose a word).
                Some(existing) => {
                    let merged = format!("{existing}{text}");
                    *existing = Cow::Owned(merged);
                }
                None => group.text = Some(Cow::Borrowed(text)),
            },
            Message::ToolCall(call) => {
                group.calls.push(WireToolCall {
                    id: &call.id,
                    kind: "function",
                    function: WireFunction {
                        name: &call.name,
                        arguments: &call.arguments,
                    },
                });
            }
            Message::ToolResult(result) => {
                // The assistant message carrying the calls must precede
                // its results on the wire.
                group.flush(&mut out);
                out.push(WireMessage::tool_result(result));
                if !result.images.is_empty() {
                    out.push(WireMessage::image_carrier(result));
                }
            }
        }
    }
    group.flush(&mut out);
    out
}

/// One streamed chunk. `choices[0]` is all hi reads of a content
/// chunk; ids and finish reasons stay ignored by serde. The final
/// chunk's `usage` object is the round's bill (AD-0026).
#[derive(Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    #[serde(default)]
    usage: Option<RawUsage>,
}

/// The wire's `usage` object. Defaults everywhere: a half-compatible
/// server reporting a partial bill parses instead of breaking the
/// stream, and absent details stay absent (`None` is not zero).
#[derive(Deserialize)]
struct RawUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokenDetails>,
    #[serde(default)]
    completion_tokens_details: Option<CompletionTokenDetails>,
}

#[derive(Deserialize)]
struct PromptTokenDetails {
    #[serde(default)]
    cached_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct CompletionTokenDetails {
    #[serde(default)]
    reasoning_tokens: Option<u64>,
}

impl From<RawUsage> for Usage {
    fn from(raw: RawUsage) -> Self {
        Usage {
            prompt_tokens: raw.prompt_tokens,
            completion_tokens: raw.completion_tokens,
            cached_tokens: raw.prompt_tokens_details.and_then(|d| d.cached_tokens),
            reasoning_tokens: raw
                .completion_tokens_details
                .and_then(|d| d.reasoning_tokens),
        }
    }
}

#[derive(Deserialize)]
struct StreamChoice {
    #[serde(default)]
    delta: Delta,
}

#[derive(Deserialize, Default)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
    /// OpenAI streams a tool call in index-keyed fragments: the first
    /// chunk carries `id` and `function.name`, later chunks append
    /// `function.arguments` pieces — never the whole call at once.
    #[serde(default)]
    tool_calls: Vec<ToolCallDelta>,
}

#[derive(Deserialize, Default, Debug, PartialEq)]
struct ToolCallDelta {
    #[serde(default)]
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<ToolCallFunctionDelta>,
}

#[derive(Deserialize, Default, Debug, PartialEq)]
struct ToolCallFunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

/// Accumulator for one in-flight streamed tool call.
#[derive(Default)]
struct PartialToolCall {
    id: Option<String>,
    name: String,
    arguments: String,
}

/// Fold one streamed fragment into the accumulator map, keyed by the
/// fragment's `index` — the protocol's own ordering key for parallel
/// calls streaming interleaved.
fn accumulate_fragment(partials: &mut BTreeMap<usize, PartialToolCall>, delta: ToolCallDelta) {
    let entry = partials.entry(delta.index).or_default();
    if entry.id.is_none() {
        entry.id = delta.id;
    }
    if let Some(function) = delta.function {
        if let Some(name) = function.name {
            entry.name.push_str(&name);
        }
        if let Some(arguments) = function.arguments {
            entry.arguments.push_str(&arguments);
        }
    }
}

/// Turn accumulated fragments into finished calls, in index order. A
/// call whose server never sent an id gets `call_{index}` — every
/// result still needs something to point back at.
fn finalize_tool_calls(partials: BTreeMap<usize, PartialToolCall>) -> Vec<ToolCall> {
    partials
        .into_iter()
        .map(|(index, partial)| ToolCall {
            id: partial.id.unwrap_or_else(|| format!("call_{index}")),
            name: partial.name,
            arguments: partial.arguments,
        })
        .collect()
}

/// Surface accumulated calls as `Event::ToolCall`s, in index order —
/// after the assistant message's text tokens, mirroring the protocol's
/// own field order.
fn emit_tool_calls(partials: BTreeMap<usize, PartialToolCall>, tx: &Sender<Event>) {
    for call in finalize_tool_calls(partials) {
        // A failed send is the dropped-receiver cancellation, not an
        // error to report.
        if tx.send(Event::ToolCall(call)).is_err() {
            return;
        }
    }
}

/// Pull model ids out of a `/models` response. The OpenAI shape is
/// `{"data":[{"id":…}]}`; some providers (z.ai's `/api/v1` catalog,
/// for one) instead serve `{"models":[{"slug":…}]}` — accepted too,
/// because discovery answers "what does this server serve", not
/// "whose protocol doc is canonical".
fn extract_model_ids(value: &serde_json::Value) -> Vec<String> {
    let Some(map) = value.as_object() else {
        return Vec::new();
    };
    let items = map
        .get("data")
        .or_else(|| map.get("models"))
        .and_then(|items| items.as_array());
    let Some(items) = items else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let entry = item.as_object()?;
            let id = entry.get("id").or_else(|| entry.get("slug"))?;
            id.as_str().map(str::to_string)
        })
        .filter(|id| !id.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::spawn_turn;
    use crate::test_support::{EchoTool, FakeApi, json_response, sse_response};
    use crate::{Event, spawn_agent_turn};
    use std::collections::BTreeMap;
    use std::sync::Arc;

    #[test]
    fn parses_sse_data_lines() {
        assert_eq!(
            parse_data_line("data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}"),
            vec![DataLine::Token("Hi".into())]
        );
        assert_eq!(parse_data_line("data: [DONE]"), vec![DataLine::Done]);
        // Role-only opener chunk: no content, not a token.
        assert_eq!(
            parse_data_line("data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}"),
            Vec::new()
        );
        // SSE comments and stray lines are noise.
        assert_eq!(parse_data_line(": keep-alive"), Vec::new());
        assert_eq!(parse_data_line(""), Vec::new());
        // Garbage payload must not be silently dropped.
        assert!(matches!(
            parse_data_line("data: {not json").as_slice(),
            [DataLine::Malformed(_)]
        ));
    }

    #[test]
    fn usage_chunks_parse_with_or_without_details() {
        assert_eq!(
            parse_data_line(
                "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,                 \"completion_tokens\":5,\"prompt_tokens_details\":{\"cached_tokens\":8},                 \"completion_tokens_details\":{\"reasoning_tokens\":3}}}"
            ),
            vec![DataLine::Usage(Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                cached_tokens: Some(8),
                reasoning_tokens: Some(3),
            })]
        );
        // A half-compatible server's bare bill still parses — absent
        // details stay absent, not zero.
        assert_eq!(
            parse_data_line("data: {\"usage\":{\"prompt_tokens\":10}}"),
            vec![DataLine::Usage(Usage {
                prompt_tokens: 10,
                completion_tokens: 0,
                cached_tokens: None,
                reasoning_tokens: None,
            })]
        );
    }

    #[test]
    fn the_usage_chunk_closes_the_round_after_the_tools() {
        let api = FakeApi::start(sse_response(&[
            "{\"choices\":[{\"delta\":{\"content\":\"checking\"}}]}",
            "{\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"go\",\"arguments\":\"{}\"}}]}}]}",
            "{\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}",
            "[DONE]",
        ]));
        let provider = OpenAiCompatProvider::new(api.base_url.clone(), None, "m");
        let events: Vec<Event> = spawn_turn(Arc::new(provider), &[Message::User("go".into())])
            .iter()
            .collect();

        // Wire order, kept: text, the calls, then the bill.
        assert_eq!(events.first(), Some(&Event::TurnStart));
        assert_eq!(events.last(), Some(&Event::TurnEnd));
        let bills: Vec<usize> = events
            .iter()
            .enumerate()
            .filter_map(|(i, e)| matches!(e, Event::Usage(_)).then_some(i))
            .collect();
        assert_eq!(bills.len(), 1, "one round, one bill: {events:?}");
        let bill = bills[0];
        assert!(
            events[..bill]
                .iter()
                .any(|e| matches!(e, Event::ToolCall(_))),
            "tools precede the bill"
        );
        assert_eq!(
            events[bill + 1],
            Event::TurnEnd,
            "the bill is the round's last fact"
        );
    }

    #[test]
    fn error_messages_are_never_sent_to_the_model() {
        let history = vec![
            Message::User("question".into()),
            Message::Error("local annotation".into()),
            Message::Assistant("answer".into()),
        ];
        let wire = wire_messages(&history);
        assert_eq!(wire.len(), 2);
        assert_eq!((wire[0].role, wire[1].role), ("user", "assistant"));
    }

    #[test]
    fn configured_models_win_without_touching_the_network() {
        // Port 1 on localhost is not listening — any HTTP attempt
        // would fail this test loudly.
        let cfg = ProviderConfig {
            base_url: "http://127.0.0.1:1/v1".into(),
            api_key: None,
            models: vec![ModelConfig {
                id: "listed".into(),
                ..Default::default()
            }],
        };
        let provider = OpenAiCompatProvider::from_config(&cfg, "listed");
        assert_eq!(provider.list_models().unwrap(), vec!["listed".to_string()]);
    }

    #[test]
    fn discovers_models_when_none_are_configured() {
        let api = FakeApi::start(json_response(
            "{\"data\":[{\"id\":\"beta\"},{\"id\":\"alpha\"},{\"id\":\"beta\"}]}",
        ));
        let provider = OpenAiCompatProvider::new(api.base_url.clone(), None, "whatever");
        // Sorted, deduplicated.
        assert_eq!(
            provider.list_models().unwrap(),
            vec!["alpha".to_string(), "beta".to_string()]
        );
        let request = api.first_request();
        assert!(request.starts_with("GET /v1/models"));
        // No key configured — no Authorization header may be sent.
        assert!(!request.to_ascii_lowercase().contains("authorization:"));
        // Discovery identifies hi too — not the bare HTTP library.
        assert!(
            request
                .to_ascii_lowercase()
                .contains(&format!("user-agent: hi/{}", env!("CARGO_PKG_VERSION")))
        );
    }

    #[test]
    fn streams_tokens_from_chat_completions() {
        let api = FakeApi::start(sse_response(&[
            "{\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}",
            "{\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}",
            "{\"choices\":[{\"delta\":{\"content\":\"lo!\"}}]}",
            "[DONE]",
        ]));
        let cfg = ProviderConfig {
            base_url: api.base_url.clone(),
            api_key: Some("sekrit".into()),
            models: vec![ModelConfig {
                id: "test-model".into(),
                thinking_level_map: Some(
                    [(
                        "high".to_string(),
                        serde_json::json!({"thinking": {"type": "enabled"}}),
                    )]
                    .into_iter()
                    .collect(),
                ),
                ..Default::default()
            }],
        };
        let provider = OpenAiCompatProvider::from_config(&cfg, "test-model")
            .with_system_prompt(Some("You are terse.".into()))
            .with_thinking_level(Some(ThinkingLevel::High));
        let rx = spawn_turn(Arc::new(provider), &[Message::User("hi there".into())]);
        let events: Vec<Event> = rx.iter().collect();

        assert_eq!(events.first(), Some(&Event::TurnStart));
        assert_eq!(events.last(), Some(&Event::TurnEnd));
        assert!(!events.iter().any(|event| matches!(event, Event::Error(_))));
        let text: String = events
            .iter()
            .filter_map(|event| match event {
                Event::Token(token) => Some(token.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hello!");

        let request = api.first_request();
        assert!(request.starts_with("POST /v1/chat/completions"));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer sekrit")
        );
        // Every stream asks for the bill (AD-0026).
        assert!(
            compact_body(api.first_request())
                .contains(r#""stream_options":{"include_usage":true}"#)
        );
        // send_json pretty-prints, so compare the body without
        // whitespace: collapse it all, then look for compact pairs.
        let compact: String = request.split_whitespace().collect();
        assert!(compact.contains("\"model\":\"test-model\""));
        assert!(compact.contains("\"stream\":true"));
        assert!(compact.contains("\"content\":\"hithere\""));
        // The system prompt rides as the leading system message,
        // before any user content.
        assert!(compact.contains("\"role\":\"system\""));
        assert!(compact.contains("Youareterse."));
        assert!(
            compact.find("\"role\":\"system\"").unwrap()
                < compact.find("\"role\":\"user\"").unwrap()
        );
        // The thinking level's kwargs merged into the request body.
        assert!(compact.contains("\"thinking\":{\"type\":\"enabled\"}"));
    }

    #[test]
    fn fragments_accumulate_into_calls_in_index_order() {
        let mut partials: BTreeMap<usize, PartialToolCall> = BTreeMap::new();
        // Call 1 streams in pieces; call 0 arrives interleaved between
        // call 1's fragments.
        for delta in [
            r#"{"index":1,"id":"call_b","function":{"name":"write","arguments":"{\"pa"}}"#,
            r#"{"index":0,"id":"call_a","function":{"name":"read","arguments":"{}"}}"#,
            r#"{"index":1,"function":{"arguments":"th\":\"x\"}"}}"#,
        ] {
            let delta: ToolCallDelta = serde_json::from_str(delta).unwrap();
            accumulate_fragment(&mut partials, delta);
        }
        let calls = finalize_tool_calls(partials);
        assert_eq!(calls.len(), 2);
        // Index order, fragments concatenated, ids kept.
        assert_eq!(calls[0].id, "call_a");
        assert_eq!(calls[0].name, "read");
        assert_eq!(calls[0].arguments, "{}");
        assert_eq!(calls[1].id, "call_b");
        assert_eq!(calls[1].name, "write");
        assert_eq!(calls[1].arguments, r#"{"path":"x"}"#);

        // A call whose server never sent an id gets a synthesized one.
        let mut partials: BTreeMap<usize, PartialToolCall> = BTreeMap::new();
        let delta: ToolCallDelta =
            serde_json::from_str(r#"{"index":2,"function":{"name":"go","arguments":"{}"}}"#)
                .unwrap();
        accumulate_fragment(&mut partials, delta);
        assert_eq!(finalize_tool_calls(partials)[0].id, "call_2");
    }

    #[test]
    fn streamed_tool_calls_surface_after_text_tokens() {
        let api = FakeApi::start(sse_response(&[
            "{\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}",
            "{\"choices\":[{\"delta\":{\"content\":\"Let me look. \"}}]}",
            "{\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"p\\\":\"}}]}}]}",
            "{\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"src\\\"}\"}}]}}]}",
            "[DONE]",
        ]));
        let provider = OpenAiCompatProvider::new(api.base_url.clone(), None, "m");
        let events: Vec<Event> = spawn_turn(Arc::new(provider), &[Message::User("look".into())])
            .iter()
            .collect();

        assert_eq!(events.first(), Some(&Event::TurnStart));
        assert_eq!(events.last(), Some(&Event::TurnEnd));
        // Text first, then the assembled call, then the turn closes.
        assert_eq!(events[1], Event::Token("Let me look. ".into()));
        assert_eq!(
            events[2],
            Event::ToolCall(ToolCall {
                id: "call_1".into(),
                name: "read".into(),
                arguments: r#"{"p":"src"}"#.into(),
            })
        );
        assert_eq!(events[3], Event::TurnEnd);
    }

    #[test]
    fn tools_and_results_round_trip_on_the_wire() {
        // Round 1 asks for the tool; round 2 settles.
        let api = FakeApi::start_sequence(vec![
            sse_response(&[
                "{\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_x\",\"function\":{\"name\":\"echo\",\"arguments\":\"{}\"}}]}}]}",
                "[DONE]",
            ]),
            sse_response(&[
                "{\"choices\":[{\"delta\":{\"content\":\"done\"}}]}",
                "[DONE]",
            ]),
        ]);
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(EchoTool)).unwrap();
        let registry = Arc::new(registry);
        let provider = OpenAiCompatProvider::new(api.base_url.clone(), None, "m")
            .with_tools(Arc::clone(&registry));
        let events: Vec<Event> = spawn_agent_turn(
            Arc::new(provider),
            registry,
            None,
            &[Message::User("go".into())],
            4,
        )
        .iter()
        .collect();

        assert!(!events.iter().any(|event| matches!(event, Event::Error(_))));
        assert_eq!(events.last(), Some(&Event::TurnEnd));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::ToolCall(_)))
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::ToolResult(r) if !r.is_error))
        );

        // Request 1 offered the tool.
        let compact: String = api.request(0).unwrap().split_whitespace().collect();
        // `json!` sorts keys alphabetically, so `function` precedes
        // `type` here — stable order is what matters, not key order.
        assert!(compact.contains("\"tools\":[{\"function\":"));
        assert!(compact.contains("\"type\":\"function\""));
        assert!(compact.contains("\"name\":\"echo\""));

        // Request 2 replayed the exchange in OpenAI's shapes: one
        // assistant message carrying the call, then a tool message
        // pointing back at it.
        let compact: String = api.request(1).unwrap().split_whitespace().collect();
        // Key order is alphabetical on the wire (`to_value` routes
        // through serde_json's sorted map), so assert content, not
        // order: the assistant message carries the call, and the tool
        // message points back at it via `tool_call_id`.
        assert!(compact.contains("\"tool_calls\":["));
        assert!(compact.contains("\"id\":\"call_x\""));
        assert!(compact.contains("\"role\":\"tool\""));
        assert!(compact.contains("\"tool_call_id\":\"call_x\""));
        assert!(compact.contains("\"content\":\"echo{}\""));
        assert!(compact.contains("\"tools\":["));
    }

    #[test]
    fn no_thinking_level_sends_no_kwargs() {
        let api = FakeApi::start(sse_response(&[
            "{\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}",
            "[DONE]",
        ]));
        let provider = OpenAiCompatProvider::new(api.base_url.clone(), None, "test-model");
        let rx = spawn_turn(Arc::new(provider), &[Message::User("hi".into())]);
        let events: Vec<Event> = rx.iter().collect();
        assert!(!events.iter().any(|event| matches!(event, Event::Error(_))));
        let request = api.first_request();
        let compact: String = request.split_whitespace().collect();
        assert!(!compact.contains("\"thinking\""));
    }

    #[test]
    fn http_error_becomes_error_then_turnend() {
        let api = FakeApi::start(
            "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        let provider = OpenAiCompatProvider::new(api.base_url.clone(), Some("bad".into()), "m");
        let rx = spawn_turn(Arc::new(provider), &[]);
        let events: Vec<Event> = rx.iter().collect();

        assert_eq!(events.first(), Some(&Event::TurnStart));
        let error = events.iter().find_map(|event| match event {
            Event::Error(msg) => Some(msg.clone()),
            _ => None,
        });
        assert!(error.unwrap().contains("401"));
        assert_eq!(events.last(), Some(&Event::TurnEnd));
    }

    #[test]
    fn extracts_ids_from_both_response_shapes() {
        let openai: serde_json::Value =
            serde_json::from_str(r#"{"data":[{"id":"b"},{"id":"a"}]}"#).unwrap();
        assert_eq!(
            extract_model_ids(&openai),
            vec!["b".to_string(), "a".to_string()]
        );

        let catalog: serde_json::Value =
            serde_json::from_str(r#"{"models":[{"slug":"glm-5.3"},{"id":"glm-4.6"}]}"#).unwrap();
        assert_eq!(
            extract_model_ids(&catalog),
            vec!["glm-5.3".to_string(), "glm-4.6".to_string()]
        );

        // Recognized envelope, unrecognized entries: empty, not error.
        let junk: serde_json::Value = serde_json::from_str(r#"{"data":[{}]}"#).unwrap();
        assert!(extract_model_ids(&junk).is_empty());
    }

    #[test]
    fn discovery_with_no_recognizable_shape_is_a_loud_error() {
        let api = FakeApi::start(json_response("{\"surprise\":true}"));
        let provider = OpenAiCompatProvider::new(api.base_url.clone(), None, "m");
        let err = provider.list_models().unwrap_err();
        assert!(err.contains("no recognizable"), "{err}");
        assert!(err.contains("models.json"), "{err}");
    }

    #[test]
    fn catalog_shaped_discovery_works_end_to_end() {
        let api = FakeApi::start(json_response(
            "{\"models\":[{\"slug\":\"glm-5.3\"},{\"slug\":\"glm-4.6\"}]}",
        ));
        let provider = OpenAiCompatProvider::new(api.base_url.clone(), None, "m");
        assert_eq!(
            provider.list_models().unwrap(),
            vec!["glm-4.6".to_string(), "glm-5.3".to_string()]
        );
    }

    // ---- wire identity: honest UA always, gated extras, cache key ----

    fn expected_user_agent() -> String {
        format!(
            "hi/{} ({}; {})",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    }

    #[test]
    fn wire_headers_are_honest_and_extras_are_gated() {
        let openrouter = OpenAiCompatProvider::new("https://openrouter.ai/api/v1", None, "m");
        // Default: only the User-Agent. Honest, nothing else.
        assert_eq!(
            openrouter.wire_headers(),
            vec![("User-Agent", expected_user_agent())]
        );
        // Telemetry opted in, OpenRouter endpoint: attribution rides.
        let attributed = OpenAiCompatProvider::new("https://openrouter.ai/api/v1", None, "m")
            .with_telemetry(true)
            .with_cache_key(Some("sess".into()));
        assert_eq!(
            attributed.wire_headers(),
            vec![
                ("User-Agent", expected_user_agent()),
                ("HTTP-Referer", "https://github.com/noosxe/hi".into()),
                ("X-Title", "hi".into()),
                ("x-session-id", "sess".into()),
            ]
        );
        // Telemetry on, other endpoint: no attribution, but the cache
        // affinity is OpenRouter-only too — other hosts get the bare UA.
        let elsewhere = OpenAiCompatProvider::new("http://127.0.0.1:8080/v1", None, "m")
            .with_telemetry(true)
            .with_cache_key(Some("sess".into()));
        assert_eq!(
            elsewhere.wire_headers(),
            vec![("User-Agent", expected_user_agent())]
        );
        // Telemetry off, OpenRouter endpoint: no attribution even so,
        // and the session header is routing (not attribution) — still
        // gated to the OpenRouter host only.
        let unattributed = OpenAiCompatProvider::new("https://openrouter.ai/api/v1", None, "m")
            .with_cache_key(Some("sess".into()));
        assert_eq!(
            unattributed.wire_headers(),
            vec![
                ("User-Agent", expected_user_agent()),
                ("x-session-id", "sess".into()),
            ]
        );
    }

    #[test]
    fn is_openrouter_matches_the_host_exactly() {
        assert!(is_openrouter("https://openrouter.ai/api/v1"));
        assert!(is_openrouter("http://OPENROUTER.AI"));
        assert!(is_openrouter("openrouter.ai/v1")); // scheme-less still parses
        // A host merely ending in the name is not the host.
        assert!(!is_openrouter("https://openrouter.ai.evil.example/v1"));
        assert!(!is_openrouter("https://example.com/v1"));
    }

    #[test]
    fn user_agent_rides_on_chat_requests() {
        let api = FakeApi::start(sse_response(&[
            "{\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}",
            "[DONE]",
        ]));
        let provider = OpenAiCompatProvider::new(api.base_url.clone(), None, "m");
        // Drain: the turn thread must finish before the request is read.
        let _: Vec<Event> = spawn_turn(Arc::new(provider), &[Message::User("hi".into())])
            .iter()
            .collect();
        assert!(
            api.first_request()
                .to_ascii_lowercase()
                .contains(&format!("user-agent: {}", expected_user_agent()))
        );
    }

    #[test]
    fn cache_key_rides_the_body_only_when_a_session_is_open() {
        let frames = sse_response(&[
            "{\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}",
            "[DONE]",
        ]);
        // With a session: the body names the cache key.
        let api = FakeApi::start(frames.clone());
        let provider = OpenAiCompatProvider::new(api.base_url.clone(), None, "m")
            .with_cache_key(Some("11111111-2222".into()));
        let _: Vec<Event> = spawn_turn(Arc::new(provider), &[Message::User("q".into())])
            .iter()
            .collect();
        assert!(
            compact_body(api.first_request()).contains("\"prompt_cache_key\":\"11111111-2222\"")
        );

        // Without one: the field is absent entirely.
        let api = FakeApi::start(frames);
        let provider = OpenAiCompatProvider::new(api.base_url.clone(), None, "m");
        let _: Vec<Event> = spawn_turn(Arc::new(provider), &[Message::User("q".into())])
            .iter()
            .collect();
        assert!(!compact_body(api.first_request()).contains("prompt_cache_key"));
    }

    /// Pull the JSON body out of a recorded request and collapse it for
    /// substring checks.
    fn compact_body(request: String) -> String {
        let body = request.split("\r\n\r\n").nth(1).unwrap_or(&request);
        body.split_whitespace().collect()
    }

    #[test]
    fn consecutive_turns_keep_a_byte_stable_prefix() {
        // The actual caching lever: OpenAI-compat caches match by
        // prefix, so turn N+1's system message, tool definitions, and
        // earlier history must serialize exactly as turn N sent them.
        let frames = |tail: &str| {
            sse_response(&[
                "{\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}",
                &format!("{{\"choices\":[{{\"delta\":{{\"content\":\"{tail}\"}}}}]}}"),
                "[DONE]",
            ])
        };
        let api = FakeApi::start_sequence(vec![frames("one"), frames("two")]);
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(EchoTool)).unwrap();
        let provider = OpenAiCompatProvider::new(api.base_url.clone(), None, "m")
            .with_system_prompt(Some("stable system".into()))
            .with_tools(Arc::new(registry));

        let turn1 = vec![Message::User("q1".into())];
        let _: Vec<Event> = spawn_turn(Arc::new(provider.clone()), &turn1)
            .iter()
            .collect();
        let turn2 = vec![
            Message::User("q1".into()),
            Message::Assistant("one".into()),
            Message::User("q2".into()),
        ];
        let _: Vec<Event> = spawn_turn(Arc::new(provider), &turn2).iter().collect();

        let parse = |request: String| -> serde_json::Value {
            let body = request.split("\r\n\r\n").nth(1).unwrap_or(&request);
            serde_json::from_str(body).expect("recorded body is JSON")
        };
        let first = parse(api.request(0).unwrap());
        let second = parse(api.request(1).unwrap());
        // The whole first payload is a prefix of the second's.
        assert_eq!(second["messages"][0], first["messages"][0]); // system
        assert_eq!(second["messages"][1], first["messages"][1]); // q1
        assert_eq!(second["tools"], first["tools"]);
        // Only the tail is new: turn 1's reply, then the follow-up.
        let first_len = first["messages"].as_array().unwrap().len();
        assert_eq!(first_len, 2);
        assert_eq!(
            second["messages"][first_len],
            serde_json::json!({
                "role": "assistant", "content": "one"
            })
        );
        assert_eq!(
            second["messages"][first_len + 1],
            serde_json::json!({
                "role": "user", "content": "q2"
            })
        );
        assert_eq!(second["messages"].as_array().unwrap().len(), first_len + 2);
    }
    // ---- images in read: the synthetic user carrier ----

    fn image_result() -> Message {
        Message::ToolResult(ToolResult {
            call_id: "call_1".into(),
            name: "imgtool".into(),
            content: "Read image file [image/png] (attached)".into(),
            is_error: false,
            images: vec![crate::tool::ImageBlock {
                data: "aW1n".into(),
                mime_type: "image/png".into(),
            }],
        })
    }

    #[test]
    fn tool_result_images_ride_a_synthetic_user_message() {
        // The OpenAI-compat `role: "tool"` message is text-only, so the
        // images must move to a follow-up user turn — pi-ai's exact
        // transformation, asserted here on the real wire.
        let api = FakeApi::start_sequence(vec![
            sse_response(&[
                "{\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"imgtool\",\"arguments\":\"{}\"}}]}}]}",
                "[DONE]",
            ]),
            sse_response(&[
                "{\"choices\":[{\"delta\":{\"content\":\"done\"}}]}",
                "[DONE]",
            ]),
        ]);
        let mut registry = ToolRegistry::new();
        registry
            .register(Arc::new(crate::test_support::ImageTool))
            .unwrap();
        let tools = Arc::new(registry.clone());
        let provider =
            OpenAiCompatProvider::new(api.base_url.clone(), None, "m").with_tools(tools.clone());
        let events: Vec<Event> = spawn_agent_turn(
            Arc::new(provider),
            tools,
            None,
            &[Message::User("look".into())],
            3,
        )
        .iter()
        .collect();
        assert!(matches!(events.last(), Some(Event::TurnEnd)));
        let body = api.request(1).unwrap();
        let json: serde_json::Value =
            serde_json::from_str(body.split("\r\n\r\n").nth(1).unwrap_or(&body)).unwrap();
        let messages = json["messages"].as_array().unwrap();
        let tool = messages
            .iter()
            .find(|m| m["role"] == "tool")
            .expect("tool message on the wire");
        assert_eq!(tool["content"], "Read image file [image/png] (attached)");
        // The carrier follows the tool message: text label, then parts.
        let carrier = &messages[messages.iter().position(|m| m["role"] == "tool").unwrap() + 1];
        assert_eq!(carrier["role"], "user");
        let parts = carrier["content"].as_array().unwrap();
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[0]["text"], "Attached image(s) from imgtool:");
        assert_eq!(parts[1]["type"], "image_url");
        assert_eq!(parts[1]["image_url"]["url"], "data:image/png;base64,aW1n");
    }

    #[test]
    fn image_only_tool_results_send_a_placeholder() {
        let api = FakeApi::start(sse_response(&[
            "{\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}",
            "[DONE]",
        ]));
        let provider = OpenAiCompatProvider::new(api.base_url.clone(), None, "m");
        let history = vec![
            Message::User("q".into()),
            Message::ToolResult(ToolResult {
                call_id: "c1".into(),
                name: "imgtool".into(),
                content: String::new(),
                is_error: false,
                images: vec![crate::tool::ImageBlock {
                    data: "aW1n".into(),
                    mime_type: "image/png".into(),
                }],
            }),
        ];
        let _: Vec<Event> = spawn_turn(Arc::new(provider), &history).iter().collect();
        let compact = compact_body(api.first_request());
        assert!(compact.contains("(seeattachedimage)"));
        assert!(compact.contains("Attachedimage(s)fromimgtool:"));
    }

    #[test]
    fn image_payloads_keep_the_prefix_stable() {
        // The caching invariant (AD-0007) survives images: the
        // synthetic carrier is derived, not stored, and serializes
        // byte-identically on every replay.
        let frames = |tail: &str| {
            sse_response(&[
                "{\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}",
                &format!("{{\"choices\":[{{\"delta\":{{\"content\":\"{tail}\"}}}}]}}"),
                "[DONE]",
            ])
        };
        let api = FakeApi::start_sequence(vec![frames("one"), frames("two")]);
        let provider = OpenAiCompatProvider::new(api.base_url.clone(), None, "m");
        let base = vec![
            Message::User("q1".into()),
            Message::Assistant("Checking.".into()),
            Message::ToolCall(crate::tool::ToolCall {
                id: "call_1".into(),
                name: "imgtool".into(),
                arguments: "{}".into(),
            }),
            image_result(),
        ];
        let turn1 = base.clone();
        let _: Vec<Event> = spawn_turn(Arc::new(provider.clone()), &turn1)
            .iter()
            .collect();
        let mut turn2 = base.clone();
        turn2.push(Message::Assistant("one".into()));
        turn2.push(Message::User("q2".into()));
        let _: Vec<Event> = spawn_turn(Arc::new(provider), &turn2).iter().collect();

        let parse = |request: String| -> serde_json::Value {
            serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap_or(&request)).unwrap()
        };
        let first = parse(api.request(0).unwrap());
        let second = parse(api.request(1).unwrap());
        let first_len = first["messages"].as_array().unwrap().len();
        for (i, old) in first["messages"].as_array().unwrap().iter().enumerate() {
            assert_eq!(second["messages"][i], *old, "prefix message {i} moved");
        }
        assert_eq!(first_len, 4);
        // The carrier is exactly where it was, byte-for-byte.
        assert_eq!(
            second["messages"][3]["content"][1]["image_url"]["url"],
            "data:image/png;base64,aW1n"
        );
    }
}
