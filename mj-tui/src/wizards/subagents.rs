use super::*;
use mj_core::subagent::{SubagentOptions, SubagentPolicy};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SubagentDiscovery {
    pub result: Option<Result<SubagentOptions, String>>,
}

/// The delegation choice on a review step: the policy being drafted, its
/// open combo box, and global configuration availability. Move initializes it
/// from the session record.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SubagentDraft {
    pub(crate) policy: SubagentPolicy,
    chosen_policy: Option<Box<SubagentPolicy>>,
    last_mjolnir_policy: Option<Box<SubagentPolicy>>,
    pub(crate) combo: ComboBoxState<WizardControl>,
    pub(crate) discovery: Option<SubagentDiscovery>,
}

impl SubagentDraft {
    pub(crate) fn new(policy: SubagentPolicy) -> Self {
        Self {
            last_mjolnir_policy: policy.uses_mjolnir().then(|| Box::new(policy.clone())),
            policy: if policy.uses_mjolnir() {
                policy
            } else {
                SubagentPolicy::Native
            },
            ..Self::default()
        }
    }

    pub(crate) fn policies(&self) -> Vec<String> {
        let remembered = self.last_mjolnir_policy.as_deref().unwrap_or(&self.policy);
        let label = |name: &str, choice: Option<&mj_core::config::SubagentChoice>| {
            choice.map(|choice| {
                let effort = choice
                    .effort
                    .as_deref()
                    .map(|effort| format!(", {effort}"))
                    .unwrap_or_default();
                format!("{name}: {}{effort}", choice.model)
            })
        };
        let overrides = [
            label(
                "straightforward",
                remembered.override_choice(mj_core::subagent::SubagentTaskType::Straightforward),
            ),
            label(
                "subtle",
                remembered.override_choice(mj_core::subagent::SubagentTaskType::Subtle),
            ),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("; ");
        let mjolnir = if overrides.is_empty() {
            "Mjolnir".into()
        } else {
            format!("Mjolnir ({overrides})")
        };
        vec!["Native".into(), mjolnir]
    }
    pub(crate) fn policy_index(&self) -> usize {
        usize::from(self.policy.uses_mjolnir())
    }
    pub(crate) fn unavailable_reason(&self) -> Option<&str> {
        self.discovery
            .as_ref()?
            .result
            .as_ref()?
            .as_ref()
            .err()
            .map(String::as_str)
    }
    pub(crate) fn error(&self) -> Option<String> {
        self.policy
            .uses_mjolnir()
            .then(|| {
                self.discovery
                    .as_ref()?
                    .result
                    .as_ref()?
                    .as_ref()
                    .err()
                    .cloned()
            })
            .flatten()
    }
    pub(crate) fn select_policy(&mut self, index: usize) -> bool {
        if index == 1 && self.unavailable_reason().is_some() {
            return false;
        }
        let policy = if index == 1 {
            self.last_mjolnir_policy
                .as_deref()
                .cloned()
                .unwrap_or(SubagentPolicy::Mjolnir {
                    straightforward: None,
                    subtle: None,
                })
        } else {
            SubagentPolicy::Native
        };
        let changed = self.policy != policy;
        self.chosen_policy = Some(Box::new(policy.clone()));
        if policy.uses_mjolnir() {
            self.last_mjolnir_policy = Some(Box::new(policy.clone()));
        }
        self.policy = policy;
        changed
    }
    pub(crate) fn activate(&mut self, id: WizardControl) -> bool {
        if id != WizardControl::Subagents {
            return false;
        }
        self.combo.open(id, self.policy_index());
        true
    }
    pub(crate) fn apply(&mut self, interaction: &Interaction<WizardControl>) -> Option<bool> {
        match interaction {
            Interaction::ComboBoxCommit(WizardControl::Subagents, selected) => {
                Some(self.select_policy(*selected))
            }
            _ => None,
        }
    }
    pub(crate) fn declare(&self, form: &mut Dialog<WizardControl>) {
        form.declare_with_enabled(
            WizardControl::Subagents,
            ControlKind::ComboBox {
                len: self.policies().len(),
                selected: self
                    .combo
                    .selection(WizardControl::Subagents, self.policy_index()),
                expanded: self.combo.is_open(WizardControl::Subagents),
            },
            true,
        );
        form.set_row_enabled(
            WizardControl::Subagents,
            vec![true, self.unavailable_reason().is_none()],
        );
    }
    pub(crate) fn refresh_create(
        &mut self,
        profile: &str,
        config: &Config,
        snapshot: &mj_core::profile_capabilities::ProfileCapabilitiesSnapshot,
        remembered: &SubagentPolicy,
    ) {
        self.update_choices(profile, config, snapshot);
        // Keep the user's intent separate from the available displayed policy.
        let desired = self
            .chosen_policy
            .as_deref()
            .cloned()
            .unwrap_or_else(|| remembered.clone());
        let policy = if desired.uses_mjolnir() && self.unavailable_reason().is_none() {
            desired.clone()
        } else {
            SubagentPolicy::Native
        };
        if self.policy != policy {
            self.policy = policy;
            self.combo = ComboBoxState::default();
        }
        if desired.uses_mjolnir() {
            self.last_mjolnir_policy = Some(Box::new(desired));
        }
    }

    pub(crate) fn update_choices(
        &mut self,
        profile: &str,
        config: &Config,
        _snapshot: &mj_core::profile_capabilities::ProfileCapabilitiesSnapshot,
    ) {
        self.discovery = Some(SubagentDiscovery {
            result: Some(
                if !config
                    .profiles
                    .get(profile)
                    .is_some_and(|p| p.kind.supports_delegation_tools())
                {
                    Err("Mjolnir sub-agents need a Claude or Codex profile".into())
                } else if (self
                    .last_mjolnir_policy
                    .as_deref()
                    .unwrap_or(&self.policy)
                    .override_choice(mj_core::subagent::SubagentTaskType::Straightforward)
                    .is_some()
                    || config.subagents.straightforward.is_some())
                    && (self
                        .last_mjolnir_policy
                        .as_deref()
                        .unwrap_or(&self.policy)
                        .override_choice(mj_core::subagent::SubagentTaskType::Subtle)
                        .is_some()
                        || config.subagents.subtle.is_some())
                {
                    Ok(SubagentOptions::default())
                } else {
                    Err("Set up Mjolnir sub-agents in Settings -> Sub-agents".into())
                },
            ),
        });
    }
}

impl DashboardState {
    /// The review step's delegation draft and the profile it is for, when
    /// the open wizard offers the choice.
    fn review_subagent_draft(&mut self) -> Option<(&mut SubagentDraft, String)> {
        let applies = matches!(&self.mode, Mode::New(_))
            || matches!(&self.mode, Mode::Resume(wizard) if wizard.step == WizardStep::Review && wizard.subagent_choice_applies(self));
        if !applies {
            return None;
        }
        let profile = match &self.mode {
            Mode::New(wizard) => self
                .config
                .enabled_profiles()
                .nth(wizard.profile)?
                .0
                .to_owned(),
            Mode::Resume(wizard) => wizard.destination_profile(self),
            _ => unreachable!("checked above"),
        };
        match &mut self.mode {
            Mode::New(wizard) => Some((&mut *wizard.subagents, profile)),
            Mode::Resume(wizard) => Some((&mut *wizard.subagents, profile)),
            _ => None,
        }
    }

