//! Price recorded model selections through their profile's advertised catalog.
use super::pricing;
use crate::acp::SessionConfigChoice;
use crate::model_catalog;
use std::collections::BTreeMap;

/// The newest discovered model list of each profile.
#[derive(Default)]
pub struct ModelCatalogs(pub BTreeMap<String, Vec<SessionConfigChoice>>);

impl ModelCatalogs {
    /// The model id to price `model` as: itself when it has a rate, else the
    /// priced model its profile's catalog says it runs, else itself.
    pub fn priced_model(&self, profile: Option<&str>, model: String) -> String {
        if pricing::is_priced(&model) {
            return model;
        }
        profile
            .and_then(|profile| self.0.get(profile))
            .map(|choices| model_catalog::model_id(choices, &model))
            .map(|id| pricing::normalize_model(&id))
            .filter(|id| pricing::is_priced(id))
            .unwrap_or(model)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choice(value: &str, name: &str) -> SessionConfigChoice {
        SessionConfigChoice {
            value: value.into(),
            name: name.into(),
            description: None,
        }
    }

    /// claude2's discovered list on the live store, which offers both the
    /// `opus` alias and the older `claude-opus-5`.
    fn claude_catalog() -> ModelCatalogs {
        ModelCatalogs(BTreeMap::from([(
            "claude".to_owned(),
            vec![
                choice("default", "Default (recommended)"),
                choice("opus", "Opus 5.5"),
                choice("claude-fable-5-1[1m]", "Fable 5.1"),
                choice("sonnet", "Sonnet 5"),
                choice("haiku", "Haiku 4.5"),
                choice("claude-opus-5", "Opus 5"),
                choice("claude-fable-5", "Fable 5"),
            ],
        )]))
    }

    #[test]
    fn claude_aliases_resolve_to_the_model_the_catalog_names() {
        let catalogs = claude_catalog();
        let priced = |model: &str| catalogs.priced_model(Some("claude"), model.into());
        assert_eq!(priced("opus"), "claude-opus-5-5");
        assert_eq!(priced("fable"), "claude-fable-5-1");
        assert_eq!(priced("sonnet"), "claude-sonnet-5");
        assert_eq!(priced("haiku"), "claude-haiku-4-5");
        assert_eq!(priced("claude-opus-5"), "claude-opus-5");
        // `fable` is not listed; it runs the family's newest listed version.
        // Unresolvable values and unknown profiles stay as recorded.
        assert_eq!(priced("default"), "default");
        assert_eq!(priced("custom-model"), "custom-model");
        assert_eq!(catalogs.priced_model(None, "opus".into()), "opus");
        assert_eq!(catalogs.priced_model(Some("other"), "opus".into()), "opus");
    }
}
