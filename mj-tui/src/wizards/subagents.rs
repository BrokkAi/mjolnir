use super::*;
use mj_core::subagent::{SubagentOptions, SubagentPolicy};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SubagentDiscovery {
    pub id: u64,
    pub profile: String,
    pub model: Option<String>,
    pub result: Option<Result<SubagentOptions, String>>,
}

impl NewWizard {
    pub(crate) fn subagent_options(&self) -> Option<&SubagentOptions> {
        self.subagent_discovery
            .as_ref()?
            .result
            .as_ref()?
            .as_ref()
            .ok()
    }

    pub(crate) fn subagent_error(&self) -> Option<String> {
        if !matches!(*self.subagents, SubagentPolicy::SingleModel { .. }) {
            return None;
        }
        match self
            .subagent_discovery
            .as_ref()
            .and_then(|discovery| discovery.result.as_ref())
        {
            None => Some("Loading eligible models and efforts…".into()),
            Some(Err(error)) => Some(error.clone()),
            Some(Ok(options)) => options.validate(&self.subagents).err(),
        }
    }

    pub(crate) fn subagent_models(&self) -> Vec<String> {
        std::iter::once("Select model".into())
            .chain(
                self.subagent_options()
                    .into_iter()
                    .flat_map(|options| options.models.iter().map(|choice| choice.name.clone())),
            )
            .collect()
    }

    pub(crate) fn subagent_efforts(&self) -> Vec<String> {
        let Some(options) = self.subagent_options() else {
            return vec!["Loading efforts…".into()];
        };
        if options.efforts.is_empty() {
            return vec!["Harness default".into()];
        }
        std::iter::once("Select effort".into())
            .chain(options.efforts.iter().map(|choice| choice.name.clone()))
            .collect()
    }

    pub(crate) fn subagent_model_index(&self) -> usize {
        let SubagentPolicy::SingleModel { model, .. } = &*self.subagents else {
            return 0;
        };
        self.subagent_options()
            .and_then(|options| {
                options
                    .models
                    .iter()
                    .position(|choice| &choice.value == model)
            })
            .map_or(0, |index| index + 1)
    }

    pub(crate) fn subagent_effort_index(&self) -> usize {
        let SubagentPolicy::SingleModel {
            effort: Some(effort),
            ..
        } = &*self.subagents
        else {
            return 0;
        };
        self.subagent_options()
            .and_then(|options| {
                options
                    .efforts
                    .iter()
                    .position(|choice| &choice.value == effort)
            })
            .map_or(0, |index| index + 1)
    }

    pub(crate) fn select_subagent_model(&mut self, index: usize) {
        let model = index
            .checked_sub(1)
            .and_then(|index| self.subagent_options()?.models.get(index))
            .map(|choice| choice.value.clone())
            .unwrap_or_default();
        if matches!(&*self.subagents, SubagentPolicy::SingleModel { model: selected, .. } if selected == &model)
        {
            return;
        }
        *self.subagents = SubagentPolicy::SingleModel {
            model,
            effort: None,
        };
        self.subagent_discovery = None;
    }

    pub(crate) fn select_subagent_effort(&mut self, index: usize) {
        let effort = index
            .checked_sub(1)
            .and_then(|index| self.subagent_options()?.efforts.get(index))
            .map(|choice| choice.value.clone());
        if let SubagentPolicy::SingleModel {
            effort: selected, ..
        } = &mut *self.subagents
        {
            *selected = effort;
        }
    }
}

impl DashboardState {
    pub(crate) fn take_subagent_discovery(&mut self) -> Option<DashboardAction> {
        let Mode::New(wizard) = &mut self.mode else {
            return None;
        };
        if wizard.step != WizardStep::Review || !wizard.subagent_choice_applies(&self.config) {
            return None;
        }
        let SubagentPolicy::SingleModel { model, .. } = &*wizard.subagents else {
            return None;
        };
        let model = (!model.is_empty()).then(|| model.clone());
        let profile = nth_enabled_profile(&self.config, wizard.profile);
        if wizard
            .subagent_discovery
            .as_ref()
            .is_some_and(|discovery| discovery.profile == profile && discovery.model == model)
        {
            return None;
        }
        static NEXT_REQUEST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let id = NEXT_REQUEST.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        wizard.subagent_discovery = Some(Box::new(SubagentDiscovery {
            id,
            profile: profile.clone(),
            model: model.clone(),
            result: None,
        }));
        Some(DashboardAction::DiscoverSubagentOptions { id, profile, model })
    }

    pub fn apply_subagent_options(&mut self, id: u64, result: Result<SubagentOptions, String>) {
        let Mode::New(wizard) = &mut self.mode else {
            return;
        };
        let Some(discovery) = &mut wizard.subagent_discovery else {
            return;
        };
        if discovery.id == id {
            discovery.result = Some(result);
        }
    }
}
