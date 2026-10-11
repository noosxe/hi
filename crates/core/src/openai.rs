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

use std::io::{BufRead, BufReader};
use std::sync::mpsc::Sender;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::{ModelConfig, ProviderConfig};
use crate::event::Event;
use crate::message::Message;
use crate::provider::Provider;
use crate::thinking::ThinkingLevel;

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
            messages.push(WireMessage {
                role: "system",
                content: system,
            });
        }
        messages.extend(wire_messages(history));
        let chat_request = ChatRequest {
            model: &self.model,
            messages,
            stream: true,
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
        let response = self
            .authorized(self.agent.post(&url))
            .send_json(&payload)
            .map_err(|err| describe("chat request", err))?;

        // The body reader is a plain `Read`; buffering turns it into
        // lines, which is exactly what SSE frames are.
        let buffered = BufReader::new(response.into_body().into_reader());
        for line in buffered.lines() {
            let line = line.map_err(|err| format!("reading stream: {err}"))?;
            match parse_data_line(&line) {
                DataLine::Token(token) => {
                    if tx.send(Event::Token(token)).is_err() {
                        // Receiver dropped mid-stream: that is the
                        // cancellation signal. Stop quietly — nobody
                        // is listening for a final event.
                        return Ok(());
                    }
                }
                DataLine::Done => return Ok(()),
                DataLine::Ignore => {}
                DataLine::Malformed(why) => return Err(why),
            }
        }
        // Server closed without `[DONE]`: tolerate it and treat the
        // stream as complete.
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
pub(crate) fn discover_model_ids(
    base_url: &str,
    api_key: Option<&str>,
) -> Result<Vec<String>, String> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let request = bounded_agent()
        .get(&url)
        .config()
        .timeout_global(Some(Duration::from_secs(10)))
        .build();
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
/// comments (`: ping`), event names, and blank lines are noise.
#[derive(Debug, PartialEq)]
enum DataLine {
    Token(String),
    Done,
    Ignore,
    Malformed(String),
}

fn parse_data_line(line: &str) -> DataLine {
    let Some(payload) = line.strip_prefix("data:") else {
        return DataLine::Ignore;
    };
    let payload = payload.trim();
    if payload == "[DONE]" {
        return DataLine::Done;
    }
    let chunk: StreamChunk = match serde_json::from_str(payload) {
        Ok(chunk) => chunk,
        Err(err) => return DataLine::Malformed(format!("malformed stream payload: {err}")),
    };
    // First choice, its delta content if present. Role-only and empty
    // deltas happen (openers, keep-alives) and are not tokens.
    match chunk
        .choices
        .into_iter()
        .next()
        .and_then(|c| c.delta.content)
    {
        Some(content) if !content.is_empty() => DataLine::Token(content),
        _ => DataLine::Ignore,
    }
}

/// One OpenAI chat request. `&'a str` borrows instead of cloning —
/// the payload is built, sent, and dropped within one call.
#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<WireMessage<'a>>,
    stream: bool,
}

#[derive(Serialize)]
struct WireMessage<'a> {
    role: &'static str,
    content: &'a str,
}

/// Map the harness's history onto the wire. `Message::Error` and
/// `Message::System` entries are local annotations — replaying them
/// to the model as conversation turns would be meaningless, so they
/// are dropped.
fn wire_messages(history: &[Message]) -> Vec<WireMessage<'_>> {
    history
        .iter()
        .filter_map(|msg| {
            let role = match msg {
                Message::Error(_) | Message::System(_) => return None,
                Message::User(_) => "user",
                Message::Assistant(_) => "assistant",
            };
            Some(WireMessage {
                role,
                content: msg.text(),
            })
        })
        .collect()
}

/// One streamed chunk. Only `choices[0].delta.content` matters to hi;
/// everything else (ids, finish reasons, usage) is ignored by serde.
#[derive(Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
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
    use crate::test_support::{FakeApi, json_response, sse_response};
    use std::sync::Arc;

    #[test]
    fn parses_sse_data_lines() {
        assert_eq!(
            parse_data_line("data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}"),
            DataLine::Token("Hi".into())
        );
        assert_eq!(parse_data_line("data: [DONE]"), DataLine::Done);
        // Role-only opener chunk: no content, not a token.
        assert_eq!(
            parse_data_line("data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}"),
            DataLine::Ignore
        );
        // SSE comments and stray lines are noise.
        assert_eq!(parse_data_line(": keep-alive"), DataLine::Ignore);
        assert_eq!(parse_data_line(""), DataLine::Ignore);
        // Garbage payload must not be silently dropped.
        assert!(matches!(
            parse_data_line("data: {not json"),
            DataLine::Malformed(_)
        ));
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
}
