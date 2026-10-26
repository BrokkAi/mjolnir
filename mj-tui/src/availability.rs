//! What is there: the one answer to which profiles and runtimes the session
//! pickers offer.
//!
//! A check runs in the background when the dashboard starts and whenever a
//! new-session or resume picker opens, never more than one at a time. It
//! adds what is new to the configuration and answers, for everything
//! configured, whether it is there. The pickers list only what is offered:
//! enabled, and not found missing. Nothing is ever shown as being checked:
//! until an answer arrives the configuration stands, and the screen changes
//! only when an answer does. What is missing is named once under the list,
//! with why and how to fix it.

use std::collections::{BTreeMap, BTreeSet};

use mj_core::config::{
    HarnessProfile, Machine, TargetTemplate, runtime_kind_name, target_machine_id,
};

use crate::{DashboardAction, DashboardState, Mode, RejectedRuntime};

/// What probing one machine found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineRuntimeProbe {
    pub machine_id: String,
    /// The machine as it was probed.
    pub machine: Machine,
    pub finding: MachineFinding,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MachineFinding {
    /// The machine answered.
    Answered {
        /// The runtimes it can run, under the ids they would like.
        runtimes: Vec<(String, TargetTemplate)>,
        /// The runtimes it cannot run, with why.
        rejected: Vec<RejectedRuntime>,
    },
    /// The machine did not answer, so what it runs is unknown.
    Unreachable { detail: String, remediation: String },
}

/// One of this computer's container engines, as a check found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalEngine {
    /// Why it cannot run sessions, or `None` when it can.
    pub missing: Option<String>,
    /// Whether its command is on this computer at all. A standard runtime
    /// for an engine that was never installed is left out without a word;
    /// one that is installed but not working is named.
    pub installed: bool,
}

/// What one availability check found and changed.
#[derive(Debug, Clone, PartialEq)]
pub struct AvailabilityChecked {
    /// Every configured profile as it was checked, and why it is missing.
    pub profiles: BTreeMap<String, (HarnessProfile, Option<String>)>,
    /// This computer's engines, by runtime kind (`podman`, `docker`,
    /// `apple-container`).
    pub local: BTreeMap<String, LocalEngine>,
    /// What each SSH and EC2 machine can run.
    pub machines: Vec<MachineRuntimeProbe>,
    pub added_profiles: Vec<String>,
    pub added_runtimes: BTreeMap<String, Vec<String>>,
}

/// The dashboard's latest answers. Each answer applies only while what it
/// describes is configured unchanged; an edited profile or machine has no
/// answer until the next check.
#[derive(Debug, Clone)]
pub(crate) struct Availability {
    /// A check should start: at startup, and whenever a picker opens.
    wanted: bool,
    in_flight: Option<u64>,
    generation: u64,
    profiles: BTreeMap<String, (HarnessProfile, Option<String>)>,
    local: BTreeMap<String, LocalEngine>,
    machines: BTreeMap<String, MachineRuntimeProbe>,
    /// The last failure reported, so a check that keeps failing the same
    /// way says so once.
    failure: Option<String>,
}

impl Default for Availability {
    fn default() -> Self {
        Self {
            wanted: true,
            in_flight: None,
            generation: 0,
            profiles: BTreeMap::new(),
            local: BTreeMap::new(),
            machines: BTreeMap::new(),
            failure: None,
        }
    }
}

/// An entry the pickers leave out, and the line that says why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Missing {
    /// Named under the list.
    Named(String),
    /// A standard runtime whose engine was never installed here.
    Silent,
}

impl DashboardState {
    /// Ask for a check the next time the dashboard can start one.
    pub(crate) fn want_availability_check(&mut self) {
        self.availability.wanted = true;
    }

    /// Start a check if one is wanted and none is running. Settings holds
    /// its own draft of profiles and machines, so a check waits for it to
    /// close rather than write under it.
    pub fn take_availability_check(&mut self) -> Option<DashboardAction> {
        let availability = &mut self.availability;
        if !availability.wanted || availability.in_flight.is_some() || setup_open(&self.mode) {
            return None;
        }
        availability.wanted = false;
        availability.generation = availability.generation.wrapping_add(1);
        availability.in_flight = Some(availability.generation);
        Some(DashboardAction::CheckAvailability {
            generation: availability.generation,
        })
    }

