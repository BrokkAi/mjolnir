//! Request launch checks and render the verdicts published by the daemon.
use mj_client::availability::{AvailabilityNote, RuntimeAvailability};
use mj_core::config::{Config, HarnessProfile, TargetTemplate};
use mj_core::snapshot_map::SharedSnapshot;

use crate::{DashboardAction, DashboardState, Mode};

pub use mj_client::availability::{MachineFinding, MachineRuntimeProbe};

#[derive(Debug, Clone)]
pub(crate) struct Availability {
    wanted: bool,
    held: bool,
    pub(crate) snapshot: SharedSnapshot<RuntimeAvailability>,
}

impl Default for Availability {
    fn default() -> Self {
        Self {
            wanted: true,
            held: false,
            snapshot: Default::default(),
        }
    }
}

impl DashboardState {
    pub(crate) fn want_availability_check(&mut self) {
        self.availability.wanted = true;
    }

    /// Wait for first-run setup to decide whether the configuration is new.
    pub fn hold_availability_check(&mut self) {
        self.availability.held = true;
    }

    pub fn release_availability_check(&mut self) {
        self.availability.held = false;
    }

    pub fn take_availability_check(&mut self) -> Option<DashboardAction> {
        if !self.availability.wanted
            || self.availability.held
            || matches!(self.mode, Mode::Setup(_))
        {
            return None;
        }
        self.availability.wanted = false;
        Some(DashboardAction::CheckAvailability)
    }

    /// Install configuration and its verdicts from the same feed frame.
    pub fn set_runtime_config(
        &mut self,
        config: Config,
        availability: SharedSnapshot<RuntimeAvailability>,
    ) {
        if *self.config == config && self.availability.snapshot == availability {
            return;
        }
        let selected = self.wizard_selection();
        self.availability.snapshot = availability;
        self.set_config(config);
        self.reselect_wizard(selected);
    }

    pub(crate) fn profile_missing(&self, id: &str) -> Option<&str> {
        self.availability
            .snapshot
            .profiles
            .get(id)?
            .missing_reason()
    }

    pub(crate) fn missing_target_lines(&self) -> &[AvailabilityNote] {
        &self.availability.snapshot.target_notes
    }

    pub(crate) fn target_unavailable(&self, id: &str) -> Option<String> {
        if let Some(notice) = self
            .config
            .targets
            .get(id)
            .and_then(TargetTemplate::launch_refusal)
        {
            return Some(notice.into());
        }
        (!self.target_offered(id)).then(|| {
            self.target_missing_line(id)
                .unwrap_or_else(|| format!("{id} is not available."))
        })
    }

    pub(crate) fn target_offered(&self, id: &str) -> bool {
        self.config.enabled_target(id).is_some() && self.target_missing_line(id).is_none()
    }

    pub(crate) fn target_missing_line(&self, id: &str) -> Option<String> {
        self.config.enabled_target(id)?;
        self.availability
            .snapshot
            .targets
            .get(id)?
            .missing_reason()
            .map(str::to_owned)
    }

    pub(crate) fn default_engine_missing(&self, id: &str) -> bool {
        self.config.is_default_target(id)
            && self.availability.snapshot.uninstalled_engines.contains(id)
    }

    pub(crate) fn machine_runtime_reason(&self, id: &str) -> Option<String> {
        self.availability
            .snapshot
            .machines_without_runtime
            .get(id)
            .cloned()
    }

    /// Whether profile `id` is offered for new work.
    pub(crate) fn profile_offered(&self, id: &str) -> bool {
        self.config.enabled_profile(id).is_some() && self.profile_missing(id).is_none()
    }

    /// The profiles a new session can use, in configuration order.
    pub(crate) fn offered_profiles(&self) -> impl Iterator<Item = (&str, &HarnessProfile)> {
        self.config
            .enabled_profiles()
            .filter(|(id, _)| self.profile_missing(id).is_none())
    }

    /// The id of the offered profile at `index`, if there is one.
    pub(crate) fn nth_offered_profile(&self, index: usize) -> Option<String> {
        self.offered_profiles()
            .nth(index)
            .map(|(id, _)| id.to_owned())
    }

    /// The index in `config.targets` of `wanted` when it is offered, or of
    /// the first offered runtime.
    pub(crate) fn offered_target_index_or_first(&self, wanted: &str) -> usize {
        let keys = || self.config.targets.keys();
        keys()
            .position(|id| id == wanted)
            .filter(|_| self.target_offered(wanted))
            .or_else(|| keys().position(|id| self.target_offered(id)))
            .unwrap_or(0)
    }

    /// Why no profile can be offered, when every enabled one is missing.
    pub(crate) fn missing_profiles_notice(&self) -> Option<String> {
        let lines = self.missing_profile_lines();
        (!lines.is_empty()).then(|| format!("No profile is available here. {}", lines.join(" ")))
    }

    /// The line under a profile list for each enabled profile it leaves out.
    pub(crate) fn missing_profile_lines(&self) -> Vec<String> {
        self.config
            .enabled_profiles()
            .filter_map(|(id, _)| Some(format!("{id}: {}", self.profile_missing(id)?)))
            .collect()
    }

