//! The set of models the `/model` picker can offer, gathered once at
//! startup (each provider bounded by the same 10s discovery timeout
//! as `--list-models`).

/// Provider name → model ids, in config order.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    pub entries: Vec<(String, Vec<String>)>,
}

impl Catalog {
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
}
