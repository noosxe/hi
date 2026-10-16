//! The abstract thinking level — hi's provider-neutral "effort".
//!
//! Levels name intent (`off` … `high`); models.json's per-model
//! `thinkingLevelMap` turns a level into concrete request kwargs
//! (e.g. GLM: `{"thinking": {"type": "enabled"}}`, OpenAI:
//! `{"reasoning_effort": "high"}`). No provider knowledge in code —
//! the mapping is configuration.

use serde::{Deserialize, Serialize};

/// The reasoning-effort levels, ordered weakest to strongest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    Off,
    Minimal,
    Low,
    Medium,
    High,
    /// OpenAI/xAI's extra rungs above `high` — present so the ladder
    /// matches pi's and the vendors' (AD-0044 ruling 1); they only
    /// reach the wire through a `thinkingLevelMap` entry or a family
    /// default that names them.
    Xhigh,
    Max,
}

impl ThinkingLevel {
    /// Parse a level name (case-insensitive).
    pub fn parse(name: &str) -> Option<Self> {
        match name.to_lowercase().as_str() {
            "off" => Some(Self::Off),
            "minimal" => Some(Self::Minimal),
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" => Some(Self::Xhigh),
            "max" => Some(Self::Max),
            _ => None,
        }
    }

    /// The canonical lowercase name (settings keys, status display).
    pub fn name(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

/// URL-family default kwargs (AD-0044 ruling 4's wire half): when the
/// model has no `thinkingLevelMap` entry, a recognized provider family
/// still gets sensible level kwargs — config always overrides these.
/// Unknown hosts return `None`; nothing is guessed.
pub fn family_kwargs(base_url: &str, level: ThinkingLevel) -> Option<serde_json::Value> {
    let host = base_url
        .split("://")
        .nth(1)
        .unwrap_or(base_url)
        .split('/')
        .next()
        .unwrap_or("");
    let host = host.strip_prefix("api.").unwrap_or(host);
    match host {
        // Z.ai/GLM: binary thinking switch, no effort gradations.
        h if h.starts_with("z.ai") => Some(match level {
            ThinkingLevel::Off => serde_json::json!({ "thinking": { "type": "disabled" } }),
            _ => serde_json::json!({ "thinking": { "type": "enabled" } }),
        }),
        "openai.com" => match level {
            ThinkingLevel::Off => None,
            level => Some(serde_json::json!({ "reasoning_effort": level.name() })),
        },
        "openrouter.ai" => match level {
            ThinkingLevel::Off => None,
            level => Some(serde_json::json!({ "reasoning": { "effort": level.name() } })),
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_levels_case_insensitively() {
        for name in ["off", "minimal", "low", "medium", "high", "xhigh", "max"] {
            let level = ThinkingLevel::parse(name).unwrap();
            assert_eq!(level.name(), name);
            assert_eq!(ThinkingLevel::parse(&name.to_uppercase()), Some(level));
        }
        // The full ladder matches pi's and OpenAI's (AD-0044 ruling 1).
        assert_eq!(ThinkingLevel::parse("ultra"), None);
        assert_eq!(ThinkingLevel::parse(""), None);
    }

    #[test]
    fn family_defaults_by_base_url() {
        // Z.ai/GLM is a binary switch: any level maps to enabled, off
        // to disabled (AD-0044: unconfigured models still answer
        // /thinking).
        let zai = "https://api.z.ai/api/paas/v4";
        let kw = family_kwargs(zai, ThinkingLevel::High).unwrap();
        assert_eq!(kw["thinking"]["type"], "enabled");
        let kw = family_kwargs(zai, ThinkingLevel::Off).unwrap();
        assert_eq!(kw["thinking"]["type"], "disabled");

        // OpenAI-family: the level name rides `reasoning_effort`; off
        // omits rather than forcing `none` (model-dependent support).
        let openai = "https://api.openai.com/v1";
        let kw = family_kwargs(openai, ThinkingLevel::Xhigh).unwrap();
        assert_eq!(kw["reasoning_effort"], "xhigh");
        assert_eq!(family_kwargs(openai, ThinkingLevel::Off), None);

        // OpenRouter normalizes to reasoning.effort.
        let or = "https://openrouter.ai/api/v1";
        let kw = family_kwargs(or, ThinkingLevel::Minimal).unwrap();
        assert_eq!(kw["reasoning"]["effort"], "minimal");

        // Unknown hosts stay out of the guessing business.
        assert_eq!(
            family_kwargs("http://localhost:8080/v1", ThinkingLevel::High),
            None
        );
    }

    #[test]
    fn serde_uses_the_lowercase_name() {
        let level: ThinkingLevel = serde_json::from_str("\"high\"").unwrap();
        assert_eq!(level, ThinkingLevel::High);
        assert_eq!(
            serde_json::to_string(&ThinkingLevel::Off).unwrap(),
            "\"off\""
        );
    }
}
