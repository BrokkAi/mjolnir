use super::*;
use mj_core::subagent::{SubagentOptions, SubagentPolicy};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SubagentDiscovery {
    pub result: Option<Result<SubagentOptions, String>>,
}

/// The delegation choice on a review step: the policy being drafted, its
/// open combo box, and the model discovery a single-model policy needs. Move carries one initialized from its session record.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SubagentDraft {
    pub(crate) policy: SubagentPolicy,
    pub(crate) combo: ComboBoxState<WizardControl>,
    pub(crate) discovery: Option<SubagentDiscovery>,
    models: Vec<mj_core::acp::SessionConfigChoice>,
}

impl SubagentDraft {
    pub(crate) fn new(policy: SubagentPolicy) -> Self {
        Self {
            policy,
            ..Self::default()
        }
    }

    pub(crate) fn policies(&self) -> Vec<String> {
        let mut labels = vec![
            "Native".into(),
            "Mjolnir, single model".into(),
            "None".into(),
        ];
        if self.policy == SubagentPolicy::AllModels {
            labels.push("Mjolnir, all models (legacy)".into());
        }
        labels
    }

    pub(crate) fn policy_index(&self) -> usize {
        match self.policy {
            SubagentPolicy::Native => 0,
            SubagentPolicy::SingleModel { .. } => 1,
            SubagentPolicy::None => 2,
            SubagentPolicy::AllModels => 3,
        }
    }

    pub(crate) fn options(&self) -> Option<&SubagentOptions> {
        self.discovery.as_ref()?.result.as_ref()?.as_ref().ok()
    }

    pub(crate) fn error(&self) -> Option<String> {
        if !matches!(self.policy, SubagentPolicy::SingleModel { .. }) {
            return None;
        }
        match self
            .discovery
            .as_ref()
            .and_then(|discovery| discovery.result.as_ref())
        {
            None => Some("Loading eligible models and efforts…".into()),
            Some(Err(error)) => Some(error.clone()),
            Some(Ok(options)) => options.validate(&self.policy).err(),
        }
    }

    fn model_values(&self) -> Vec<Option<String>> {
        let model = match &self.policy {
            SubagentPolicy::SingleModel { model, .. } => {
                (!model.is_empty()).then_some(model.as_str())
            }
            _ => None,
        };
        crate::widgets::config_choice_values(model, &self.models)
    }

    fn effort_values(&self) -> Vec<Option<String>> {
        let effort = match &self.policy {
            SubagentPolicy::SingleModel { effort, .. } => effort.as_deref(),
            _ => None,
        };
        crate::widgets::subagent_effort_choice_values(
            effort,
            self.options().map_or(&[], |o| &o.efforts),
        )
    }

    pub(crate) fn models_ready(&self) -> bool {
        !self.models.is_empty()
    }

    pub(crate) fn models(&self) -> Vec<String> {
        self.model_values()
            .into_iter()
            .map(|value| match value {
                None => "Select model".into(),
                Some(value) => crate::widgets::config_choice_label(
                    Some(&value),
                    &self.models,
                    self.options().is_some(),
                ),
            })
            .collect()
    }

    pub(crate) fn efforts(&self) -> Vec<String> {
        let offered = self.options().map_or(&[][..], |o| o.efforts.as_slice());
        self.effort_values()
            .into_iter()
            .map(|value| match value {
                None => if self.options().is_none() {
                    "Loading efforts…"
                } else if offered.is_empty() {
                    "Harness default"
                } else {
                    "Select effort"
                }
                .into(),
                Some(value) => crate::widgets::config_choice_label(
                    Some(&value),
                    offered,
                    self.options().is_some(),
                ),
            })
            .collect()
    }

    pub(crate) fn model_index(&self) -> usize {
        let SubagentPolicy::SingleModel { model, .. } = &self.policy else {
            return 0;
        };
        self.model_values()
            .iter()
            .position(|value| value.as_deref() == Some(model))
            .unwrap_or(0)
    }

    pub(crate) fn effort_index(&self) -> usize {
        let SubagentPolicy::SingleModel { effort, .. } = &self.policy else {
            return 0;
        };
        self.effort_values()
            .iter()
            .position(|value| value.as_deref() == effort.as_deref())
            .unwrap_or(0)
    }

    /// Commits the policy row of the combo box. Returns whether it changed.
    pub(crate) fn select_policy(&mut self, index: usize) -> bool {
        if self.policy_index() == index {
            return false;
        }
        self.policy = match index {
            1 => SubagentPolicy::SingleModel {
                model: String::new(),
                effort: None,
            },
            2 => SubagentPolicy::None,
            _ => SubagentPolicy::Native,
        };
        self.discovery = None;
        true
    }

    /// Returns whether the model changed.
    pub(crate) fn select_model(&mut self, index: usize) -> bool {
        let model = self
            .model_values()
            .get(index)
            .cloned()
            .flatten()
            .unwrap_or_default();
        if matches!(&self.policy, SubagentPolicy::SingleModel { model: selected, .. } if selected == &model)
        {
            return false;
        }
        self.policy = SubagentPolicy::SingleModel {
            model,
            effort: None,
        };
        self.discovery = None;
        true
    }

