//! Picking which provider and model a session uses.
//!
//! v0 precedence (a saved `defaultModel` sits between flags and
//! autopick):
//!
//! 1. `--provider P --model M` — used as given. M is deliberately not
//!    validated: for discovery providers hi cannot know the ids
//!    without asking the server, and the turn request is the asking.
//! 2. `--provider P` — the first model P offers (configured list, or
//!    live discovery).
//! 3. `--model M` — the first provider (sorted by name) whose
//!    *configured* list contains M.
//! 4. `defaultModel` from settings — used only when *no* flags are
//!    given; a stale or malformed preference falls through to
//!    autopick instead of failing startup.
//! 5. nothing — the first provider in sorted order that offers any
//!    model at all ("first available"): discovery runs per provider;
//!    failures and empty lists skip to the next one.
//!
//! Every error names what *was* available, so the fix is in the message.

use crate::config::ModelsConfig;

/// The outcome of resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selected {
    pub provider: String,
    pub model: String,
}

/// Resolve provider/model from optional CLI flags against a config.
/// Is `provider/model` selectable right now? The restore rule
/// (AD-0046): the provider entry must exist, and a configured model
/// list must contain the id — an entry with no `models` discovers,
/// so any id on it is fair game.
pub fn model_available(config: &crate::config::ModelsConfig, provider: &str, model: &str) -> bool {
    match config.providers.get(provider) {
        None => false,
        Some(entry) => entry.models.is_empty() || entry.models.iter().any(|m| m.id == model),
    }
}

pub fn resolve(
    config: &ModelsConfig,
    provider: Option<&str>,
    model: Option<&str>,
    default_model: Option<&str>,
) -> Result<Selected, String> {
    // The saved default only speaks when flags are silent, and a
    // stale preference must never brick startup: fall through to the
    // flag/autopick paths instead of erroring.
    if provider.is_none()
        && model.is_none()
        && let Some(selected) = default_model.and_then(|saved| from_saved(config, saved))
    {
        return Ok(selected);
    }
    match (provider, model) {
        (Some(name), Some(id)) => {
            if config.providers.contains_key(name) {
                Ok(Selected {
                    provider: name.into(),
                    model: id.into(),
                })
            } else {
                Err(unknown_provider(config, name))
            }
        }
        (Some(name), None) => {
            let entry = config
                .providers
                .get(name)
                .ok_or_else(|| unknown_provider(config, name))?;
            let models = entry
                .list_model_ids()
                .map_err(|err| format!("provider '{name}': {err}"))?;
            models
                .first()
                .map(|id| Selected {
                    provider: name.into(),
                    model: id.clone(),
                })
                .ok_or_else(|| format!("provider '{name}' offers no models"))
        }
        (None, Some(id)) => {
            for (name, entry) in &config.providers {
                if entry.models.iter().any(|m| m.id == id) {
                    return Ok(Selected {
                        provider: name.clone(),
                        model: id.into(),
                    });
                }
            }
            Err(format!(
                "model '{id}' is not in any configured model list (configured: {}). \
                 Providers that discover their models need --provider.",
                available_pairs(config),
            ))
        }
        (None, None) => {
            // First available, in sorted provider order. Discovery
            // failures skip to the next provider; every attempt is
            // remembered for the final error.
            let mut failures: Vec<String> = Vec::new();
            for (name, entry) in &config.providers {
                match entry.list_model_ids() {
                    Ok(models) => {
                        if let Some(id) = models.first() {
                            return Ok(Selected {
                                provider: name.clone(),
                                model: id.clone(),
                            });
                        }
                        failures.push(format!("{name}: no models"));
                    }
                    Err(err) => failures.push(format!("{name}: {err}")),
                }
            }
            let detail = if failures.is_empty() {
                "the config has no providers".to_string()
            } else {
                failures.join("; ")
            };
            Err(format!(
                "no provider offers a model ({detail}). Add models to \
                 models.json, check that servers are reachable, or run with --mock."
            ))
        }
    }
}

