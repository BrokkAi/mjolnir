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
        let selected = self.resume_wizard_selection();
        self.availability.snapshot = availability;
        self.set_config(config);
        self.reselect_resume_wizard(selected);
        // Availability alone can change what a draft may choose.
        self.reconcile_new_wizard();
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

    /// The ids an open resume wizard has chosen, so an answer that removes one
    /// can move the wizard off it. The new-session draft keeps its choices by
    /// id and reconciles itself; see [`Self::reconcile_new_wizard`].
    fn resume_wizard_selection(&self) -> Option<(Option<String>, String)> {
        match &self.mode {
            Mode::Resume(wizard) => Some((
                self.resume_wizard_profiles(wizard)
                    .get(wizard.profile)
                    .map(|(id, _)| (*id).clone()),
                crate::nth_key(&self.config.targets, wizard.target),
            )),
            _ => None,
        }
    }

    /// Keep an open resume wizard on the profile and runtime it had chosen,
    /// by id. One that is no longer offered gives way to the first that is,
    /// and the wizard returns to that step so the change is seen before
    /// anything launches. A launch already submitted keeps its choices and is
    /// left to its own preflight. A wizard left with nothing to offer, before
    /// it launches, closes with the reason.
    fn reselect_resume_wizard(&mut self, selected: Option<(Option<String>, String)>) {
        use crate::wizards::WizardStep;
        let Some((profile_id, target_id)) = selected else {
            return;
        };
        let Mode::Resume(wizard) = &self.mode else {
            return;
        };
        let profiles: Vec<String> = self
            .resume_wizard_profiles(wizard)
            .into_iter()
            .map(|(id, _)| id.clone())
            .collect();
        let launching = wizard.step == WizardStep::Launching;
        let first_target = self
            .config
            .targets
            .keys()
            .position(|id| self.target_offered(id));
        if !launching && (profiles.is_empty() || first_target.is_none()) {
            self.close_wizard_without_choices(
                profile_id.as_deref(),
                &target_id,
                profiles.is_empty(),
            );
            return;
        }
        let profile = profile_id
            .as_deref()
            .and_then(|id| profiles.iter().position(|offered| offered == id));
        let target_index = self.config.targets.keys().position(|id| *id == target_id);
        let target = target_index.filter(|_| self.target_offered(&target_id));
        let last_profile = profiles.len().saturating_sub(1);
        let mut gone = None;
        if let Mode::Resume(wizard) = &mut self.mode {
            if !launching && (profile.is_none() || target.is_none()) {
                wizard.note_destination_change();
            }
            if launching {
                // Keep pointing at the same choices; the launch carries them.
                wizard.profile = profile.unwrap_or(wizard.profile.min(last_profile));
                wizard.target = target_index.unwrap_or(wizard.target);
            } else {
                wizard.profile = profile.unwrap_or(0);
                wizard.target = target.or(first_target).unwrap_or(0);
                gone =
                    return_to_changed_step(&mut wizard.step, profile.is_none(), target.is_none());
            }
        }
        if let Some(gone) = gone {
            self.report_wizard_choice_gone(gone, profile_id.unwrap_or_default(), &target_id);
        }
    }

    /// Keep an open new-session draft on choices that still exist. The draft
    /// holds its profile and target by id, so a reload cannot move them onto
    /// another row; this checks each against the new configuration and
    /// availability, reseats one that is gone on the first that is offered,
    /// and returns to the step where the change must be seen. Provider
    /// selections still in flight are invalidated here and asked again by
    /// [`DashboardState::take_prerequisite_check`] only if the draft still
    /// asks the same question.
    pub(crate) fn reconcile_new_wizard(&mut self) {
        use crate::wizards::{NewWizard, ProfileChoice, WizardStep};
        let Mode::New(wizard) = &self.mode else {
            return;
        };
        let (pending, target_id, chosen, resolved) = (
            wizard.profile_selection_request.is_some(),
            wizard.target.clone(),
            wizard.profile.clone(),
            wizard.resolved_profile.clone(),
        );
        if pending {
            self.invalidate_session_preflight();
        }
        let chosen_id = match &chosen {
            ProfileChoice::Profile(id) => Some(id.clone()),
            ProfileChoice::AutoPrimary => None,
        };
        // A draft in `Launching` has skipped its review and is waiting on the
        // checks that precede creation. While the choices it holds are still
        // configured, a check that now calls one unavailable is left to say
        // so itself, as for any launch under way (#1370); only a choice the
        // configuration no longer has is reseated.
        let launching =
            matches!(&self.mode, Mode::New(wizard) if wizard.step == WizardStep::Launching);
        let first_target = self
            .config
            .targets
            .keys()
            .find(|id| self.target_offered(id))
            .or_else(|| {
                launching
                    .then(|| self.config.targets.keys().next())
                    .flatten()
            })
            .cloned();
        let no_profiles = if launching {
            self.config.enabled_profiles().next().is_none()
        } else {
            self.offered_profiles().next().is_none()
        };
        let Some(first_target) = first_target.filter(|_| !no_profiles) else {
            self.close_wizard_without_choices(chosen_id.as_deref(), &target_id, no_profiles);
            return;
        };
        let profile_gone = if launching {
            match &chosen {
                ProfileChoice::AutoPrimary => !self.new_wizard_has_auto_primary(),
                ProfileChoice::Profile(id) => self.config.enabled_profile(id).is_none(),
            }
        } else {
            self.new_wizard_profile_row(&chosen).is_none()
        };
        // A provider's pick must still belong to the provider it was made for.
        let resolved_stale = !launching
            && resolved.as_ref().is_some_and(|(id, provider)| {
                NewWizard::provider_for_profile(self, id).as_ref() != Some(provider)
            });
        let target_gone = if launching {
            !self.config.targets.contains_key(&target_id)
        } else {
            !self.target_offered(&target_id)
        };
        let replacement = if profile_gone {
            self.new_wizard_choice_at(0)
        } else {
            None
        };
        let revealed = match &replacement {
            Some(ProfileChoice::Profile(id)) => NewWizard::provider_for_profile(self, id),
            _ => None,
        };
        let Mode::New(wizard) = &mut self.mode else {
            return;
        };
        if let Some(choice) = replacement {
            wizard.set_profile_choice(choice);
        } else if resolved_stale {
            wizard.resolved_profile = None;
        }
        if let Some(provider) = revealed {
            wizard.expanded_providers.insert(provider);
        }
        if target_gone {
            wizard.target = first_target;
        }
        let changed = profile_gone || resolved_stale || target_gone;
        if changed {
            // A new-session draft in `Launching` has only skipped its review:
            // creation is submitted by closing the wizard, so no launch has
            // been accepted yet. Any check still running described the old
            // choices; its reply is refused by the generation bump below and
            // the review asks again.
            if wizard.step == WizardStep::Launching {
                wizard.step = WizardStep::Review;
            }
            wizard.remote_repositories = None;
            wizard.remote_preflight_in_flight = false;
            wizard.remote_preflight_error = None;
        }
        let gone = return_to_changed_step(
            &mut wizard.step,
            profile_gone || resolved_stale,
            target_gone,
        );
        if changed {
            self.invalidate_session_preflight();
        }
        if let Some(gone) = gone {
            self.report_wizard_choice_gone(gone, chosen_id.unwrap_or_default(), &target_id);
        }
    }

    /// Closes a wizard whose configuration left nothing to choose, saying why.
    fn close_wizard_without_choices(
        &mut self,
        profile_id: Option<&str>,
        target_id: &str,
        no_profiles: bool,
    ) {
        let reason = match profile_id {
            Some(id) if no_profiles => self
                .profile_missing(id)
                .map(|reason| format!("No profile is available here. {id}: {reason}"))
                .unwrap_or_else(|| "No profile is available here.".to_owned()),
            _ if no_profiles => "No profile is available here.".to_owned(),
            _ => self
                .target_missing_line(target_id)
                .map(|reason| format!("No runtime is available here. {target_id}: {reason}"))
                .unwrap_or_else(|| "No runtime is available here.".to_owned()),
        };
        self.mode = Mode::Dashboard;
        self.set_failure_notice(reason);
    }

    /// Tells the person which choice a reload removed.
    fn report_wizard_choice_gone(
        &mut self,
        step: crate::wizards::WizardStep,
        profile_id: String,
        target_id: &str,
    ) {
        use crate::wizards::WizardStep;
        let (id, reason) = match step {
            WizardStep::Profile => {
                let reason = self.profile_missing(&profile_id).map(str::to_owned);
                (profile_id, reason)
            }
            _ => (target_id.to_owned(), self.target_missing_line(target_id)),
        };
        self.set_failure_notice(match reason {
            Some(reason) => format!("{id} is no longer available: {reason}"),
            None => format!("{id} is no longer available."),
        });
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
