//! Read-only publications from the daemon's single capability cache.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::acp::SessionConfigChoice;
use crate::config::Config;
use crate::subagent::SubagentOptions;
use crate::worker_launch::ProfileConfig;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", content = "value", rename_all = "snake_case")]
pub enum CapabilityState<T> {
    #[default]
    Pending,
    Ready(T),
    Failed(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileCapabilities {
    pub choices: CapabilityState<ProfileConfig>,
    pub efforts: BTreeMap<String, CapabilityState<Vec<SessionConfigChoice>>>,
}

/// Entries are keyed by opaque configured-installation identities. This also
/// lets unsaved drafts share entries without changing the live profile list.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileCapabilitiesSnapshot {
    pub profiles: BTreeMap<String, ProfileCapabilities>,
}

impl ProfileCapabilitiesSnapshot {
    /// Completed profiles contribute choices while pending profiles remain
    /// unknown. None means no profile has completed discovery yet.
    pub fn options(&self, config: &Config, model: Option<&str>) -> Option<SubagentOptions> {
        let mut options = SubagentOptions::default();
        let mut completed = false;
        for (id, profile) in config.enabled_profiles() {
            let Some(entry) = self.profiles.get(&profile.capabilities_key(id)) else {
                options.unavailable.push(format!("{id}: discovery pending"));
                continue;
            };
            match &entry.choices {
                CapabilityState::Pending => {
                    options.unavailable.push(format!("{id}: discovery pending"))
                }
                CapabilityState::Failed(error) => {
                    completed = true;
                    options.unavailable.push(format!("{id}: {error}"))
                }
                CapabilityState::Ready(choices) => {
                    completed = true;
                    merge_choices(&mut options.models, &choices.models);
                    if let Some(model) = model
                        .filter(|model| choices.models.iter().any(|choice| choice.value == *model))
                    {
                        match entry.efforts.get(model) {
                            None | Some(CapabilityState::Pending) => options
                                .unavailable
                                .push(format!("{id}: effort discovery pending")),
                            Some(CapabilityState::Ready(efforts)) => {
                                merge_choices(&mut options.efforts, efforts)
                            }
                            Some(CapabilityState::Failed(error)) => {
                                options.unavailable.push(format!("{id}: {error}"))
                            }
                        }
                    }
                }
            }
        }
        completed.then_some(options)
    }

    /// The models completed enabled profiles offer, one offer per value, with
    /// `usage_priced` saying which profiles are billed per use.
    pub fn model_offers(
        &self,
        config: &Config,
        usage_priced: impl Fn(&str) -> bool,
    ) -> Vec<crate::review::autoconfigure::ModelOffer> {
        let ready = config
            .enabled_profiles()
            .filter_map(|(id, profile)| {
                match &self.profiles.get(&profile.capabilities_key(id))?.choices {
                    CapabilityState::Ready(choices) => {
                        Some((choices.models.as_slice(), usage_priced(id)))
                    }
                    _ => None,
                }
            })
            .collect::<Vec<_>>();
        crate::review::autoconfigure::model_offers(ready)
    }

    /// Why `model`'s efforts are not fully known: a profile that has not
    /// finished discovery (it may offer the model), or one that offers the
    /// model whose effort discovery is pending or failed. A profile whose own
    /// discovery failed is not listed: nothing shows that it offers the model.
    pub fn effort_blockers(&self, config: &Config, model: &str) -> Vec<String> {
        let mut blockers = Vec::new();
        for (id, profile) in config.enabled_profiles() {
            let Some(entry) = self.profiles.get(&profile.capabilities_key(id)) else {
                blockers.push(format!("{id}: discovery pending"));
                continue;
            };
            match &entry.choices {
                CapabilityState::Pending => blockers.push(format!("{id}: discovery pending")),
                CapabilityState::Failed(_) => {}
                CapabilityState::Ready(choices) => {
                    if choices.models.iter().any(|choice| choice.value == model) {
                        match entry.efforts.get(model) {
                            None | Some(CapabilityState::Pending) => {
                                blockers.push(format!("{id}: effort discovery pending"))
                            }
                            Some(CapabilityState::Ready(_)) => {}
                            Some(CapabilityState::Failed(error)) => {
                                blockers.push(format!("{id}: {error}"))
                            }
                        }
                    }
                }
            }
        }
        blockers
    }

    /// Limit a public projection to saved enabled profiles. Draft identities
    /// are controller-private; environment sources are never published.
    pub fn for_config(&self, config: &Config) -> Self {
        Self {
            profiles: config
                .enabled_profiles()
                .filter_map(|(id, profile)| {
                    let key = profile.capabilities_key(id);
                    self.profiles.get(&key).cloned().map(|entry| (key, entry))
                })
                .collect(),
        }
    }
}

fn merge_choices(into: &mut Vec<SessionConfigChoice>, choices: &[SessionConfigChoice]) {
    for choice in choices {
        if !into.iter().any(|existing| existing.value == choice.value) {
            into.push(choice.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{HarnessKind, HarnessProfile};

    fn fixture() -> (Config, ProfileCapabilitiesSnapshot) {
        let profile = HarnessProfile {
            ..HarnessProfile::new(HarnessKind::Codex, "/profiles/parent")
        };
        let choice = |value: &str| SessionConfigChoice {
            value: value.into(),
            name: value.into(),
            description: None,
        };
        let key = profile.capabilities_key("parent");
        let config = Config {
            profiles: BTreeMap::from([("parent".into(), profile)]),
            ..Default::default()
        };
        let snapshot = ProfileCapabilitiesSnapshot {
            profiles: BTreeMap::from([(
                key,
                ProfileCapabilities {
                    choices: CapabilityState::Ready(ProfileConfig {
                        model: Some("a".into()),
                        models: vec![choice("a"), choice("b")],
                        efforts: vec![],
                        observed_at: 1,
                    }),
                    efforts: BTreeMap::from([
                        ("a".into(), CapabilityState::Ready(vec![])),
                        ("b".into(), CapabilityState::Pending),
                    ]),
                },
            )]),
        };
        (config, snapshot)
    }

    #[test]
    fn defaults_and_eligibility_change_views_without_changing_profile_identity() {
        let (mut config, snapshot) = fixture();
        let original = config.profiles_discovery_key();

        config.subagents.max_concurrent = 99;
        config
            .profiles
            .get_mut("parent")
            .unwrap()
            .context_window_bytes = Some(42);
        assert_eq!(config.profiles_discovery_key(), original);
        assert!(snapshot.options(&config, Some("a")).is_some());
        config.profiles.get_mut("parent").unwrap().home = "/another/home".into();
        assert_ne!(config.profiles_discovery_key(), original);
        assert!(snapshot.options(&config, None).is_none());
    }

    #[test]
    fn completed_profile_remains_usable_while_another_profile_is_pending() {
        let (mut config, snapshot) = fixture();
        let second = config.profiles["parent"].clone();
        config.profiles.insert("other".into(), second);
        let models = snapshot.options(&config, None).unwrap();
        assert!(models.models.iter().any(|choice| choice.value == "a"));
        assert!(
            models
                .unavailable
                .iter()
                .any(|reason| reason.contains("other: discovery pending"))
        );
        let efforts = snapshot.options(&config, Some("a")).unwrap();
        assert!(
            efforts
                .unavailable
                .iter()
                .any(|reason| reason.contains("other: discovery pending"))
        );
    }
}
