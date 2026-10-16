//! The shipped model-metadata table — hi's tier-3 knowledge source.
//!
//! Tier chain per AD-0044: explicit `models.json` fields win; then live
//! OpenRouter metadata (only when the provider *is* OpenRouter); then
//! this snapshot; then nothing. The table is regenerated per release
//! from OpenRouter's public `/api/v1/models` by
//! `scripts/gen-catalog.py`; only fields hi can act on are kept.
//!
//! Lookup matches the provider-local model id against OpenRouter slugs:
//! exact (`vendor/model`) first, then a *unique* bare-name suffix
//! (`claude-opus-4.7` → `anthropic/claude-opus-4.7`). Ambiguous or
//! missing ⇒ `None` — honest ignorance, never a wrong guess. The
//! snapshot's bare names are unique today (checked at generation time);
//! the uniqueness guard keeps that a safety property, not an accident.

use std::collections::HashMap;
use std::sync::OnceLock;

/// One model's vendor metadata, as far as hi can act on it.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelInfo {
    /// Vendor display name (`"Anthropic: Claude Opus 4.7"`).
    pub name: Option<String>,
    /// Context window, tokens — the usage line's context-share
    /// denominator (AD-0026's "nothing is guessed" now reads "nothing
    /// beyond vendor metadata").
    pub context_window: Option<u64>,
    /// Max completion tokens.
    pub max_output: Option<u64>,
    /// Supported reasoning efforts (OpenAI's vocabulary), when the
    /// vendor publishes them. `None` = unknown — the `/thinking` marks
    /// come from URL-family defaults instead.
    pub efforts: Option<Vec<String>>,
    /// The vendor's default effort, when published.
    pub default_effort: Option<String>,
    /// `true` when reasoning cannot be turned off on this model.
    pub reasoning_mandatory: bool,
    /// Per-token rates, dollars — the bill's `~$` estimate.
    pub prompt_rate: Option<f64>,
    pub completion_rate: Option<f64>,
    pub cache_read_rate: Option<f64>,
    pub cache_write_rate: Option<f64>,
    /// Image input support — `None` when the vendor says nothing.
    pub vision: Option<bool>,
    /// Tool-calling support.
    pub tools: bool,
}

pub fn lookup(id: &str) -> Option<&'static ModelInfo> {
    let table = table();
    if let Some(info) = table.get(id) {
        return Some(info);
    }
    let suffix = format!("/{id}");
    let matches: Vec<&ModelInfo> = table
        .iter()
        .filter(|(key, _)| key.ends_with(&suffix))
        .map(|(_, info)| info)
        .collect();
    // Unique bare name only — two vendors shipping the same name means
    // no metadata rather than a coin flip.
    if matches.len() == 1 {
        Some(matches[0])
    } else {
        None
    }
}

fn table() -> &'static HashMap<String, ModelInfo> {
    static TABLE: OnceLock<HashMap<String, ModelInfo>> = OnceLock::new();
    TABLE.get_or_init(|| {
        serde_json::from_str::<RawTable>(include_str!("catalog.json"))
            .expect("catalog.json is generated; malformed output is a gen-catalog bug")
            .models
            .into_iter()
            .map(|(id, raw)| (id, raw.into_info()))
            .collect()
    })
}

#[derive(serde::Deserialize)]
struct RawTable {
    #[serde(default)]
    models: HashMap<String, RawInfo>,
}

#[derive(serde::Deserialize)]
struct RawInfo {
    name: Option<String>,
    context: Option<u64>,
    #[serde(rename = "maxOutput")]
    max_output: Option<u64>,
    efforts: Option<Vec<String>>,
    #[serde(rename = "defaultEffort")]
    default_effort: Option<String>,
    #[serde(rename = "reasoningMandatory", default)]
    reasoning_mandatory: bool,
    #[serde(rename = "promptRate")]
    prompt_rate: Option<f64>,
    #[serde(rename = "completionRate")]
    completion_rate: Option<f64>,
    #[serde(rename = "cacheReadRate")]
    cache_read_rate: Option<f64>,
    #[serde(rename = "cacheWriteRate")]
    cache_write_rate: Option<f64>,
    vision: Option<bool>,
    tools: Option<bool>,
}

