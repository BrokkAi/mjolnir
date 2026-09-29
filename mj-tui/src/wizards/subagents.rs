use super::*;
use mj_core::subagent::{SubagentOptions, SubagentPolicy};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SubagentDiscovery {
    pub id: u64,
    pub profile: String,
    pub model: Option<String>,
    pub result: Option<Result<SubagentOptions, String>>,
}

/// The delegation choice on a review step: the policy being drafted, its
/// open combo box, and the model discovery a single-model policy needs. New
/// session and Move both carry one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SubagentDraft {
    pub(crate) policy: SubagentPolicy,
    pub(crate) combo: ComboBoxState<WizardControl>,
    pub(crate) discovery: Option<SubagentDiscovery>,
}

impl SubagentDraft {
    pub(crate) fn new(policy: SubagentPolicy) -> Self {
        Self {
            policy,
            ..Self::default()
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

    pub(crate) fn models(&self) -> Vec<String> {
        std::iter::once("Select model".into())
            .chain(
                self.options()
                    .into_iter()
                    .flat_map(|options| options.models.iter().map(|choice| choice.name.clone())),
            )
            .collect()
    }

    pub(crate) fn efforts(&self) -> Vec<String> {
        let Some(options) = self.options() else {
            return vec!["Loading efforts…".into()];
        };
        if options.efforts.is_empty() {
            return vec!["Harness default".into()];
        }
        std::iter::once("Select effort".into())
            .chain(options.efforts.iter().map(|choice| choice.name.clone()))
            .collect()
    }

    pub(crate) fn model_index(&self) -> usize {
        let SubagentPolicy::SingleModel { model, .. } = &self.policy else {
            return 0;
        };
        self.options()
            .and_then(|options| {
                options
                    .models
                    .iter()
                    .position(|choice| &choice.value == model)
            })
            .map_or(0, |index| index + 1)
    }

    pub(crate) fn effort_index(&self) -> usize {
        let SubagentPolicy::SingleModel {
            effort: Some(effort),
            ..
        } = &self.policy
        else {
            return 0;
        };
        self.options()
            .and_then(|options| {
                options
                    .efforts
                    .iter()
                    .position(|choice| &choice.value == effort)
            })
            .map_or(0, |index| index + 1)
    }

    /// Commits the policy row of the combo box. Returns whether it changed.
    pub(crate) fn select_policy(&mut self, index: usize) -> bool {
        if self.policy.index() == index {
            return false;
        }
        self.policy = SubagentPolicy::at_index(index);
        self.discovery = None;
        true
    }

    /// Returns whether the model changed.
    pub(crate) fn select_model(&mut self, index: usize) -> bool {
        let model = index
            .checked_sub(1)
            .and_then(|index| self.options()?.models.get(index))
            .map(|choice| choice.value.clone())
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
        let effort = index
            .checked_sub(1)
            .and_then(|index| self.options()?.efforts.get(index))
            .map(|choice| choice.value.clone());
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

    /// Opens the combo box behind `id`, or forgets the failed discovery for
    /// the retry button. Returns false for any other control.
    pub(crate) fn activate(&mut self, id: WizardControl) -> bool {
        let selected = match id {
            WizardControl::Subagents => self.policy.index(),
            WizardControl::SubagentModel => self.model_index(),
            WizardControl::SubagentEffort => self.effort_index(),
            WizardControl::SubagentRetry => {
                self.discovery = None;
                return true;
            }
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
                len: 4,
                selected: self
                    .combo
                    .selection(WizardControl::Subagents, self.policy.index()),
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
                self.options().is_some(),
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
            form.declare_with_enabled(WizardControl::SubagentRetry, ControlKind::Button, true);
        }
    }

    /// Starts discovery for a single-model policy on `profile` unless the
    /// current one already covers it.
    fn take_discovery(&mut self, profile: String) -> Option<DashboardAction> {
        let SubagentPolicy::SingleModel { model, .. } = &self.policy else {
            return None;
        };
        let model = (!model.is_empty()).then(|| model.clone());
        if self
            .discovery
            .as_ref()
            .is_some_and(|discovery| discovery.profile == profile && discovery.model == model)
        {
            return None;
        }
        static NEXT_REQUEST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let id = NEXT_REQUEST.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.discovery = Some(SubagentDiscovery {
            id,
            profile: profile.clone(),
            model: model.clone(),
            result: None,
        });
        Some(DashboardAction::DiscoverSubagentOptions { id, profile, model })
    }
}

impl DashboardState {
    /// The review step's delegation draft and the profile it is for, when
    /// the open wizard offers the choice.
    fn review_subagent_draft(&mut self) -> Option<(&mut SubagentDraft, String)> {
        let applies = match &self.mode {
            Mode::New(wizard) => {
                wizard.step == WizardStep::Review && wizard.subagent_choice_applies(&self.config)
            }
            Mode::Resume(wizard) => {
                wizard.step == WizardStep::Review && wizard.subagent_choice_applies(self)
            }
            _ => false,
        };
        if !applies {
            return None;
        }
        let profile = match &self.mode {
            Mode::New(wizard) => nth_enabled_profile(&self.config, wizard.profile),
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
        let (draft, profile) = self.review_subagent_draft()?;
        draft.take_discovery(profile)
    }

    pub fn apply_subagent_options(&mut self, id: u64, result: Result<SubagentOptions, String>) {
        let draft = match &mut self.mode {
            Mode::New(wizard) => &mut *wizard.subagents,
            Mode::Resume(wizard) => &mut *wizard.subagents,
            _ => return,
        };
        if let Some(discovery) = &mut draft.discovery
            && discovery.id == id
        {
            discovery.result = Some(result);
        }
    }
}

impl ResumeWizard {
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
            .state
            .sessions
            .get(&self.session_id)
            .and_then(|session| session.subagents.clone())
            .unwrap_or_default();
        (self.subagent_choice_applies(dashboard) && self.subagents.policy != stored)
            .then(|| self.subagents.policy.clone())
    }
}
