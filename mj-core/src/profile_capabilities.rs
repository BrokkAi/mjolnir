//! Read-only publications from the daemon's single capability cache.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::acp::SessionConfigChoice;
use crate::config::Config;
use crate::model_catalog;
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
        let identity = model.map(|model| self.model_id(config, model));
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
                    options.models.extend(choices.models.iter().cloned());
                    if let Some(offer) = identity.as_deref().and_then(|identity| {
                        model_catalog::offer(&choices.models, identity, model.unwrap_or(identity))
                    }) {
                        match entry.efforts.get(&offer.value) {
                            None | Some(CapabilityState::Pending) => options
                                .unavailable
                                .push(format!("{id}: effort discovery pending")),
                            Some(CapabilityState::Ready(efforts)) => {
                                merge_efforts(&mut options.efforts, efforts)
                            }
                            Some(CapabilityState::Failed(error)) => {
                                options.unavailable.push(format!("{id}: {error}"))
                            }
                        }
                    }
                }
            }
        }
        options.models = model_catalog::merged_choices(options.models);
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
        let identity = self.model_id(config, model);
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
                    if let Some(offer) = model_catalog::offer(&choices.models, &identity, model) {
                        match entry.efforts.get(&offer.value) {
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

    /// Resolve once over the ready enabled profiles in this publication.
    pub fn model_id(&self, config: &Config, wanted: &str) -> String {
        let models = config
            .enabled_profiles()
            .filter_map(|(id, profile)| {
                match &self.profiles.get(&profile.capabilities_key(id))?.choices {
                    CapabilityState::Ready(choices) => Some(choices.models.iter().cloned()),
                    _ => None,
                }
            })
            .flatten()
            .collect::<Vec<_>>();
        model_catalog::model_id(&models, wanted)
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

fn merge_efforts(into: &mut Vec<SessionConfigChoice>, choices: &[SessionConfigChoice]) {
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

    // Hard-won: #1534: settings ignored equivalent harness IDs when collecting efforts.
    #[test]
    fn alias_efforts_use_profile_launch_keys_and_exclude_stale_versions() {
        let recorded: BTreeMap<String, Vec<SessionConfigChoice>> = serde_json::from_str(
            include_str!("../tests/fixtures/claude-antigravity-models.json"),
        )
        .unwrap();
        let mut config = Config::default();
        let mut snapshot = ProfileCapabilitiesSnapshot::default();
        for (id, kind, mut models, launch, state) in [
            (
                "claude",
                HarnessKind::Claude,
                recorded["claude"].clone(),
                "sonnet",
                CapabilityState::Pending,
            ),
            (
                "gemini",
                HarnessKind::Antigravity,
                recorded["antigravity"].clone(),
                "claude-sonnet-5-5",
                CapabilityState::Ready(vec![SessionConfigChoice {
                    value: "high".into(),
                    name: "High".into(),
                    description: None,
                }]),
            ),
            (
                "a-stale",
                HarnessKind::Claude,
                recorded["claude"].clone(),
                "sonnet",
                CapabilityState::Pending,
            ),
        ] {
            if id == "a-stale" {
                models[1].name = "Sonnet 5".into();
            }
            let profile = HarnessProfile::new(kind, format!("/profiles/{id}"));
            snapshot.profiles.insert(
                profile.capabilities_key(id),
                ProfileCapabilities {
                    choices: CapabilityState::Ready(ProfileConfig {
                        model: None,
                        models,
                        efforts: vec![],
                        observed_at: 0,
                    }),
                    efforts: BTreeMap::from([(launch.into(), state)]),
                },
            );
            config.profiles.insert(id.into(), profile);
        }
        assert_eq!(snapshot.model_id(&config, "sonnet"), "claude-sonnet-5-5");
        let options = snapshot.options(&config, Some("sonnet")).unwrap();
        options
            .validate(&crate::config::SubagentChoice {
                model: "sonnet".into(),
                effort: Some("high".into()),
            })
            .unwrap();
        assert_eq!(
            snapshot.effort_blockers(&config, "sonnet"),
            ["claude: effort discovery pending"]
        );
        config.profiles.get_mut("claude").unwrap().enabled = false;
        config.profiles.get_mut("a-stale").unwrap().enabled = false;
        let options = snapshot.options(&config, Some("sonnet")).unwrap();
        options
            .validate(&crate::config::SubagentChoice {
                model: "sonnet".into(),
                effort: Some("high".into()),
            })
            .unwrap();
        // The older alias sorts first; without identity-preserving merging
        // it would hide the newer profile when Gemini is disabled.
        config.profiles.get_mut("claude").unwrap().enabled = true;
        config.profiles.get_mut("a-stale").unwrap().enabled = true;
        config.profiles.get_mut("gemini").unwrap().enabled = false;
        let options = snapshot
            .options(&config, Some("claude-sonnet-5-5"))
            .unwrap();
        options
            .validate(&crate::config::SubagentChoice {
                model: "claude-sonnet-5-5".into(),
                effort: None,
            })
            .unwrap();
        assert_eq!(
            model_catalog::model_id(&options.models, "sonnet"),
            "claude-sonnet-5-5"
        );
    }

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