impl RawInfo {
    fn into_info(self) -> ModelInfo {
        ModelInfo {
            name: self.name,
            context_window: self.context,
            max_output: self.max_output,
            efforts: self.efforts,
            default_effort: self.default_effort,
            reasoning_mandatory: self.reasoning_mandatory,
            prompt_rate: self.prompt_rate,
            completion_rate: self.completion_rate,
            cache_read_rate: self.cache_read_rate,
            cache_write_rate: self.cache_write_rate,
            vision: self.vision,
            tools: self.tools.unwrap_or(false),
        }
    }
}

/// One row of OpenRouter's live `/api/v1/models` payload — tier 2's
/// fresher context/output (AD-0044).
#[derive(Debug, Clone, PartialEq)]
pub struct LiveRow {
    pub id: String,
    pub context_window: u64,
    pub max_output: Option<u64>,
}

/// Fetch OpenRouter's public model list — tier 2's live refresh.
/// `None` on any failure (network, timeout, shape): tier 2 must never
/// block a start (AD-0044).
pub fn fetch_live_rows(timeout: std::time::Duration) -> Option<Vec<LiveRow>> {
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .build(),
    );
    let body = agent
        .get("https://openrouter.ai/api/v1/models")
        .call()
        .ok()?
        .body_mut()
        .read_to_string()
        .ok()?;
    parse_live(&body)
}

/// Parse the live payload down to [`LiveRow`]s. `None` when the shape
/// isn't what the public API promises — tier 2 stays silent then.
pub fn parse_live(json: &str) -> Option<Vec<LiveRow>> {
    #[derive(serde::Deserialize)]
    struct Payload {
        data: Vec<Raw>,
    }
    #[derive(serde::Deserialize)]
    struct Raw {
        id: String,
        #[serde(rename = "context_length")]
        context_window: u64,
        // OpenRouter's payload is snake_case on both nests.
        top_provider: Option<Top>,
    }
    #[derive(serde::Deserialize)]
    struct Top {
        max_completion_tokens: Option<u64>,
    }
    let payload: Payload = serde_json::from_str(json).ok()?;
    Some(
        payload
            .data
            .into_iter()
            .map(|raw| LiveRow {
                id: raw.id,
                context_window: raw.context_window,
                max_output: raw.top_provider.and_then(|t| t.max_completion_tokens),
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn parse_live_reads_the_public_shape() {
        let json = r#"{"data":[{"id":"anthropic/claude-opus-4.7","context_length":1000000,
            "top_provider":{"context_length":1000000,"max_completion_tokens":128000,"is_moderated":false}},
            {"id":"x/y","context_length":8192,"top_provider":null}]}"#;
        let rows = super::parse_live(json).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, "anthropic/claude-opus-4.7");
        assert_eq!(rows[0].context_window, 1_000_000);
        assert_eq!(rows[0].max_output, Some(128_000));
        assert_eq!(rows[1].max_output, None);
        assert!(super::parse_live(r#"{"unexpected": true}"#).is_none());
    }

    use super::*;

    #[test]
    fn exact_slug_lookup() {
        let info = lookup("anthropic/claude-opus-4.7").unwrap();
        assert_eq!(info.context_window, Some(1_000_000));
        assert_eq!(info.vision, Some(true));
        assert!(info.tools);
        let efforts = info.efforts.as_ref().unwrap();
        assert!(efforts.contains(&"max".to_string()));
        assert!(efforts.contains(&"low".to_string()));
        assert_eq!(info.default_effort.as_deref(), Some("high"));
    }

    #[test]
    fn bare_suffix_lookup() {
        // Provider-local ids drop the vendor prefix; the suffix match
        // bridges (today every bare name in the snapshot is unique).
        let bare = lookup("claude-opus-4.7").unwrap();
        let full = lookup("anthropic/claude-opus-4.7").unwrap();
        assert_eq!(bare, full);
    }

    #[test]
    fn missing_model_is_none() {
        assert!(lookup("no-such-model-anywhere").is_none());
    }

    #[test]
    fn vendor_without_reasoning_metadata_is_known_but_unmarked() {
        // Z.ai publishes no reasoning metadata via OpenRouter — the row
        // still carries window/pricing; efforts stay None and the
        // /thinking marks fall to URL-family defaults (AD-0044).
        let info = lookup("z-ai/glm-4.6").unwrap();
        assert!(info.efforts.is_none());
        assert_eq!(info.context_window, Some(204_800));
        // OpenRouter lists GLM-4.6 text-only: vision stays None = "no
        // vendor opinion", and hi's config default (AD-0012) applies.
        assert_eq!(info.vision, None);
    }
}
