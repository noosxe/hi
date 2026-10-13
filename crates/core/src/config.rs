//! `models.json` — provider/model configuration, shaped like pi's.
//!
//! The file lives at `~/.config/hi/models.json` on every platform
//! (`$XDG_CONFIG_HOME/hi/models.json` when set) — one path to
//! configure, sync, and reason about on Linux and macOS alike.
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

fn default_vision() -> bool {
    true
}

/// One model entry.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ModelConfig {
    /// The model identifier sent to the API.
    pub id: String,
    /// Optional display name; falls back to `id`.
    #[serde(default)]
    pub name: Option<String>,
    /// Whether the model supports reasoning output.
    #[serde(default)]
    pub reasoning: bool,
    /// Whether the model accepts image input. Defaults to **true**
    /// (pi-parity, AD-0012): models omitted from this file — e.g.
    /// discovered from the API — read as vision-capable, and
    /// `vision: false` is the explicit opt-out that makes `read`
    /// return a metadata note instead of attaching. The old default
    /// (false, #29) blinded every dynamically discovered model.
    #[serde(default = "default_vision")]
    pub vision: bool,
    /// Context window size, in tokens — the denominator of the usage
    /// line's context share on the status board (AD-0026). Absent ⇒ no
    /// share is shown; nothing is guessed. Truncation policy may claim
    /// it later.
    #[serde(rename = "contextWindow", default)]
    pub context_window: Option<u32>,
    /// Maximum completion tokens the model accepts.
    #[serde(rename = "maxTokens", default)]
    pub max_tokens: Option<u32>,
    /// Per-level request kwargs (pi-compatible key): when the
    /// session's thinking level is `"high"` and this map has a
    /// `"high"` entry, that JSON object merges into the chat request
    /// body. Values are arbitrary JSON, e.g.
    /// `{"thinking": {"type": "enabled"}}` for GLM.
    #[serde(rename = "thinkingLevelMap", default)]
    pub thinking_level_map: Option<BTreeMap<String, serde_json::Value>>,
}

