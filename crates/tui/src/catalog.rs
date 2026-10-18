//! The set of models the `/model` picker can offer, gathered once at
//! startup (each provider bounded by the same 10s discovery timeout
//! as `--list-models`).

/// Provider name → model ids, in config order.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    pub entries: Vec<(String, Vec<String>)>,
}

impl Catalog {
    /// Gather the picker menu from a config — startup's policy, shared
    /// with `/reload` since AD-0054: configured model lists win in
    /// sorted provider order; providers that fail discovery or list
    /// nothing simply don't appear. Discovery hits each empty-list
    /// provider once, bounded by the same 10s timeout as startup.
    pub fn from_config(config: &hi_core::ModelsConfig) -> Catalog {
        Catalog {
            entries: config
                .providers
                .iter()
                .filter_map(|(name, entry)| {
                    entry
                        .list_model_ids()
                        .ok()
                        .filter(|models| !models.is_empty())
                        .map(|models| (name.clone(), models))
                })
                .collect(),
        }
    }

    /// Flat `provider/model` items for the picker list.
    pub fn items(&self) -> Vec<String> {
        self.entries
            .iter()
            .flat_map(|(provider, models)| {
                models
                    .iter()
                    .map(move |model| format!("{provider}/{model}"))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn items_flatten_provider_and_model() {
        let catalog = Catalog {
            entries: vec![
                ("zai".into(), vec!["glm-5.3".into(), "glm-4.6".into()]),
                ("local".into(), vec!["llama".into()]),
            ],
        };
        assert_eq!(
            catalog.items(),
            vec!["zai/glm-5.3", "zai/glm-4.6", "local/llama"]
        );
    }

    #[test]
    fn empty_catalog_yields_no_items() {
        assert!(Catalog::default().items().is_empty());
    }

    #[test]
    fn from_config_follows_the_configured_lists() {
        // Same gather as startup (AD-0054): configured lists win in
        // sorted provider order; only providers that list models appear.
        let config: hi_core::ModelsConfig = serde_json::from_str(
            r#"{"providers":{
                "b":{"baseUrl":"http://b/v1","models":[{"id":"x"}]},
                "a":{"baseUrl":"http://a/v1","models":[{"id":"m2"},{"id":"m1"}]}
            }}"#,
        )
        .unwrap();
        let catalog = Catalog::from_config(&config);
        assert_eq!(catalog.items(), ["a/m2", "a/m1", "b/x"]);
    }
}
