//! `models.json` — provider/model configuration, shaped like pi's.
//!
//! The file lives in hi's config directory (`~/.config/hi/models.json`
//! on Linux, `~/Library/Application Support/hi/models.json` on macOS).
//! Deserialization ignores unknown fields on purpose: new keys added
//! later (or copied from a pi config) never break older builds — the
//! forward-compatibility trick pi itself relies on.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Deserialize;

/// The parsed contents of `models.json`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ModelsConfig {
    /// Providers by name. The name is only a lookup key — `hi` has no
    /// opinion about which services exist.
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
}

/// One OpenAI-compatible endpoint and the models reachable through it.
#[derive(Debug, Clone, Deserialize)]
pub struct ProviderConfig {
    /// Root of the API, including any version path — for example
    /// `http://localhost:8080/v1`.
    #[serde(rename = "baseUrl")]
    pub base_url: String,
    /// Credential source: a literal value, or `$NAME` / `${NAME}` for
    /// environment interpolation (pi's syntax). Optional — some local
    /// servers ignore auth entirely.
    #[serde(default, rename = "apiKey")]
    pub api_key: Option<String>,
    /// Models reachable at this endpoint.
    #[serde(default)]
    pub models: Vec<ModelConfig>,
}

/// One model entry.
#[derive(Debug, Clone, Deserialize)]
pub struct ModelConfig {
    /// The model identifier sent to the API.
    pub id: String,
    /// Optional display name; falls back to `id`.
    #[serde(default)]
    pub name: Option<String>,
    /// Whether the model supports reasoning output.
    #[serde(default)]
    pub reasoning: bool,
    /// Context window size, in tokens. Informational for now — the
    /// harness will use it for truncation policy later.
    #[serde(rename = "contextWindow", default)]
    pub context_window: Option<u32>,
    /// Maximum completion tokens the model accepts.
    #[serde(rename = "maxTokens", default)]
    pub max_tokens: Option<u32>,
}

impl ProviderConfig {
    /// Resolve the API key for a request: `$NAME` / `${NAME}` read
    /// the environment (missing variables become empty — the request
    /// then simply goes out without credentials); anything else is a
    /// literal. `None` means the endpoint wants no auth header.
    pub fn resolved_api_key(&self) -> Option<String> {
        self.api_key.as_ref().map(|raw| resolve_value(raw))
    }
}

/// pi's value syntax: `$NAME` or `${NAME}` interpolates the
/// environment; anything else is taken literally.
fn resolve_value(raw: &str) -> String {
    if let Some(name) = raw.strip_prefix("${").and_then(|s| s.strip_suffix('}')) {
        std::env::var(name).unwrap_or_default()
    } else if let Some(name) = raw.strip_prefix('$') {
        std::env::var(name).unwrap_or_default()
    } else {
        raw.to_string()
    }
}

/// hi's configuration directory — platform-conventional via the
/// `dirs` crate: `~/.config/hi` on Linux, `~/Library/Application
/// Support/hi` on macOS.
pub fn config_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("hi"))
}

/// The expected path of `models.json` inside the config directory.
pub fn models_path() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("models.json"))
}

impl ModelsConfig {
    /// Load and parse `<config-dir>/models.json`. Errors are plain
    /// strings for now — descriptive, user-facing, and good enough
    /// until a dedicated error type earns its keep.
    pub fn load() -> Result<Self, String> {
        let path = models_path().ok_or("could not determine config directory")?;
        let raw = std::fs::read_to_string(&path)
            .map_err(|err| format!("cannot read {}: {err}", path.display()))?;
        serde_json::from_str(&raw)
            .map_err(|err| format!("cannot parse {}: {err}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pi_style_models_json() {
        let raw = r#"{
            "providers": {
                "local": {
                    "baseUrl": "http://localhost:8080/v1",
                    "apiKey": "$HI_TEST_KEY",
                    "models": [
                        {
                            "id": "test-model",
                            "name": "Test Model",
                            "reasoning": true,
                            "contextWindow": 8192,
                            "maxTokens": 2048
                        }
                    ]
                }
            }
        }"#;
        let cfg: ModelsConfig = serde_json::from_str(raw).unwrap();
        let provider = cfg.providers.get("local").unwrap();
        assert_eq!(provider.base_url, "http://localhost:8080/v1");
        assert_eq!(provider.models[0].id, "test-model");
        assert_eq!(provider.models[0].name.as_deref(), Some("Test Model"));
        assert!(provider.models[0].reasoning);
        assert_eq!(provider.models[0].context_window, Some(8192));
        assert_eq!(provider.models[0].max_tokens, Some(2048));
    }

    #[test]
    fn unknown_fields_are_ignored_for_forward_compatibility() {
        // Keys copied from a pi models.json that hi does not know
        // about yet must not break parsing.
        let raw = r#"{
            "providers": {
                "p": {
                    "baseUrl": "http://x/v1",
                    "api": "openai-completions",
                    "compat": { "supportsDeveloperRole": false },
                    "models": [
                        { "id": "m", "input": ["text"], "thinkingLevelMap": { "low": "low" } }
                    ]
                }
            },
            "modelOverrides": {}
        }"#;
        let cfg: ModelsConfig = serde_json::from_str(raw).unwrap();
        assert_eq!(cfg.providers["p"].models[0].id, "m");
    }

    #[test]
    fn resolves_env_interpolation_like_pi() {
        // Edition 2024 marks set_var unsafe: this test process is
        // single-threaded, so mutating the environment is sound here.
        unsafe { std::env::set_var("HI_TEST_KEY", "sekrit") };
        assert_eq!(resolve_value("$HI_TEST_KEY"), "sekrit");
        assert_eq!(resolve_value("${HI_TEST_KEY}"), "sekrit");
        assert_eq!(resolve_value("literal-key"), "literal-key");
        assert_eq!(resolve_value("$HI_TEST_KEY_MISSING"), "");
    }
}