    /// The ids an open wizard has chosen, so an answer that removes one can
    /// move the wizard off it.
    fn wizard_selection(&self) -> Option<(Option<String>, String)> {
        match &self.mode {
            Mode::New(wizard) => Some((
                self.offered_profiles()
                    .nth(wizard.profile)
                    .map(|(id, _)| id.to_owned()),
                crate::nth_key(&self.config.targets, wizard.target),
            )),
            Mode::Resume(wizard) => Some((
                self.resume_wizard_profiles(wizard)
                    .get(wizard.profile)
                    .map(|(id, _)| (*id).clone()),
                crate::nth_key(&self.config.targets, wizard.target),
            )),
            _ => None,
        }
    }

    /// Keep an open wizard on the profile and runtime it had chosen, by id.
    /// One that is no longer offered gives way to the first that is, and the
    /// wizard returns to that step so the change is seen before anything
    /// launches. A launch already submitted keeps its choices and is left to
    /// its own preflight. A wizard left with nothing to offer, before it
    /// launches, closes with the reason.
    fn reselect_wizard(&mut self, selected: Option<(Option<String>, String)>) {
        use crate::wizards::WizardStep;
        let Some((profile_id, target_id)) = selected else {
            return;
        };
        let (profiles, launching): (Vec<String>, bool) = match &self.mode {
            Mode::New(wizard) => (
                self.offered_profiles()
                    .map(|(id, _)| id.to_owned())
                    .collect(),
                wizard.step == WizardStep::Launching,
            ),
            Mode::Resume(wizard) => (
                self.resume_wizard_profiles(wizard)
                    .into_iter()
                    .map(|(id, _)| id.clone())
                    .collect(),
                wizard.step == WizardStep::Launching,
            ),
            _ => return,
        };
        let first_target = self
            .config
            .targets
            .keys()
            .position(|id| self.target_offered(id));
        if !launching && (profiles.is_empty() || first_target.is_none()) {
            let reason = match profile_id.as_deref() {
                Some(id) if profiles.is_empty() => self
                    .profile_missing(id)
                    .map(|reason| format!("No profile is available here. {id}: {reason}"))
                    .unwrap_or_else(|| "No profile is available here.".to_owned()),
                _ => self
                    .target_missing_line(&target_id)
                    .map(|reason| format!("No runtime is available here. {target_id}: {reason}"))
                    .unwrap_or_else(|| "No runtime is available here.".to_owned()),
            };
            self.mode = Mode::Dashboard;
            self.set_failure_notice(reason);
            return;
        }
        let profile = profile_id
            .as_deref()
            .and_then(|id| profiles.iter().position(|offered| offered == id));
        let target_index = self.config.targets.keys().position(|id| *id == target_id);
        let target = target_index.filter(|_| self.target_offered(&target_id));
        let last_profile = profiles.len().saturating_sub(1);
        let mut gone = None;
        let mut place =
            |step: &mut WizardStep, wizard_profile: &mut usize, wizard_target: &mut usize| {
                if *step == WizardStep::Launching {
                    // Keep pointing at the same choices; the launch carries them.
                    *wizard_profile = profile.unwrap_or((*wizard_profile).min(last_profile));
                    *wizard_target = target_index.unwrap_or(*wizard_target);
                    return;
                }
                *wizard_profile = profile.unwrap_or(0);
                *wizard_target = target.or(first_target).unwrap_or(0);
                gone = return_to_changed_step(step, profile.is_none(), target.is_none());
            };
        let mut reveal_new_profile = None;
        match &mut self.mode {
            Mode::New(wizard) => {
                let before = wizard.profile;
                place(&mut wizard.step, &mut wizard.profile, &mut wizard.target);
                if wizard.profile != before || profile.is_none() {
                    wizard.focused_provider = None;
                    reveal_new_profile = Some(wizard.profile);
                }
            }
            Mode::Resume(wizard) => {
                if !launching && (profile.is_none() || target.is_none()) {
                    wizard.note_destination_change();
                }
                place(&mut wizard.step, &mut wizard.profile, &mut wizard.target);
            }
            _ => {}
        }
        if let Some(profile_index) = reveal_new_profile {
            let provider = crate::wizards::NewWizard::provider_for_profile(self, profile_index);
            if let (Some(provider), Mode::New(wizard)) = (provider, &mut self.mode) {
                wizard.expanded_providers.insert(provider);
            }
        }
        if let Some(gone) = gone {
            let (id, reason) = match gone {
                WizardStep::Profile => {
                    let id = profile_id.unwrap_or_default();
                    let reason = self.profile_missing(&id).map(str::to_owned);
                    (id, reason)
                }
                _ => (target_id.clone(), self.target_missing_line(&target_id)),
            };
            self.set_failure_notice(match reason {
                Some(reason) => format!("{id} is no longer available: {reason}"),
                None => format!("{id} is no longer available."),
            });
        }
    }
}

/// Send a wizard past the step of a choice that just disappeared back to
/// that step. Returns the step it returned to, if it moved. A launch already
/// submitted is left to its own preflight.
fn return_to_changed_step(
    step: &mut crate::wizards::WizardStep,
    profile_gone: bool,
    target_gone: bool,
) -> Option<crate::wizards::WizardStep> {
    use crate::wizards::WizardStep;
    if *step == WizardStep::Launching {
        return None;
    }
    if profile_gone && *step != WizardStep::Profile {
        *step = WizardStep::Profile;
        return Some(WizardStep::Profile);
    }
    if target_gone && !matches!(*step, WizardStep::Profile | WizardStep::Target) {
        *step = WizardStep::Target;
        return Some(WizardStep::Target);
    }
    None
}