impl ProviderConfig {
    /// The image-attachment policy for `model_id` — pi's fail-open
    /// rule (AD-0012): the settings master switch wins first; a model
    /// configured here honors an explicit `vision: false`; a model
    /// with no entry (API-discovered) reads as vision-capable. The
    /// pessimistic #29 default blinded every dynamically discovered
    /// model, which is most of them in practice.
    pub fn images_allowed(&self, model_id: &str, settings_enabled: bool) -> bool {
        if !settings_enabled {
            return false;
        }
        match self.models.iter().find(|m| m.id == model_id) {
            Some(model) => model.vision,
            None => true,
        }
    }
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

impl ProviderConfig {
    /// Hybrid model listing for selection-time decisions (before any
    /// provider object exists): explicit entries win; an empty list
    /// means `GET {baseUrl}/models`.
    pub fn list_model_ids(&self) -> Result<Vec<String>, String> {
        if !self.models.is_empty() {
            return Ok(self.models.iter().map(|model| model.id.clone()).collect());
        }
        let key = self.resolved_api_key().filter(|key| !key.is_empty());
        crate::openai::discover_model_ids(&self.base_url, key.as_deref())
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

/// hi's configuration directory: `~/.config/hi`, honoring
/// `$XDG_CONFIG_HOME`. Deliberately identical on Linux and macOS —
/// one path across machines, friendly to dotfile syncing.
pub fn config_dir() -> Option<PathBuf> {
    config_dir_from(
        std::env::var_os("XDG_CONFIG_HOME").as_deref(),
        std::env::home_dir().as_deref(),
    )
}

/// The pure decision, split out for testing: an explicit XDG
/// override wins, else `<home>/.config`. An *empty*
/// `XDG_CONFIG_HOME` counts as unset, per the XDG spec.
fn config_dir_from(
    xdg: Option<&std::ffi::OsStr>,
    home: Option<&std::path::Path>,
) -> Option<PathBuf> {
    let base = match xdg {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => home?.join(".config"),
    };
    Some(base.join("hi"))
}

/// hi's data directory: `~/.local/share/hi`, honoring `$XDG_DATA_HOME`
/// with the same precedence rules as [`config_dir`] (explicit override
/// wins, empty counts as unset). Sessions are user *data*, not
/// configuration, so they live here — big, machine-generated, safe to
/// back up separately from the hand-written config files.
pub fn data_dir() -> Option<PathBuf> {
    data_dir_from(
        std::env::var_os("XDG_DATA_HOME").as_deref(),
        std::env::home_dir().as_deref(),
    )
}

/// The pure decision, split out for testing — mirrors
/// [`config_dir_from`] with the `.local/share` base.
fn data_dir_from(xdg: Option<&std::ffi::OsStr>, home: Option<&std::path::Path>) -> Option<PathBuf> {
    let base = match xdg {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => home?.join(".local/share"),
    };
    Some(base.join("hi"))
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
        serde_json::from_str(&raw).map_err(|err| format!("cannot parse {}: {err}", path.display()))
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
    fn vision_defaults_to_true_and_parses() {
        let raw = r#"{"providers":{"p":{"baseUrl":"http://x/v1","models":[
            {"id":"plain"},{"id":"blind","vision":false}]}}}"#;
        let cfg: ModelsConfig = serde_json::from_str(raw).unwrap();
        let models = &cfg.providers.get("p").unwrap().models;
        // Unnamed capability = vision-capable (pi's default, AD-0012);
        // only an explicit `vision: false` opts out.
        assert!(models[0].vision);
        assert!(!models[1].vision);
    }

    #[test]
    fn image_policy_is_fail_open_for_unknown_models() {
        let raw = r#"{"providers":{"p":{"baseUrl":"http://x/v1","models":[
            {"id":"blind","vision":false},{"id":"seer"}]}}}"#;
        let cfg: ModelsConfig = serde_json::from_str(raw).unwrap();
        let entry = cfg.providers.get("p").unwrap();
        // Master switch off wins over everything.
        assert!(!entry.images_allowed("seer", false));
        assert!(!entry.images_allowed("blind", false));
        // Configured model: the explicit opt-out is honored.
        assert!(!entry.images_allowed("blind", true));
        assert!(entry.images_allowed("seer", true));
        // No entry (API discovery): attach — pi's `!model ||` arm.
        assert!(entry.images_allowed("gpt-who-knows", true));
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

    #[test]
    fn config_dir_prefers_xdg_then_falls_back_to_home() {
        let from_xdg =
            config_dir_from(Some("/custom/xdg".as_ref()), Some("/home/u".as_ref())).unwrap();
        assert_eq!(from_xdg, PathBuf::from("/custom/xdg/hi"));

        let from_home = config_dir_from(None, Some("/home/u".as_ref())).unwrap();
        assert_eq!(from_home, PathBuf::from("/home/u/.config/hi"));

        // Empty XDG_CONFIG_HOME counts as unset, per the XDG spec.
        let empty = config_dir_from(Some("".as_ref()), Some("/home/u".as_ref())).unwrap();
        assert_eq!(empty, PathBuf::from("/home/u/.config/hi"));

        // No home anywhere — no directory to speak of.
        assert!(config_dir_from(None, None).is_none());

        // The data directory mirrors the same precedence rules, with
        // `.local/share` as the base.
        let data_xdg =
            data_dir_from(Some("/custom/xdg".as_ref()), Some("/home/u".as_ref())).unwrap();
        assert_eq!(data_xdg, PathBuf::from("/custom/xdg/hi"));
        let data_home = data_dir_from(None, Some("/home/u".as_ref())).unwrap();
        assert_eq!(data_home, PathBuf::from("/home/u/.local/share/hi"));
        assert!(data_dir_from(None, None).is_none());
    }
}