    /// Returns whether the effort changed.
    pub(crate) fn select_effort(&mut self, index: usize) -> bool {
        let effort = self.effort_values().get(index).cloned().flatten();
        match &mut self.policy {
            SubagentPolicy::SingleModel {
                effort: selected, ..
            } if *selected != effort => {
                *selected = effort;
                true
            }
            _ => false,
        }
    }

    /// Opens the combo box behind `id`. Returns false for another control.
    pub(crate) fn activate(&mut self, id: WizardControl) -> bool {
        let selected = match id {
            WizardControl::Subagents => self.policy_index(),
            WizardControl::SubagentModel => self.model_index(),
            WizardControl::SubagentEffort => self.effort_index(),
            _ => return false,
        };
        self.combo.open(id, selected);
        true
    }

    /// Applies a committed combo box row. Returns `None` for an interaction
    /// that is not one of these controls, otherwise whether the policy
    /// changed.
    pub(crate) fn apply(&mut self, interaction: &Interaction<WizardControl>) -> Option<bool> {
        match interaction {
            Interaction::ComboBoxCommit(WizardControl::Subagents, selected) => {
                Some(self.select_policy(*selected))
            }
            Interaction::ComboBoxCommit(WizardControl::SubagentModel, selected) => {
                Some(self.select_model(*selected))
            }
            Interaction::ComboBoxCommit(WizardControl::SubagentEffort, selected) => {
                Some(self.select_effort(*selected))
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
        if matches!(self.policy, SubagentPolicy::SingleModel { .. }) {
            form.declare_with_enabled(
                WizardControl::SubagentModel,
                ControlKind::ComboBox {
                    len: self.models().len(),
                    selected: self
                        .combo
                        .selection(WizardControl::SubagentModel, self.model_index()),
                    expanded: self.combo.is_open(WizardControl::SubagentModel),
                },
                self.models_ready(),
            );
            form.declare_with_enabled(
                WizardControl::SubagentEffort,
                ControlKind::ComboBox {
                    len: self.efforts().len(),
                    selected: self
                        .combo
                        .selection(WizardControl::SubagentEffort, self.effort_index()),
                    expanded: self.combo.is_open(WizardControl::SubagentEffort),
                },
                self.options().is_some(),
            );
        }
    }

    fn update_choices(
        &mut self,
        profile: &str,
        config: &Config,
        snapshot: &mj_core::profile_capabilities::ProfileCapabilitiesSnapshot,
    ) {
        let SubagentPolicy::SingleModel { model, .. } = &self.policy else {
            return;
        };
        let model = (!model.is_empty()).then_some(model.as_str());
        self.models = snapshot
            .options(config, profile, None)
            .map(|options| options.models)
            .unwrap_or_default();
        self.discovery = Some(SubagentDiscovery {
            result: snapshot.options(config, profile, model).map(Ok),
        });
    }
}

impl DashboardState {
    /// The review step's delegation draft and the profile it is for, when
    /// the open wizard offers the choice.
    fn review_subagent_draft(&mut self) -> Option<(&mut SubagentDraft, String)> {
        let applies = matches!(&self.mode, Mode::Resume(wizard) if wizard.step == WizardStep::Review && wizard.subagent_choice_applies(self));
        if !applies {
            return None;
        }
        let profile = match &self.mode {
            Mode::Resume(wizard) => wizard.destination_profile(self),
            _ => unreachable!("checked above"),
        };
        match &mut self.mode {
            Mode::Resume(wizard) => Some((&mut *wizard.subagents, profile)),
            _ => None,
        }
    }

    pub(crate) fn take_subagent_discovery(&mut self) -> Option<DashboardAction> {
        let config = self.config.clone();
        let snapshot = self.profile_capabilities.clone();
        let (draft, profile) = self.review_subagent_draft()?;
        draft.update_choices(&profile, &config, &snapshot);
        None
    }
}

impl ResumeWizard {
    /// The draft owns the displayed choice. Send it explicitly rather than
    /// deriving a delta from a session snapshot that can be stale or absent.
    pub(crate) fn subagent_selection(&self, dashboard: &DashboardState) -> Option<SubagentPolicy> {
        self.subagent_choice_applies(dashboard)
            .then(|| self.subagents.policy.clone())
    }

    /// Move offers the delegation choice when its destination profile is
    /// Claude or Codex. A plain resume keeps the session's own policy.
    pub(crate) fn subagent_choice_applies(&self, dashboard: &DashboardState) -> bool {
        self.moving
            && dashboard
                .resume_wizard_profiles(self)
                .get(self.profile)
                .is_some_and(|(_, kind)| kind.supports_delegation_tools())
    }

    /// The policy a Move request carries: `None` keeps the session's own, so
    /// an untouched choice changes nothing.
    pub(crate) fn subagent_change(&self, dashboard: &DashboardState) -> Option<SubagentPolicy> {
        let stored = dashboard
            .session_record(&self.session_id)
            .and_then(|session| session.subagents.clone())
            .unwrap_or_default();
        (self.subagent_choice_applies(dashboard) && self.subagents.policy != stored)
            .then(|| self.subagents.policy.clone())
    }
}
