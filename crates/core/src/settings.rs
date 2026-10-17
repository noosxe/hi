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
    /// Master switch for sending images to the model (v1: `read`
    /// attachments; the paste path (#41) will honor it too). Defaults
    /// to on; the starting model's `vision` flag gates it further.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<bool>,
    /// Opt-in orchestrator hook (#72, AD-0022): a finished background
    /// task may start a turn whose payload carries the completion
    /// facts, so unattended flows ("run the suite, then summarize")
    /// complete without a keystroke. Defaults to off — in v1 only the
    /// human spends turns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// The `subagents` master switch (AD-0043): off removes the
    /// nested-agent tool from the registry — absent beats erroring.
    /// Startup policy only, not `/settings`-flippable.
    pub subagents: Option<bool>,
    pub task_turns: Option<bool>,
    /// Opt-in session retention (#23): delete session files older than
    /// this many days at startup, before any session opens. Absent
    /// means never prune — sessions are user history, and pi-parity
    /// was verified (pi never deletes them). Startup policy only, so
    /// it is not flippable from `/settings`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_retention: Option<u64>,
    /// Main-loop tool-round cap, resolved once at startup: absent means
    /// the built-in default (`agent::DEFAULT_MAX_TOOL_ROUNDS`, 32);
    /// when present it must be at least 1 (zero fails every turn).
    /// The subagent's own cap is a different knob — the `subagent`
    /// tool's `max_rounds` argument, defaulting to
    /// `subagent::SUBAGENT_MAX_TOOL_ROUNDS` (8). Startup policy only,
    /// not flippable from `/settings`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tool_rounds: Option<usize>,
    /// Opt-in subagent transcript recording (#116): when true, every
    /// subagent run writes its own ordinary session file (`kind:
    /// "subagent"`, parent id in the header) under this project's
    /// sessions directory. Absent/false records nothing — the default,
    /// because a chatty parent turn can spawn many files. Startup
    /// policy only, not flippable from `/settings`.
    #[serde(default)]
    pub subagent_transcripts: bool,
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
            Ok(raw) => {
                let settings: Settings = serde_json::from_str(&raw)
                    .map_err(|err| format!("cannot parse {}: {err}", path.display()))?;
                if settings.max_tool_rounds == Some(0) {
                    return Err(format!(
                        "cannot parse {}: maxToolRounds must be at least 1",
                        path.display()
                    ));
                }
                Ok(settings)
            }
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

    /// Image upload is on unless switched off — the reverse of
    /// telemetry's opt-in, because attaching a file is a per-turn
    /// convenience, not an identification fact.
    pub fn images_enabled(&self) -> bool {
        self.images.unwrap_or(true)
    }

    /// Auto-triggering is off unless switched on — like telemetry,
    /// because a turn costs the user's tokens and the default session
    /// must behave exactly as before (#72).
    /// Ruling #1/#2 (AD-0043): the `subagents` master switch —
    /// `false` removes the `subagent` tool from the registry entirely,
    /// so the model never sees it. Absent means on: subagents ship as
    /// a default capability, not an opt-in.
    pub fn subagents_enabled(&self) -> bool {
        self.subagents.unwrap_or(true)
    }

    pub fn task_turns_enabled(&self) -> bool {
        self.task_turns.unwrap_or(false)
    }

    /// Persist `default_model` while keeping every other setting — the
    /// Ctrl-S path (#45). Load-modify-save, never reconstruct: building a
    /// fresh `Settings` here would silently erase the user's thinking,
    /// tools, telemetry, and image choices. An unparseable file is an
    /// error and is left untouched — surfacing beats destroying.
    pub fn persist_default_model(model: &str) -> Result<(), String> {
        let path = settings_path().ok_or("could not determine config directory")?;
        Self::persist_default_model_to(&path, model)
    }

    /// The path-independent half of `persist_default_model`.
    fn persist_default_model_to(path: &Path, model: &str) -> Result<(), String> {
        Self::persist_with(path, |settings| {
            settings.default_model = Some(model.to_string());
        })
    }

    /// Persist the `/settings` board's task-turns flip (#79, AD-0023)
    /// — Ctrl-S, not Enter. Load-modify-save: only that field is
    /// written; every other user setting survives, and an unparseable
    /// file is refused untouched.
    pub fn persist_task_turns(enabled: bool) -> Result<(), String> {
        let path = settings_path().ok_or("could not determine config directory")?;
        Self::persist_task_turns_to(&path, enabled)
    }

    /// The path-independent half of `persist_task_turns`.
    fn persist_task_turns_to(path: &Path, enabled: bool) -> Result<(), String> {
        Self::persist_with(path, |settings| settings.task_turns = Some(enabled))
    }

    /// Persist `/thinking`'s level choice (AD-0044 ruling 2): `None`
    /// means auto — the key is simply cleared, provider decides.
    pub fn persist_thinking(level: Option<crate::thinking::ThinkingLevel>) -> Result<(), String> {
        let path = settings_path().ok_or("could not determine config directory")?;
        Self::persist_thinking_to(&path, level)
    }

    /// The path-independent half of `persist_thinking`.
    fn persist_thinking_to(
        path: &Path,
        level: Option<crate::thinking::ThinkingLevel>,
    ) -> Result<(), String> {
        Self::persist_with(path, |settings| settings.thinking = level)
    }

    /// Persist the `/settings` board's image flip (#79, AD-0023).
    pub fn persist_images(enabled: bool) -> Result<(), String> {
        let path = settings_path().ok_or("could not determine config directory")?;
        Self::persist_images_to(&path, enabled)
    }

    /// The path-independent half of `persist_images`.
    fn persist_images_to(path: &Path, enabled: bool) -> Result<(), String> {
        Self::persist_with(path, |settings| settings.images = Some(enabled))
    }

    /// The shared Ctrl-S shape (#45): load, mutate one field, save.
    /// One construction path so a new board row can never grow a
    /// bespoke persistence dialect.
    fn persist_with(path: &Path, mutate: impl FnOnce(&mut Settings)) -> Result<(), String> {
        let mut settings = Self::load_from(path)?;
        mutate(&mut settings);
        settings.save_to(path)
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
    fn persist_thinking_round_trips_and_clears() {
        let dir = std::env::temp_dir().join(format!("hi-set-think-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(&path, r#"{"defaultModel": "m"}"#).unwrap();
        Settings::persist_thinking_to(&path, Some(crate::thinking::ThinkingLevel::Xhigh)).unwrap();
        let s = Settings::load_from(&path).unwrap();
        assert_eq!(s.thinking, Some(crate::thinking::ThinkingLevel::Xhigh));
        assert_eq!(s.default_model.as_deref(), Some("m"), "siblings survive");
        Settings::persist_thinking_to(&path, None).unwrap();
        let s = Settings::load_from(&path).unwrap();
        assert_eq!(s.thinking, None, "auto = key cleared");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn subagents_default_on_camel_case() {
        // Absent = on (the default capability); camelCase `subagents`
        // set false removes the tool at resolve time.
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert!(s.subagents_enabled());
        let s: Settings = serde_json::from_str(r#"{"subagents": false}"#).unwrap();
        assert!(!s.subagents_enabled());
        let s: Settings = serde_json::from_str(r#"{"subagents": true}"#).unwrap();
        assert!(s.subagents_enabled());
    }

    #[test]
    fn parses_camel_case_and_tolerates_unknown_fields() {
        let raw = r#"{"defaultModel": "zai/glm-5.3", "futureThing": {"a": 1}}"#;
        let settings: Settings = serde_json::from_str(raw).unwrap();
        assert_eq!(settings.default_model.as_deref(), Some("zai/glm-5.3"));
    }

    #[test]
    fn session_retention_is_opt_in_camel_case_days() {
        let raw = r#"{"sessionRetention": 30}"#;
        let settings: Settings = serde_json::from_str(raw).unwrap();
        assert_eq!(settings.session_retention, Some(30));
        let settings: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(settings.session_retention, None);
    }

    #[test]
    fn max_tool_rounds_is_opt_in_camel_case() {
        let raw = r#"{"maxToolRounds": 40}"#;
        let settings: Settings = serde_json::from_str(raw).unwrap();
        assert_eq!(settings.max_tool_rounds, Some(40));
        let settings: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(settings.max_tool_rounds, None);
    }

    #[test]
    fn max_tool_rounds_zero_is_rejected() {
        let dir = std::env::temp_dir().join(format!("hi-settings-cap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(&path, r#"{"maxToolRounds": 0}"#).unwrap();
        let err = Settings::load_from(&path).unwrap_err();
        assert!(err.contains("maxToolRounds must be at least 1"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn subagent_transcripts_default_off_camel_case() {
        let settings: Settings = serde_json::from_str(r#"{"subagentTranscripts": true}"#).unwrap();
        assert!(settings.subagent_transcripts);
        let settings: Settings = serde_json::from_str("{}").unwrap();
        assert!(!settings.subagent_transcripts);
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
    fn persist_default_model_keeps_every_other_setting() {
        // The #45 contract: Ctrl-S writes one field, not a fresh file.
        let dir = std::env::temp_dir().join(format!("hi-settings-{}-persist", std::process::id()));
        let path = dir.join("settings.json");
        let existing = Settings {
            default_model: None,
            thinking: Some(ThinkingLevel::High),
            default_tools: Some("all".into()),
            telemetry: Some(true),
            images: Some(false),
            subagents: None,
            task_turns: None,
            session_retention: None,
            max_tool_rounds: None,
            subagent_transcripts: false,
        };
        existing.save_to(&path).unwrap();

        Settings::persist_default_model_to(&path, "zai/glm-5.3").unwrap();

        let loaded = Settings::load_from(&path).unwrap();
        assert_eq!(loaded.default_model.as_deref(), Some("zai/glm-5.3"));
        assert_eq!(loaded.thinking, Some(ThinkingLevel::High));
        assert_eq!(loaded.default_tools.as_deref(), Some("all"));
        assert_eq!(loaded.telemetry, Some(true));
        assert_eq!(loaded.images, Some(false));
    }

    #[test]
    fn persist_flags_keep_every_other_setting() {
        // The #45 contract, now for the /settings board (#79): Ctrl-S
        // writes one field, not a fresh file. Both flags share the
        // same persist_with path, so one test covers the shape and
        // one pass proves neither clobbers the other.
        let dir = std::env::temp_dir().join(format!("hi-settings-{}-flags", std::process::id()));
        let path = dir.join("settings.json");
        let existing = Settings {
            default_model: Some("zai/glm-5.3".into()),
            thinking: Some(ThinkingLevel::High),
            default_tools: Some("all".into()),
            telemetry: Some(true),
            images: Some(false),
            subagents: None,
            task_turns: None,
            session_retention: None,
            max_tool_rounds: None,
            subagent_transcripts: false,
        };
        existing.save_to(&path).unwrap();

        Settings::persist_task_turns_to(&path, true).unwrap();
        Settings::persist_images_to(&path, true).unwrap();

        let loaded = Settings::load_from(&path).unwrap();
        assert_eq!(loaded.default_model.as_deref(), Some("zai/glm-5.3"));
        assert_eq!(loaded.thinking, Some(ThinkingLevel::High));
        assert_eq!(loaded.default_tools.as_deref(), Some("all"));
        assert_eq!(loaded.telemetry, Some(true));
        assert_eq!(loaded.images, Some(true));
        assert_eq!(loaded.task_turns, Some(true));
    }

    #[test]
    fn persist_flag_on_missing_file_writes_just_the_choice() {
        let dir =
            std::env::temp_dir().join(format!("hi-settings-{}-flagfirst", std::process::id()));
        let path = dir.join("settings.json");

        Settings::persist_task_turns_to(&path, true).unwrap();

        let loaded = Settings::load_from(&path).unwrap();
        assert_eq!(loaded.task_turns, Some(true));
        assert_eq!(loaded.default_model, None);
        assert_eq!(loaded.images, None);
    }

    #[test]
    fn persist_flag_refuses_to_clobber_an_unparseable_file() {
        let dir =
            std::env::temp_dir().join(format!("hi-settings-{}-flagbroken", std::process::id()));
        let path = dir.join("settings.json");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, "{ not json").unwrap();

        let err = Settings::persist_images_to(&path, true).unwrap_err();

        assert!(err.contains("cannot parse"), "got: {err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
    }

    #[test]
    fn persist_default_model_on_missing_file_writes_just_the_choice() {
        let dir =
            std::env::temp_dir().join(format!("hi-settings-{}-firstsave", std::process::id()));
        let path = dir.join("settings.json");

        Settings::persist_default_model_to(&path, "local/llama").unwrap();

        let loaded = Settings::load_from(&path).unwrap();
        assert_eq!(loaded.default_model.as_deref(), Some("local/llama"));
        // Nothing else existed to keep — every other field stays unset.
        assert_eq!(loaded.thinking, None);
        assert_eq!(loaded.default_tools, None);
        assert_eq!(loaded.telemetry, None);
        assert_eq!(loaded.images, None);
    }

    #[test]
    fn persist_default_model_refuses_to_clobber_an_unparseable_file() {
        let dir = std::env::temp_dir().join(format!("hi-settings-{}-broken", std::process::id()));
        let path = dir.join("settings.json");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, "{ not json").unwrap();

        let err = Settings::persist_default_model_to(&path, "zai/glm-5.3").unwrap_err();

        assert!(err.contains("cannot parse"), "got: {err}");
        // The broken file survives untouched — surfacing beats destroying.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
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
            images: None,
            subagents: None,
            task_turns: None,
            session_retention: None,
            max_tool_rounds: None,
            subagent_transcripts: false,
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
    fn images_default_on_and_parse_camel_case() {
        let settings: Settings = serde_json::from_str("{}").unwrap();
        assert!(settings.images_enabled());
        assert_eq!(settings.images, None);
        let settings: Settings = serde_json::from_str("{\"images\": false}").unwrap();
        assert!(!settings.images_enabled());
    }

    #[test]
    fn task_turns_defaults_off_and_parses_camel_case() {
        let settings: Settings = serde_json::from_str("{}").unwrap();
        assert!(!settings.task_turns_enabled());
        assert_eq!(settings.task_turns, None);
        let settings: Settings = serde_json::from_str(r#"{"taskTurns": true}"#).unwrap();
        assert!(settings.task_turns_enabled());
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
