//! `settings.json` — saved preferences, pi-style.
//!
//! Currently one key: `defaultModel` (`"provider/model"`). Written by
//! the model picker (Ctrl-S), read at startup between CLI flags and
//! autopick. Unknown fields are ignored; a *missing file is normal* —
//! saving a preference must not require creating it by hand first,
//! which is the difference from `models.json`.

use std::path::{Path, PathBuf};

use crate::thinking::ThinkingLevel;
use serde::{Deserialize, Serialize};

/// The parsed contents of `settings.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    /// `"provider/model"` — the session default. An effort preference
    /// will join it here later.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    /// Reasoning effort level: `off` | `minimal` | `low` | `medium` |
    /// `high`. Reaches the wire only when the model's
    /// `thinkingLevelMap` has an entry for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingLevel>,
    /// Which built-in tool bundle launches with the session: `none` |
    /// `read-only` | `coding` | `all` (default `coding`). `--tools`
    /// overrides per run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_tools: Option<String>,
    /// Attribution opt-in. When true, hi sends provider attribution
    /// headers (OpenRouter's `HTTP-Referer`/`X-Title`) so usage shows
    /// up as hi in provider dashboards. The honest `User-Agent` rides
    /// regardless; hi sends no analytics ever.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telemetry: Option<bool>,
}

/// The effective telemetry choice: `HI_TELEMETRY` wins when set (pi's
/// `PI_TELEMETRY` pattern), accepting `1`/`true`/`yes` as on and
/// anything else — including garbage — as off; otherwise the setting
/// decides, defaulting to off.
pub fn resolve_telemetry(setting: Option<bool>, env: Option<&str>) -> bool {
    match env {
        Some(raw) => matches!(raw.to_ascii_lowercase().as_str(), "1" | "true" | "yes"),
        None => setting.unwrap_or(false),
    }
}

/// The expected path of `settings.json` next to `models.json`.
pub fn settings_path() -> Option<PathBuf> {
    crate::config::config_dir().map(|dir| dir.join("settings.json"))
}

impl Settings {
    /// Load `<config-dir>/settings.json`.
    pub fn load() -> Result<Self, String> {
        let Some(path) = settings_path() else {
            return Ok(Self::default());
        };
        Self::load_from(&path)
    }

    /// The path-independent half of `load`, so tests can use temp files.
    fn load_from(path: &Path) -> Result<Self, String> {
        match std::fs::read_to_string(path) {
            Ok(raw) => serde_json::from_str(&raw)
                .map_err(|err| format!("cannot parse {}: {err}", path.display())),
            // A missing settings file is the normal first-run state.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(format!("cannot read {}: {err}", path.display())),
        }
    }

    /// The effective telemetry choice for this configuration, with
    /// `HI_TELEMETRY` overriding the file.
    pub fn telemetry_enabled(&self) -> bool {
        resolve_telemetry(
            self.telemetry,
            std::env::var("HI_TELEMETRY").ok().as_deref(),
        )
    }

    /// Persist to `<config-dir>/settings.json` (pretty-printed, so
    /// hand edits and tool writes stay compatible).
    pub fn save(&self) -> Result<(), String> {
        let path = settings_path().ok_or("could not determine config directory")?;
        self.save_to(&path)
    }

    /// The path-independent half of `save`, so tests can use temp files.
    fn save_to(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| format!("cannot create {}: {err}", parent.display()))?;
        }
        let pretty = serde_json::to_string_pretty(self)
            .map_err(|err| format!("cannot serialize settings: {err}"))?;
        std::fs::write(path, pretty + "\n")
            .map_err(|err| format!("cannot write {}: {err}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_camel_case_and_tolerates_unknown_fields() {
        let raw = r#"{"defaultModel": "zai/glm-5.3", "futureThing": {"a": 1}}"#;
        let settings: Settings = serde_json::from_str(raw).unwrap();
        assert_eq!(settings.default_model.as_deref(), Some("zai/glm-5.3"));
    }

    #[test]
    fn empty_object_means_no_default() {
        let settings: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(settings.default_model, None);
    }

    #[test]
    fn missing_file_is_default_settings() {
        let dir = std::env::temp_dir().join(format!("hi-settings-{}-missing", std::process::id()));
        let settings = Settings::load_from(&dir.join("settings.json")).unwrap();
        assert_eq!(settings, Settings::default());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir =
            std::env::temp_dir().join(format!("hi-settings-{}-roundtrip", std::process::id()));
        let path = dir.join("nested/settings.json");
        let original = Settings {
            default_model: Some("local/llama".into()),
            thinking: None,
            default_tools: Some("read-only".into()),
            telemetry: None,
        };
        original.save_to(&path).unwrap();
        let loaded = Settings::load_from(&path).unwrap();
        assert_eq!(loaded, original);
        // Pretty-printed and newline-terminated: pleasant to hand-edit.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("\"defaultModel\""));
        assert!(raw.ends_with('\n'));
    }

    #[test]
    fn unparseable_file_is_an_error_not_a_crash() {
        let dir = std::env::temp_dir().join(format!("hi-settings-{}-junk", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(&path, "{not json").unwrap();
        assert!(Settings::load_from(&path).is_err());
    }

    #[test]
    fn telemetry_defaults_to_off_and_parses_camel_case() {
        let settings: Settings = serde_json::from_str("{}").unwrap();
        assert!(!settings.telemetry_enabled());
        assert!(!resolve_telemetry(None, None));
        assert!(resolve_telemetry(Some(true), None));
        assert!(!resolve_telemetry(Some(false), None));

        let settings: Settings = serde_json::from_str("{\"telemetry\": true}").unwrap();
        assert_eq!(settings.telemetry, Some(true));
    }

    #[test]
    fn hi_telemetry_env_wins_over_the_setting() {
        // pi's PI_TELEMETRY pattern: env beats the file, truthiness is
        // strict, garbage reads as off.
        assert!(resolve_telemetry(Some(false), Some("1")));
        assert!(resolve_telemetry(Some(false), Some("YES")));
        assert!(!resolve_telemetry(Some(true), Some("0")));
        assert!(!resolve_telemetry(Some(true), Some("false")));
        // An unset-looking or malformed value is off, not an error.
        assert!(!resolve_telemetry(Some(true), Some("")));
        assert!(!resolve_telemetry(Some(true), Some("sure?")));
    }
}