/// Parse and validate a saved `"provider/model"` preference. `None`
/// when malformed or naming a provider the config no longer has —
/// the model id itself is *not* checked, because discovery providers
/// cannot be validated offline (the same rule as explicit flags).
fn from_saved(config: &ModelsConfig, saved: &str) -> Option<Selected> {
    let (provider, model) = saved.split_once('/')?;
    if provider.is_empty() || model.is_empty() {
        return None;
    }
    config.providers.contains_key(provider).then(|| Selected {
        provider: provider.to_string(),
        model: model.to_string(),
    })
}
/// Which provider's models to print for `--list-models`: the explicit
/// pick, else the first provider that answers with any model.
pub fn list_for(
    config: &ModelsConfig,
    provider: Option<&str>,
) -> Result<(String, Vec<String>), String> {
    match provider {
        Some(name) => {
            let entry = config
                .providers
                .get(name)
                .ok_or_else(|| unknown_provider(config, name))?;
            let models = entry
                .list_model_ids()
                .map_err(|err| format!("provider '{name}': {err}"))?;
            Ok((name.into(), models))
        }
        None => {
            // Same contract as `resolve`'s autopick: failures are
            // collected and reported, never silently skipped — a
            // broken discovery must not masquerade as "no config".
            let mut failures: Vec<String> = Vec::new();
            for (name, entry) in &config.providers {
                match entry.list_model_ids() {
                    Ok(models) if !models.is_empty() => {
                        return Ok((name.clone(), models));
                    }
                    Ok(_) => failures.push(format!("{name}: no models")),
                    Err(err) => failures.push(format!("{name}: {err}")),
                }
            }
            let detail = if failures.is_empty() {
                "the config has no providers".to_string()
            } else {
                failures.join("; ")
            };
            Err(format!("no provider offers models ({detail})"))
        }
    }
}

fn unknown_provider(config: &ModelsConfig, name: &str) -> String {
    let names: Vec<&str> = config.providers.keys().map(String::as_str).collect();
    let configured = if names.is_empty() {
        "none".to_string()
    } else {
        names.join(", ")
    };
    format!("unknown provider '{name}' (configured: {configured})")
}

