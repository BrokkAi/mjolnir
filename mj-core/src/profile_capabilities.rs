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
    /// None means required hydration is still in flight, including effort
    /// discovery. Successful empty choices and failures are terminal states.
    pub fn options(
        &self,
        config: &Config,
        parent: &str,
        model: Option<&str>,
    ) -> Option<SubagentOptions> {
        let mut options = SubagentOptions::default();
        for (id, profile) in config
            .enabled_profiles()
            .filter(|(id, _)| config.subagents.profile_is_eligible(parent, id))
        {
            let entry = self.profiles.get(&profile.capabilities_key(id))?;
            match &entry.choices {
                CapabilityState::Pending => return None,
                CapabilityState::Failed(error) => {
                    options.unavailable.push(format!("{id}: {error}"))
                }
                CapabilityState::Ready(choices) => {
                    merge_choices(&mut options.models, &choices.models);
                    if let Some(model) = model
                        .filter(|model| choices.models.iter().any(|choice| choice.value == *model))
                    {
                        match entry.efforts.get(model)? {
                            CapabilityState::Pending => return None,
                            CapabilityState::Ready(efforts) => {
                                merge_choices(&mut options.efforts, efforts)
                            }
                            CapabilityState::Failed(error) => {
                                options.unavailable.push(format!("{id}: {error}"))
                            }
                        }
                    }
                }
            }
        }
        Some(options)
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
            enabled: true,
            kind: HarnessKind::Codex,
            home: "/profiles/parent".into(),
            environment: Default::default(),
            context_window_bytes: None,
            guardian_review_model: None,
            subagents: Default::default(),
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
    fn models_remain_ready_while_another_models_efforts_hydrate() {
        let (config, mut snapshot) = fixture();
        assert_eq!(
            snapshot
                .options(&config, "parent", None)
                .unwrap()
                .models
                .len(),
            2
        );
        assert!(
            snapshot
                .options(&config, "parent", Some("a"))
                .unwrap()
                .efforts
                .is_empty()
        );
        assert!(snapshot.options(&config, "parent", Some("b")).is_none());
        snapshot
            .profiles
            .values_mut()
            .next()
            .unwrap()
            .efforts
            .insert(
                "b".into(),
                CapabilityState::Failed("probe unavailable".into()),
            );
        assert!(
            snapshot
                .options(&config, "parent", Some("b"))
                .unwrap()
                .unavailable[0]
                .contains("probe unavailable")
        );
    }

    #[test]
    fn defaults_and_eligibility_change_views_without_changing_profile_identity() {
        let (mut config, snapshot) = fixture();
        let original = config.profiles_discovery_key();
        config.profiles.get_mut("parent").unwrap().subagents =
            crate::subagent::SubagentPolicy::SingleModel {
                model: "a".into(),
                effort: None,
            };
        config.subagents.max_concurrent = 99;
        config
            .profiles
            .get_mut("parent")
            .unwrap()
            .context_window_bytes = Some(42);
        assert_eq!(config.profiles_discovery_key(), original);
        assert!(snapshot.options(&config, "parent", Some("a")).is_some());
        config.profiles.get_mut("parent").unwrap().home = "/another/home".into();
        assert_ne!(config.profiles_discovery_key(), original);
        assert!(snapshot.options(&config, "parent", None).is_none());
    }
}
