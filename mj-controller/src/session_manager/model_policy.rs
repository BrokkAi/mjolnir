use super::*;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SessionModelPolicy {
    pub profile: String,
    pub models: Vec<String>,
}

// Live sessions exempt their current model; catalog probes exempt the default.
impl SessionModelPolicy {
    fn offered(&self, state: &RelayOperationalState) -> Vec<mj_core::acp::SessionConfigChoice> {
        mj_core::worker_launch::ProfileConfig {
            model: mj_core::acp::surface::config_current_value(&state.config_options, "model"),
            models: mj_core::acp::session_config_choices(&state.config_options, "model"),
            efforts: Vec::new(),
            observed_at: 0,
        }
        .restricted_to(&self.models)
        .models
    }

    pub(super) fn filter(&self, state: &mut RelayOperationalState) {
        use agent_client_protocol::v1::schema::{SessionConfigKind, SessionConfigSelectOptions};
        if self.models.is_empty() {
            return;
        }
        let offered = self.offered(state);
        let Some(id) = mj_core::acp::find_session_config_option(&state.config_options, "model")
            .map(|option| option.id.clone())
        else {
            return;
        };
        let Some(option) = state
            .config_options
            .iter_mut()
            .find(|option| option.id == id)
        else {
            return;
        };
        let SessionConfigKind::Select(select) = &mut option.kind else {
            return;
        };
        let keep = |value: &str| offered.iter().any(|choice| choice.value == value);
        match &mut select.options {
            SessionConfigSelectOptions::Ungrouped(options) => {
                options.retain(|option| keep(&option.value.to_string()))
            }
            SessionConfigSelectOptions::Grouped(groups) => {
                for group in groups.iter_mut() {
                    group
                        .options
                        .retain(|option| keep(&option.value.to_string()));
                }
                groups.retain(|group| !group.options.is_empty());
            }
            _ => {}
        }
    }

    pub(super) fn refusal(
        &self,
        command: &RelayCommand,
        state: Option<&RelayOperationalState>,
    ) -> Option<String> {
        let RelayCommand::SetConfig { key, value } = command else {
            return None;
        };
        let is_model = key == "model"
            || state.is_some_and(|state| {
                mj_core::acp::find_session_config_option(&state.config_options, "model")
                    .is_some_and(|option| option.id.to_string() == *key)
            });
        if !is_model
            || self.models.is_empty()
            || self.models.contains(value)
            || state.is_some_and(|state| {
                self.offered(state)
                    .iter()
                    .any(|choice| choice.value == *value)
            })
        {
            return None;
        }
        Some(format!(
            "model {value:?} is hidden by profile {:?}'s models setting",
            self.profile
        ))
    }
}

impl SessionManagerControl {
    pub(crate) fn update_model_policies(&self, controller: &crate::controller::Controller) {
        let policies = controller
            .state
            .sessions
            .iter()
            .map(|(id, session)| {
                (
                    id.clone(),
                    SessionModelPolicy {
                        profile: session.last_profile.clone(),
                        models: controller
                            .config
                            .profiles
                            .get(&session.last_profile)
                            .map(|profile| profile.models.clone())
                            .unwrap_or_default(),
                    },
                )
            })
            .collect();
        self.model_policies.send_if_modified(|current| {
            if *current == policies {
                false
            } else {
                *current = policies;
                true
            }
        });
    }
}
