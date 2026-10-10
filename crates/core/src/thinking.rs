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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_levels_case_insensitively() {
        for name in ["off", "minimal", "low", "medium", "high"] {
            let level = ThinkingLevel::parse(name).unwrap();
            assert_eq!(level.name(), name);
            assert_eq!(ThinkingLevel::parse(&name.to_uppercase()), Some(level));
        }
        assert_eq!(ThinkingLevel::parse("max"), None);
        assert_eq!(ThinkingLevel::parse(""), None);
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