fn available_pairs(config: &ModelsConfig) -> String {
    let mut pairs = Vec::new();
    for (name, entry) in &config.providers {
        for model in &entry.models {
            pairs.push(format!("{name}/{}", model.id));
        }
    }
    if pairs.is_empty() {
        "none".to_string()
    } else {
        pairs.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ModelConfig, ProviderConfig};
    use crate::test_support::{FakeApi, json_response};
    use std::collections::BTreeMap;

    /// Build a config from `(name, base_url, [model ids])` rows.
    fn config_with(rows: &[(&str, &str, &[&str])]) -> ModelsConfig {
        let mut providers = BTreeMap::new();
        for (name, base_url, ids) in rows {
            providers.insert(
                name.to_string(),
                ProviderConfig {
                    base_url: base_url.to_string(),
                    api_key: None,
                    request_compression: false,
                    models: ids
                        .iter()
                        .map(|id| ModelConfig {
                            id: id.to_string(),
                            ..Default::default()
                        })
                        .collect(),
                },
            );
        }
        ModelsConfig { providers }
    }

    #[test]
    fn explicit_provider_and_model_pass_through() {
        let cfg = config_with(&[("a", "http://x/v1", &["x"])]);
        // The model id is not validated: for discovery providers hi
        // cannot know the ids without asking the server.
        assert_eq!(
            resolve(&cfg, Some("a"), Some("anything"), None).unwrap(),
            Selected {
                provider: "a".into(),
                model: "anything".into()
            }
        );
    }

    #[test]
    fn unknown_provider_names_the_configured_ones() {
        let cfg = config_with(&[("local", "http://x/v1", &["x"])]);
        let err = resolve(&cfg, Some("nope"), Some("m"), None).unwrap_err();
        assert!(err.contains("unknown provider 'nope'"), "{err}");
        assert!(err.contains("local"), "{err}");
    }

    #[test]
    fn provider_alone_takes_its_first_configured_model() {
        let cfg = config_with(&[("a", "http://x/v1", &["m2", "m1"])]);
        assert_eq!(
            resolve(&cfg, Some("a"), None, None).unwrap(),
            Selected {
                provider: "a".into(),
                model: "m2".into()
            }
        );
    }

    #[test]
    fn model_alone_finds_the_first_provider_listing_it() {
        let cfg = config_with(&[
            ("beta", "http://b/v1", &["other"]),
            ("alpha", "http://a/v1", &["shared"]),
        ]);
        assert_eq!(
            resolve(&cfg, None, Some("shared"), None).unwrap(),
            Selected {
                provider: "alpha".into(),
                model: "shared".into()
            }
        );
    }

    #[test]
    fn autopick_takes_the_first_provider_in_sorted_order() {
        let cfg = config_with(&[
            ("zeta", "http://z/v1", &["z-model"]),
            ("alpha", "http://a/v1", &["a-model"]),
        ]);
        assert_eq!(
            resolve(&cfg, None, None, None).unwrap(),
            Selected {
                provider: "alpha".into(),
                model: "a-model".into()
            }
        );
    }

    #[test]
    fn autopick_discovers_and_skips_failures() {
        // 'alpha' only discovers, and its server is broken: 'beta'
        // (configured) must win.
        let api = FakeApi::start(
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        let cfg = config_with(&[
            ("alpha", &api.base_url, &[]),
            ("beta", "http://b/v1", &["b-model"]),
        ]);
        assert_eq!(
            resolve(&cfg, None, None, None).unwrap(),
            Selected {
                provider: "beta".into(),
                model: "b-model".into()
            }
        );
    }

    #[test]
    fn autopick_error_names_every_attempt() {
        let api = FakeApi::start(
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        let cfg = config_with(&[("alpha", &api.base_url, &[])]);
        let err = resolve(&cfg, None, None, None).unwrap_err();
        assert!(err.contains("alpha"), "{err}");
        assert!(err.contains("--mock"), "{err}");
    }

    #[test]
    fn discovery_models_reach_autopick_and_listing() {
        let api = FakeApi::start(json_response("{\"data\":[{\"id\":\"served\"}]}"));
        let cfg = config_with(&[("local", &api.base_url, &[])]);
        assert_eq!(
            resolve(&cfg, None, None, None).unwrap(),
            Selected {
                provider: "local".into(),
                model: "served".into()
            }
        );
        // The same path feeds --list-models.
        let (name, models) = list_for(&cfg, None).unwrap();
        assert_eq!(name, "local");
        assert_eq!(models, vec!["served".to_string()]);
    }

    #[test]
    fn listing_error_names_the_causes() {
        let api = FakeApi::start(
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        let cfg = config_with(&[("alpha", &api.base_url, &[])]);
        let err = list_for(&cfg, None).unwrap_err();
        assert!(err.contains("alpha"), "{err}");
        assert!(err.contains("500"), "{err}");
    }

    #[test]
    fn listing_surfaces_unrecognized_shapes() {
        let api = FakeApi::start(json_response("{\"surprise\":true}"));
        let cfg = config_with(&[("alpha", &api.base_url, &[])]);
        let err = list_for(&cfg, None).unwrap_err();
        assert!(err.contains("no recognizable"), "{err}");
    }

    #[test]
    fn saved_default_is_used_when_flags_are_silent() {
        let cfg = config_with(&[("zai", "http://z/v1", &["glm"])]);
        assert_eq!(
            resolve(&cfg, None, None, Some("zai/glm-4.6")).unwrap(),
            Selected {
                provider: "zai".into(),
                model: "glm-4.6".into()
            }
        );
    }

    #[test]
    fn flags_beat_the_saved_default() {
        let cfg = config_with(&[("a", "http://a/v1", &["a-model"])]);
        assert_eq!(
            resolve(&cfg, Some("a"), Some("anything"), Some("a/a-model")).unwrap(),
            Selected {
                provider: "a".into(),
                model: "anything".into()
            }
        );
    }

    #[test]
    fn stale_saved_default_falls_through_to_autopick() {
        let cfg = config_with(&[("alpha", "http://a/v1", &["a-model"])]);
        // "ghost" is not in the config anymore — autopick takes over.
        assert_eq!(
            resolve(&cfg, None, None, Some("ghost/old")).unwrap(),
            Selected {
                provider: "alpha".into(),
                model: "a-model".into()
            }
        );
    }

    #[test]
    fn malformed_saved_default_falls_through() {
        let cfg = config_with(&[("alpha", "http://a/v1", &["a-model"])]);
        assert_eq!(
            resolve(&cfg, None, None, Some("no-slash")).unwrap(),
            Selected {
                provider: "alpha".into(),
                model: "a-model".into()
            }
        );
    }
}

#[cfg(test)]
mod restore_tests {
    use super::model_available;
    use crate::config::ModelsConfig;

    fn models_config(models: &[&str]) -> ModelsConfig {
        let listed: Vec<String> = models
            .iter()
            .map(|id| format!("{{\"id\":\"{id}\"}}"))
            .collect();
        let raw = format!(
            r#"{{"providers":{{"fake":{{"baseUrl":"http://x/v1","models":[{}]}}}}}}"#,
            listed.join(",")
        );
        serde_json::from_str(&raw).unwrap()
    }

    #[test]
    fn model_available_follows_the_configured_list() {
        let config = models_config(&["a", "b"]);
        assert!(model_available(&config, "fake", "a"));
        assert!(!model_available(&config, "fake", "ghost"));
        assert!(!model_available(&config, "nope", "a"));
        // A discovery provider (no models list) accepts any id.
        let discovering = models_config(&[]);
        assert!(model_available(&discovering, "fake", "anything"));
    }
}
