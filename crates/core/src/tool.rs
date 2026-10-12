//! The tool system: what a model may call, and who decides what runs.
//!
//! Three pieces live here:
//!
//! - [`ToolCall`] / [`ToolResult`] — the data types that ride through
//!   `Message`, `Event`, and the wire format. They live in one place so
//!   every consumer shares the exact same shapes.
//! - The [`Tool`] trait and [`ToolRegistry`] — how a capability announces
//!   itself (name, JSON-Schema parameters, description, annotations) and
//!   how the harness looks it up by name.
//! - The interception seam ([`ToolGate`] / [`ToolDecision`]) — the hook
//!   consulted before *any* tool executes. This is deliberately in core
//!   and deliberately runtime-free: the extension host (issue #6) will
//!   implement gates in Lua later, but the seam itself must not depend
//!   on any scripting runtime (AD-0002).

/// The model's request to run a tool.
///
/// `arguments` is kept as **raw JSON text** — byte-for-byte what the
/// model produced (or what a gate mutated it into). Parsing is the
/// dispatch step's job, not the type's: the tool itself is the authority
/// on its own schema, and keeping the raw text means the UI and the wire
/// replay show exactly what was said, errors included.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    /// Correlation id. The provider supplies it (OpenAI-style
    /// `call_…` ids); when a server omits one, the stream layer
    /// synthesizes `call_{index}`. Every `ToolResult` must carry the
    /// `call_id` of the call it answers.
    pub id: String,
    /// Which registered tool to run.
    pub name: String,
    /// Raw JSON object text, e.g. `{"path":"src/main.rs"}`.
    pub arguments: String,
}

/// The harness's answer to one [`ToolCall`] — success or failure, both
/// are *content* for the model.
///
/// Tool failures are not `Event::Error`s: an error result is still a
/// successful step of the conversation (the model learns the call failed
/// and adapts). `Event::Error` stays reserved for the turn itself
/// failing — the single-finalization-path contract.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolResult {
    /// The `ToolCall::id` this result answers.
    pub call_id: String,
    /// Echoed for display convenience; the wire uses `call_id`.
    pub name: String,
    /// What the tool produced (or the refusal/error text).
    pub content: String,
    /// Harness-side flag: true when the call was denied, unknown, or
    /// the tool reported failure. The OpenAI wire has no such flag, so
    /// it is dropped in serialization — the *content* carries the news.
    pub is_error: bool,
}

/// Hints about what a tool does to the world.
///
/// Pure metadata for permission gates and UIs — hi never enforces them
/// itself (a tool that lies about being read-only is a trust problem,
/// not a type-system problem). Names follow the MCP annotation style.
///
/// The derived default is deliberately conservative: both flags `false`
/// — assume the tool writes and might destroy, until it says otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ToolAnnotations {
    /// The tool only observes; it does not modify anything.
    pub read_only: bool,
    /// The tool may destroy or irreversibly change state.
    pub destructive: bool,
}

/// One capability the model can invoke.
///
/// Deliberately synchronous and blocking: tools run on the agent-loop
/// thread, which is off the UI thread already (the loop runs on its own
/// worker, mirroring `spawn_turn`). There is no async runtime anywhere
/// in hi; a long-running tool should do its own threading and return
/// promptly, and background tasks are a separate core feature (#5).
pub trait Tool: Send + Sync {
    /// Registry key and the name the model calls. Must be unique.
    fn name(&self) -> &str;
    /// One-paragraph explanation for the model.
    fn description(&self) -> &str;
    /// JSON Schema (an object) describing the `arguments` the tool
    /// accepts. Sent verbatim in the request's `tools` array.
    fn parameters(&self) -> serde_json::Value;
    /// Behavior hints for gates and UIs. Defaults to the conservative
    /// "writes, may destroy".
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations::default()
    }
    /// Run the tool. `arguments` is the raw JSON text (validated as
    /// parseable JSON by dispatch, but *not* schema-checked — the tool
    /// owns its schema). `Ok` is a success result, `Err` an error
    /// result; both go back to the model as content.
    fn execute(&self, arguments: &str) -> Result<String, String>;
}

/// Name-keyed collection of available tools.
///
/// A `BTreeMap` so the wire order of `tools` is stable and sorted —
/// identical sessions produce identical requests, which keeps tests and
/// provider-side caching well-behaved.
#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: std::collections::BTreeMap<String, std::sync::Arc<dyn Tool>>,
}

impl ToolRegistry {
    /// An empty registry — the default for sessions without tools.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a tool. Registering a duplicate name is a programming error,
    /// not a runtime condition, so it is a loud `Err` rather than a
    /// silent overwrite.
    pub fn register(&mut self, tool: std::sync::Arc<dyn Tool>) -> Result<(), String> {
        let name = tool.name().to_string();
        if name.is_empty() {
            return Err("tool name must not be empty".into());
        }
        if self.tools.contains_key(&name) {
            return Err(format!("duplicate tool name: {name}"));
        }
        self.tools.insert(name, tool);
        Ok(())
    }

    /// Look up by exact name.
    pub fn get(&self, name: &str) -> Option<&std::sync::Arc<dyn Tool>> {
        self.tools.get(name)
    }

    /// True when nothing is registered — providers use this to omit the
    /// `tools` field entirely instead of sending an empty array.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// How many tools are registered.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// The wire form: `{"type":"function","function":{…}}` entries in
    /// stable (sorted) order.
    pub fn definitions(&self) -> Vec<serde_json::Value> {
        self.tools
            .values()
            .map(|tool| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": tool.name(),
                        "description": tool.description(),
                        "parameters": tool.parameters(),
                    },
                })
            })
            .collect()
    }
}

/// What a [`ToolGate`] says about a proposed call.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolDecision {
    /// Run the call as proposed.
    Allow,
    /// Refuse it. The reason becomes the error `ToolResult` content, so
    /// the model sees *why* and can adapt instead of retrying blind.
    Deny(String),
    /// Run the call with different `arguments` (raw JSON text, replacing
    /// the model's wholesale). The loop re-validates the replacement as
    /// JSON before executing.
    Mutate(String),
}

/// The interception seam: consulted before any tool executes.
///
/// A plain function type on purpose — the smallest thing that satisfies
/// allow / deny / mutate without inventing a trait hierarchy ahead of
/// need. The extension host (#6) gets its Rust-side seam here and can
/// back it with whatever runtime it likes (AD-0001's "swappable host"
/// rule); nothing in this signature mentions Lua.
pub type ToolGate = dyn Fn(&ToolCall) -> ToolDecision + Send + Sync;