    pub(crate) fn take_subagent_discovery(&mut self) -> Option<DashboardAction> {
        let creating = matches!(&self.mode, Mode::New(_));
        let remembered = self.state.last_subagent_policy.clone();
        let config = self.config.clone();
        let snapshot = self.profile_capabilities.clone();
        let (draft, profile) = self.review_subagent_draft()?;
        if creating {
            draft.refresh_create(&profile, &config, &snapshot, &remembered);
        } else {
            draft.update_choices(&profile, &config, &snapshot);
        }
        None
    }
}

impl ResumeWizard {
    /// The displayed effective policy for comparison with a prepared Move.
    pub(crate) fn subagent_selection(&self, dashboard: &DashboardState) -> Option<SubagentPolicy> {
        self.subagent_choice_applies(dashboard)
            .then(|| self.subagents.policy.clone())
    }

    /// Move always shows delegation, including the reason Mjolnir is unavailable.
    /// A plain resume keeps the session's own policy.
    pub(crate) fn subagent_choice_applies(&self, dashboard: &DashboardState) -> bool {
        let _ = dashboard;
        self.moving
    }

    /// The policy a Move request carries: `None` keeps the session's own, so
    /// an untouched choice changes nothing.
    pub(crate) fn subagent_change(&self, dashboard: &DashboardState) -> Option<SubagentPolicy> {
        let stored = dashboard
            .session_record(&self.session_id)
            .and_then(|session| session.subagents.clone())
            .unwrap_or_default();
        let displayed = if stored.uses_mjolnir() {
            stored
        } else {
            SubagentPolicy::Native
        };
        (self.subagent_choice_applies(dashboard) && self.subagents.policy != displayed)
            .then(|| self.subagents.policy.clone())
    }
}