    /// Record a check's answers. What it added is said; what is missing is
    /// shown where it matters, under the picker lists, so a check that
    /// finds what the last one found changes nothing on screen.
    pub fn apply_availability_check(
        &mut self,
        generation: u64,
        result: Result<AvailabilityChecked, String>,
    ) {
        if self.availability.in_flight != Some(generation) {
            return;
        }
        self.availability.in_flight = None;
        let checked = match result {
            Ok(checked) => checked,
            Err(error) => {
                if self.availability.failure.as_deref() != Some(error.as_str()) {
                    self.set_failure_notice(format!("Could not check what is available: {error}"));
                    self.availability.failure = Some(error);
                }
                return;
            }
        };
        self.availability.failure = None;
        let selected = self.wizard_selection();
        self.availability.profiles = checked.profiles;
        self.availability.local = checked.local;
        self.availability.machines = checked
            .machines
            .into_iter()
            .map(|probe| (probe.machine_id.clone(), probe))
            .collect();
        let mut sentences = Vec::new();
        if !checked.added_profiles.is_empty() {
            sentences.push(format!(
                "Added agent profiles: {}.",
                checked.added_profiles.join(", ")
            ));
        }
        for (machine, added) in &checked.added_runtimes {
            sentences.push(format!(
                "Added runtimes for {machine}: {}.",
                added.join(", ")
            ));
        }
        if !sentences.is_empty() {
            self.set_notice(sentences.join(" "));
        }
        self.reselect_wizard(selected);
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

    /// The id of the offered profile at `index`.
    pub(crate) fn nth_offered_profile(&self, index: usize) -> String {
        self.offered_profiles()
            .nth(index)
            .map(|(id, _)| id.to_owned())
            .expect("a wizard opens only with an offered profile")
    }

    /// Why enabled profile `id` is missing, from the latest answer about it
    /// as it is configured now.
    pub(crate) fn profile_missing(&self, id: &str) -> Option<&str> {
        let current = self.config.profiles.get(id)?;
        let (checked, missing) = self.availability.profiles.get(id)?;
        (checked == current).then_some(missing.as_deref()).flatten()
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

    /// The line under a target list for each enabled runtime it leaves out,
    /// then for each machine that runs nothing yet.
    pub(crate) fn missing_target_lines(&self) -> Vec<String> {
        let mut lines: Vec<String> = self
            .config
            .targets
            .keys()
            .filter_map(|id| Some(format!("{id}: {}", self.target_missing_line(id)?)))
            .collect();
        lines.extend(self.machines_without_runtime_lines());
        lines
    }

    /// Whether runtime `id` is offered for new work.
    pub(crate) fn target_offered(&self, id: &str) -> bool {
        self.config.enabled_target(id).is_some() && self.target_missing(id).is_none()
    }

    /// Why runtime `id` is missing, from the latest answer about its engine
    /// or its machine as configured now; `None` while it is there or no
    /// answer applies.
    pub(crate) fn target_missing(&self, id: &str) -> Option<Missing> {
        let target = self.config.targets.get(id)?;
        let kind = runtime_kind_name(target);
        match target {
            TargetTemplate::LocalBare => None,
            TargetTemplate::LocalPodman { .. }
            | TargetTemplate::LocalDocker { .. }
            | TargetTemplate::AppleContainer { .. } => {
                let engine = self.availability.local.get(kind)?;
                let reason = engine.missing.as_ref()?;
                Some(if !engine.installed && self.config.is_default_target(id) {
                    Missing::Silent
                } else {
                    Missing::Named(reason.clone())
                })
            }
            TargetTemplate::SshBare { .. }
            | TargetTemplate::SshPodman { .. }
            | TargetTemplate::SshDocker { .. }
            | TargetTemplate::SshAppleContainer { .. }
            | TargetTemplate::AwsEc2 { .. } => {
                let machine_id = target_machine_id(target, &self.config.machines)?;
                let probe =
                    self.availability.machines.get(machine_id).filter(|probe| {
                        self.config.machines.get(machine_id) == Some(&probe.machine)
                    })?;
                match &probe.finding {
                    MachineFinding::Unreachable {
                        detail,
                        remediation,
                    } => Some(Missing::Named(sentence(
                        &format!("{machine_id} did not answer. {}", detail.trim()),
                        remediation,
                    ))),
                    MachineFinding::Answered { runtimes, rejected } => {
                        if runtimes
                            .iter()
                            .any(|(_, found)| runtime_kind_name(found) == kind)
                        {
                            return None;
                        }
                        let rejected = rejected.iter().find(|runtime| runtime.kind == kind)?;
                        Some(Missing::Named(sentence(
                            &rejected.detail,
                            rejected.remediation.as_deref().unwrap_or_default(),
                        )))
                    }
                }
            }
        }
    }

    /// The reason a configured, enabled runtime is not listed, for the line
    /// under the target list. A standard runtime whose engine was never
    /// installed has none.
    pub(crate) fn target_missing_line(&self, id: &str) -> Option<String> {
        self.config.enabled_target(id)?;
        match self.target_missing(id)? {
            Missing::Named(reason) => Some(reason),
            Missing::Silent => None,
        }
    }

    /// Why machine `id`, which no runtime runs on, still has none, or `None`
    /// before a check of its current definition has answered.
    pub(crate) fn machine_runtime_reason(&self, id: &str) -> Option<String> {
        let machine = self.config.machines.get(id)?;
        let probe = self
            .availability
            .machines
            .get(id)
            .filter(|probe| probe.machine == *machine)?;
        Some(finding_reason(&probe.finding))
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

    /// Keep an open wizard on the profile and runtime it had chosen. One
    /// that is no longer offered gives way to the first that is, and the
    /// wizard returns to that step so the change is seen before anything
    /// launches. A wizard left with nothing to offer closes with the reason.
    fn reselect_wizard(&mut self, selected: Option<(Option<String>, String)>) {
        let Some((profile_id, target_id)) = selected else {
            return;
        };
        let profiles: Vec<String> = match &self.mode {
            Mode::New(_) => self
                .offered_profiles()
                .map(|(id, _)| id.to_owned())
                .collect(),
            Mode::Resume(wizard) => self
                .resume_wizard_profiles(wizard)
                .into_iter()
                .map(|(id, _)| id.clone())
                .collect(),
            _ => return,
        };
        let first_target = self
            .config
            .targets
            .keys()
            .position(|id| self.target_offered(id));
        if profiles.is_empty() || first_target.is_none() {
            let reason = match profile_id.as_deref() {
                Some(id) if profiles.is_empty() => self
                    .profile_missing(id)
                    .map(|reason| format!("{id}: {reason}"))
                    .unwrap_or_else(|| "No profile is available.".to_owned()),
                _ => "No runtime is available.".to_owned(),
            };
            self.mode = Mode::Dashboard;
            self.set_failure_notice(reason);
            return;
        }
        let profile = profile_id
            .as_deref()
            .and_then(|id| profiles.iter().position(|offered| offered == id));
        let target = self
            .config
            .targets
            .keys()
            .position(|id| *id == target_id)
            .filter(|_| self.target_offered(&target_id));
        let mut notice = None;
        match &mut self.mode {
            Mode::New(wizard) => {
                wizard.profile = profile.unwrap_or(0);
                wizard.target = target.or(first_target).unwrap_or(0);
                notice =
                    return_to_changed_step(&mut wizard.step, profile.is_none(), target.is_none());
            }
            Mode::Resume(wizard) => {
                wizard.profile = profile.unwrap_or(0);
                wizard.target = target.or(first_target).unwrap_or(0);
                notice =
                    return_to_changed_step(&mut wizard.step, profile.is_none(), target.is_none());
            }
            _ => {}
        }
        if let Some(gone) = notice {
            let (id, reason) = match gone {
                crate::wizards::WizardStep::Profile => {
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

    /// The machines no runtime runs on, for the line under the target list.
    pub(crate) fn machines_without_runtime_lines(&self) -> Vec<String> {
        let ids: BTreeSet<&str> = self
            .config
            .machines_without_runtimes()
            .map(|(id, _)| id)
            .collect();
        ids.into_iter()
            .filter_map(|id| {
                self.machine_runtime_reason(id)
                    .map(|reason| format!("Machine {id} has no runtime: {reason}"))
            })
            .collect()
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

fn setup_open(mode: &Mode) -> bool {
    match mode {
        Mode::Setup(_) => true,
        Mode::Help(overlay) => setup_open(&overlay.return_to),
        _ => false,
    }
}

/// Why a machine has no runtime, from what probing it found: the reason it
/// did not answer, or each runtime it cannot run. A finding with usable
/// runtimes only arises when they could not be added.
pub(crate) fn finding_reason(finding: &MachineFinding) -> String {
    match finding {
        MachineFinding::Unreachable {
            detail,
            remediation,
        } => sentence(
            &format!("it did not answer. {}", detail.trim()),
            remediation,
        ),
        MachineFinding::Answered { runtimes, rejected } if runtimes.is_empty() => {
            let mut reason = "it can run none.".to_owned();
            for runtime in rejected {
                reason.push(' ');
                reason.push_str(&sentence(
                    &format!("{}: {}", runtime.label, runtime.detail.trim()),
                    runtime.remediation.as_deref().unwrap_or_default(),
                ));
            }
            reason
        }
        MachineFinding::Answered { runtimes, .. } => format!(
            "it can run {}; choose Detect runtimes in Settings › Runtimes to add them.",
            runtimes
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// `text` as a sentence, followed by `then` as another.
fn sentence(text: &str, then: &str) -> String {
    let mut sentence = text.trim().to_owned();
    if !sentence.ends_with('.') {
        sentence.push('.');
    }
    let then = then.trim();
    if !then.is_empty() {
        sentence.push(' ');
        sentence.push_str(then);
        if !sentence.ends_with('.') {
            sentence.push('.');
        }
    }
    sentence
}
