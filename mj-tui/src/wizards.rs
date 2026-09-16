//! New-session and resume wizards, including their mount and review steps.
use mj_chat::path_input::PathInput;

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use mj_chat::theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use mj_core::config::{
    Config, HarnessKind, TargetTemplate, container_size_host, is_bare_project_target,
    mount_history_host, project_history_host, raw_project_context_id,
};
use mj_core::state::{
    MaterializedQueuedPrompt, MoveOperation, MovePreparation, ResumeQueueDisposition,
    SessionRecord, SessionResourceAllocation, SessionState, State, allocation_cpus,
    allocation_memory,
};

use mj_chat::components::PathField;
use mj_chat::components::{
    Checkbox, ChoiceList, ComboBox, ComboBoxState, ConsumedEvent, ControlKind, Dialog, FieldEdit,
    Form, FormViewport, Interaction, Outcome, PopupSide,
};
use mj_chat::selection::FrameSurfaces;
use mj_core::targets::{AdditionalMount, MountAccess, default_mount_destination, path_completion};

use crate::widgets::{centered_modal, dismissible_modal_title, format_resource_bytes};
use crate::{
    DashboardAction, DashboardState, Mode, RemoteRepositoryPreview, move_index,
    nth_enabled_profile, nth_key,
};

const BASELINE_CPUS: u64 = 8;
const BASELINE_MEMORY_BYTES: u64 = 32 * 1024 * 1024 * 1024;
const FLOOR_CPUS: u64 = 2;
const FLOOR_MEMORY_BYTES: u64 = 8 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WizardStep {
    Profile,
    Target,
    Bundle,
    ProjectDirectory,
    Review,
    Mounts,
    NewBundle,
}

/// Stable control identities shared by the new-session and resume wizards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WizardControl {
    ProfileList,
    BundleList,
    TargetList,
    ProjectDirectory,
    NewBundleRepositories,
    NewBundleSource,
    NewBundleRemove,
    MountSource,
    MountDestination,
    MountAccess,
    ReviewAttachments,
    CreateManagedWorktree,
    MjolnirSubagents,
    DiscardQueue,
    Cancel,
    Back,
    Next,
    Add,
    Submit,
}

/// How long a stored readiness result stays usable across wizard opens
/// before it is treated as missing and re-probed. Each non-local probe is an
/// ssh round trip (about 2 seconds), so this keeps repeated wizard opens in
/// the same dashboard session from re-probing every time while still
/// catching a target that went unavailable a while ago.
pub(crate) const TARGET_READINESS_TTL: Duration = Duration::from_secs(30 * 60);

/// How long a failed readiness result is kept. Failures are usually
/// transient (a sleeping host, a VPN that is down, an expired cloud session,
/// a probe that timed out) and the user fixes them within minutes, so they
/// are re-probed much sooner than successes; the re-probe only costs the
/// user whose target is already broken.
pub(crate) const TARGET_READINESS_FAILURE_TTL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub(crate) struct TargetReadiness {
    template: TargetTemplate,
    generation: u64,
    result: Option<Result<(), String>>,
    /// When `result` was stored. Only meaningful once `result` is `Some`; a
    /// pending check (`result: None`) is never considered stale, so a
    /// probe already in flight is never re-requested.
    recorded_at: Instant,
}

impl TargetReadiness {
    fn is_stale(&self, now: Instant) -> bool {
        let ttl = match &self.result {
            None => return false,
            Some(Ok(())) => TARGET_READINESS_TTL,
            Some(Err(_)) => TARGET_READINESS_FAILURE_TTL,
        };
        now.saturating_duration_since(self.recorded_at) >= ttl
    }
}

#[derive(Debug, Clone)]
pub(crate) struct NewWizard {
    pub(crate) worktree_options: Option<(String, String, mj_core::state::ManagedWorktreeOptions)>,
    pub(crate) create_managed_worktree: bool,
    /// Whether this session gets Mjolnir's delegation tools instead of its
    /// harness's own. Only Claude and Codex can, so the review step hides the
    /// control for every other kind and the request then sends `None`.
    pub(crate) mjolnir_subagents: bool,
    /// Creation stays in this workspace even if the visible tab changes.
    pub(crate) workspace_id: String,
    pub(crate) step: WizardStep,

    pub(crate) profile: usize,
    bundle: usize,
    pub(crate) target: usize,
    pub(crate) mounts: MountWizard,

    pub(crate) new_bundle_selected: usize,
    pub(crate) new_bundle_repositories: Vec<String>,
    pub(crate) new_bundle_source: PathInput,
    pub(crate) bundle_creation_in_flight: bool,
    pub(crate) project_directory: PathInput,
    pub(crate) project_directory_error: Option<String>,
    project_history: Vec<std::path::PathBuf>,
    project_history_index: usize,
    pub(crate) resource_allocation: Option<SessionResourceAllocation>,
    aws_options: BTreeMap<String, Vec<SessionResourceAllocation>>,
    pub(crate) sizing_error: Option<String>,
    /// Network clone destinations returned by the asynchronous creation
    /// preflight. A nonempty value is concrete evidence the review is ready.
    pub(crate) remote_repositories: Option<Vec<RemoteRepositoryPreview>>,
    /// Covers the whole prerequisite check: attached-directory validation,
    /// then network source resolution.
    pub(crate) remote_preflight_in_flight: bool,
    pub(crate) remote_preflight_error: Option<String>,
    pub(crate) form: RefCell<Dialog<WizardControl>>,
}

impl PartialEq for NewWizard {
    fn eq(&self, other: &Self) -> bool {
        self.worktree_options == other.worktree_options
            && self.create_managed_worktree == other.create_managed_worktree
            && self.mjolnir_subagents == other.mjolnir_subagents
            && self.workspace_id == other.workspace_id
            && self.step == other.step
            && self.profile == other.profile
            && self.bundle == other.bundle
            && self.target == other.target
            && self.mounts == other.mounts
            && self.new_bundle_selected == other.new_bundle_selected
            && self.new_bundle_repositories == other.new_bundle_repositories
            && self.new_bundle_source == other.new_bundle_source
            && self.bundle_creation_in_flight == other.bundle_creation_in_flight
            && self.project_directory == other.project_directory
            && self.project_directory_error == other.project_directory_error
            && self.project_history == other.project_history
            && self.project_history_index == other.project_history_index
            && self.resource_allocation == other.resource_allocation
            && self.aws_options == other.aws_options
            && self.sizing_error == other.sizing_error
            && self.remote_repositories == other.remote_repositories
            && self.remote_preflight_in_flight == other.remote_preflight_in_flight
            && self.remote_preflight_error == other.remote_preflight_error
    }
}

impl Eq for NewWizard {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MountWizard {
    pub(crate) source: PathInput,
    pub(crate) destination: PathInput,

    pub(crate) access: MountAccess,
    pub(crate) access_combo: ComboBoxState<WizardControl>,
    pub(crate) mounts: Vec<AdditionalMount>,
    pub(crate) history: Vec<std::path::PathBuf>,
    history_index: usize,
    completion_cache: BTreeMap<String, Vec<String>>,
    completion_candidates: Vec<String>,
    completion_index: usize,
    /// Sources the target's host reported as unable to hold Podman's overlay,
    /// keyed by the typed source and holding the `filesystem (reason)` label.
    forced_sources: BTreeMap<String, String>,
    pub(crate) error: Option<String>,
    editing_mount: Option<usize>,
}

impl MountWizard {
    pub(crate) fn new(history: Vec<std::path::PathBuf>) -> Self {
        Self {
            source: PathInput::new(),
            destination: PathInput::new(),

            access: MountAccess::Ro,
            access_combo: ComboBoxState::default(),
            mounts: Vec::new(),
            history,
            history_index: 0,
            completion_cache: BTreeMap::new(),
            completion_candidates: Vec::new(),
            completion_index: 0,
            forced_sources: BTreeMap::new(),
            error: None,
            editing_mount: None,
        }
    }

    fn with_mounts(history: Vec<std::path::PathBuf>, mounts: Vec<AdditionalMount>) -> Self {
        let mut wizard = Self::new(history);
        wizard.mounts = mounts;
        wizard
    }

    /// Why the source under edit cannot use the copy-on-write overlay, if it
    /// cannot.
    pub(crate) fn overlay_unavailable(&self) -> Option<&str> {
        self.forced_sources
            .get(self.source.trim())
            .map(String::as_str)
    }

    /// The access modes the entry under edit may use.
    pub(crate) fn access_choices(&self) -> Vec<MountAccess> {
        access_choices(self.overlay_unavailable().is_some())
    }

    /// Settle the access mode after the host reported the overlay unusable.
    pub(crate) fn forbid_overlay(&mut self) {
        self.access = self.access.without_overlay();
    }

    fn add_validated_mount(&mut self) {
        let mount = AdditionalMount {
            source: self.source.to_string().into(),
            destination: self.destination.to_string().into(),
            access: self.access,
        };
        if let Some(index) = self.editing_mount.take() {
            self.mounts[index] = mount;
        } else {
            self.mounts.push(mount);
        }
        self.source.clear();
        self.destination.clear();
        self.access = MountAccess::Ro;
        self.completion_candidates.clear();
        self.error = None;
    }
}

impl NewWizard {
    /// The harness kind of the profile the wizard has selected.
    pub(crate) fn selected_profile_kind(&self, config: &Config) -> Option<HarnessKind> {
        config
            .profiles
            .get(&nth_enabled_profile(config, self.profile))
            .map(|profile| profile.kind)
    }

    /// Only Claude and Codex receive Mjolnir's delegation tools, so only they
    /// get the choice.
    pub(crate) fn subagent_choice_applies(&self, config: &Config) -> bool {
        matches!(
            self.selected_profile_kind(config),
            Some(HarnessKind::Claude | HarnessKind::Codex)
        )
    }

    fn selected_worktree_options(
        &self,
        config: &Config,
    ) -> Option<mj_core::state::ManagedWorktreeOptions> {
        let target_id = nth_key(&config.targets, self.target);
        self.worktree_options
            .as_ref()
            .and_then(|(target, directory, options)| {
                (target == &target_id && directory == self.project_directory.trim())
                    .then_some(*options)
            })
    }

    pub(crate) fn prepare_dialog_state(&mut self) {
        let form = self.form.get_mut();
        form.set_dismiss_actions(&[WizardControl::Cancel, WizardControl::Back]);
        form.set_escape_action(
            (self.step == WizardStep::Mounts || self.step == WizardStep::NewBundle)
                .then_some(WizardControl::Back),
        );
        form.set_dismissal_scope(WizardControl::Back, "attachment editor");
        form.track_draft_part("attachments", vec![format!("{:?}", self.mounts.mounts)]);
        if self.step == WizardStep::Mounts {
            form.track_draft_part(
                "attachment editor",
                vec![
                    self.mounts.source.to_string(),
                    self.mounts.destination.to_string(),
                    format!("{:?}", self.mounts.access),
                ],
            );
        }
        if self.step == WizardStep::ProjectDirectory {
            form.track_draft_part("project", vec![self.project_directory.to_string()]);
        }
        if self.step == WizardStep::NewBundle {
            form.track_draft_part(
                "repositories",
                vec![
                    self.new_bundle_source.to_string(),
                    format!("{:?}", self.new_bundle_repositories),
                ],
            );
        }
    }

    pub(crate) fn text_input_focused(&self) -> bool {
        if let Some(id) = self.form.borrow().focused() {
            return match self.step {
                WizardStep::ProjectDirectory => id == WizardControl::ProjectDirectory,
                WizardStep::NewBundle => {
                    id == WizardControl::NewBundleSource && !self.bundle_creation_in_flight
                }
                WizardStep::Mounts => matches!(
                    id,
                    WizardControl::MountSource | WizardControl::MountDestination
                ),
                _ => false,
            };
        }
        matches!(
            self.step,
            WizardStep::ProjectDirectory | WizardStep::NewBundle | WizardStep::Mounts
        )
    }
}

impl ResumeWizard {
    pub(crate) fn prepare_dialog_state(&mut self) {
        let form = self.form.get_mut();
        form.track_draft_part("queued work", vec![self.discard_queue.to_string()]);
        form.set_dismiss_actions(&[WizardControl::Cancel, WizardControl::Back]);
        form.set_escape_action(
            (self.step == WizardStep::Mounts || self.step == WizardStep::NewBundle)
                .then_some(WizardControl::Back),
        );
        form.set_dismissal_scope(WizardControl::Back, "attachment editor");
        form.track_draft_part("attachments", vec![format!("{:?}", self.mounts.mounts)]);
        if self.step == WizardStep::Mounts {
            form.track_draft_part(
                "attachment editor",
                vec![
                    self.mounts.source.to_string(),
                    self.mounts.destination.to_string(),
                    format!("{:?}", self.mounts.access),
                ],
            );
        }
    }

    fn has_queued_work(&self, dashboard: &DashboardState) -> bool {
        self.preparation.as_ref().map_or_else(
            || {
                dashboard
                    .session_details
                    .get(&self.session_id)
                    .is_some_and(|detail| !detail.queued_prompts.is_empty())
            },
            |preparation| !preparation.queued_commands.is_empty(),
        )
    }

    fn can_advance_target(&self, dashboard: &DashboardState) -> bool {
        let target_id = nth_key(&dashboard.config.targets, self.target);
        dashboard
            .resume_target_rejection(&self.session_id, &target_id)
            .is_none()
            && (self.resource_allocation.is_some()
                || !matches!(
                    dashboard.config.targets.get(&target_id),
                    Some(TargetTemplate::AwsEc2 { .. })
                ))
    }

    pub(crate) fn text_input_focused(&self) -> bool {
        if let Some(id) = self.form.borrow().focused() {
            return self.step == WizardStep::Mounts
                && matches!(
                    id,
                    WizardControl::MountSource | WizardControl::MountDestination
                );
        }
        self.step == WizardStep::Mounts
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ResumeWizard {
    pub(crate) session_id: String,
    /// Resuming stays in the workspace where the dialog was opened even if
    /// the visible tab changes before submission.
    pub(crate) workspace_id: String,
    /// The resume form is also the move form. Move keeps the source workspace
    /// fixed and submits a daemon Move request instead of a plain resume.
    pub(crate) moving: bool,
    pub(crate) preparation: Option<MovePreparation>,
    pub(crate) preparing: bool,
    /// Identity of the preparation request currently owned by this wizard.
    /// It is cleared whenever the draft changes or the wizard leaves review.
    pub(crate) preparation_request_id: Option<u64>,
    /// A failed preparation remains visible in the review modal so retry is
    /// an explicit, single action.
    pub(crate) preparation_error: Option<String>,
    pub(crate) step: WizardStep,

    pub(crate) profile: usize,
    pub(crate) target: usize,
    pub(crate) mounts: MountWizard,

    pub(crate) resource_allocation: Option<SessionResourceAllocation>,
    aws_options: BTreeMap<String, Vec<SessionResourceAllocation>>,
    pub(crate) sizing_error: Option<String>,
    pub(crate) discard_queue: bool,
    pub(crate) form: RefCell<Dialog<WizardControl>>,
}

impl PartialEq for ResumeWizard {
    fn eq(&self, other: &Self) -> bool {
        self.session_id == other.session_id
            && self.workspace_id == other.workspace_id
            && self.moving == other.moving
            && self.preparation == other.preparation
            && self.preparing == other.preparing
            && self.preparation_request_id == other.preparation_request_id
            && self.preparation_error == other.preparation_error
            && self.step == other.step
            && self.profile == other.profile
            && self.target == other.target
            && self.mounts == other.mounts
            && self.resource_allocation == other.resource_allocation
            && self.aws_options == other.aws_options
            && self.sizing_error == other.sizing_error
            && self.discard_queue == other.discard_queue
    }
}

impl Eq for ResumeWizard {}

fn remove_selected_mount(mounts: &mut MountWizard) {
    if mounts.mounts.is_empty() {
        return;
    }
    mounts.mounts.remove(mounts.history_index);
    mounts.history_index = mounts
        .history_index
        .min(mounts.mounts.len().saturating_sub(1));
}

fn prepare_mount_editor(step: &mut WizardStep, mounts: &mut MountWizard) {
    mounts.source.clear();
    mounts.destination.clear();
    mounts.access = MountAccess::Ro;
    mounts.error = None;
    mounts.editing_mount = None;
    mounts.completion_candidates.clear();
    *step = WizardStep::Mounts;
}

fn prepare_selected_mount_editor(step: &mut WizardStep, mounts: &mut MountWizard) {
    if mounts.mounts.is_empty() {
        return;
    }
    let index = mounts.history_index;
    let mount = mounts.mounts[index].clone();
    mounts.source = mount.source.to_string_lossy().into_owned().into();
    mounts.destination = mount.destination.to_string_lossy().into_owned().into();
    mounts.access = mount.access;
    if mounts.overlay_unavailable().is_some() {
        mounts.forbid_overlay();
    }
    mounts.error = None;
    mounts.editing_mount = Some(index);
    mounts.completion_candidates.clear();
    *step = WizardStep::Mounts;
}

fn begin_mount_editor(wizard: &mut NewWizard) {
    prepare_mount_editor(&mut wizard.step, &mut wizard.mounts);
    wizard.form.get_mut().forget_draft_part("attachment editor");
    wizard.form.get_mut().focus(WizardControl::MountSource);
}

fn edit_selected_mount(wizard: &mut NewWizard) {
    prepare_selected_mount_editor(&mut wizard.step, &mut wizard.mounts);
    wizard.form.get_mut().forget_draft_part("attachment editor");
    wizard.form.get_mut().focus(WizardControl::MountSource);
}

fn begin_resume_mount_editor(wizard: &mut ResumeWizard) {
    prepare_mount_editor(&mut wizard.step, &mut wizard.mounts);
    wizard.form.get_mut().forget_draft_part("attachment editor");
    wizard.form.get_mut().focus(WizardControl::MountSource);
}

fn edit_selected_resume_mount(wizard: &mut ResumeWizard) {
    prepare_selected_mount_editor(&mut wizard.step, &mut wizard.mounts);
    wizard.form.get_mut().forget_draft_part("attachment editor");
    wizard.form.get_mut().focus(WizardControl::MountSource);
}

fn validate_mount_entry(mounts: &MountWizard) -> Option<String> {
    if let Err(error) =
        mj_core::path_input::validate_absolute_input(std::path::Path::new(mounts.source.trim()))
    {
        return Some(error.to_string());
    }
    let mount = AdditionalMount {
        source: mounts.source.to_string().into(),
        destination: mounts.destination.to_string().into(),
        access: mounts.access,
    };
    if let Err(error) = mj_core::targets::validate_mount_destination(&mount.destination) {
        return Some(error.to_string());
    }
    let duplicate = mounts.mounts.iter().enumerate().any(|(index, existing)| {
        Some(index) != mounts.editing_mount && existing.destination == mount.destination
    });
    duplicate.then(|| {
        format!(
            "{} is already an attached directory destination.",
            mount.destination.display()
        )
    })
}

pub(crate) fn clamp_resources(
    cpus: u64,
    memory_bytes: u64,
    limits: Option<(u64, u64)>,
) -> (u64, u64) {
    let Some((max_cpus, max_memory)) = limits else {
        return (cpus.max(1), memory_bytes.max(1));
    };
    (
        cpus.min(max_cpus.max(1)),
        memory_bytes.min(max_memory.max(1)),
    )
}

fn preferred_aws_option<'a>(
    options: &'a [SessionResourceAllocation],
    previous: Option<&SessionResourceAllocation>,
) -> Option<&'a SessionResourceAllocation> {
    if let Some(SessionResourceAllocation::AwsEc2 { instance_type, .. }) = previous
        && let Some(option) = options.iter().find(|option| {
            matches!(option, SessionResourceAllocation::AwsEc2 { instance_type: candidate, .. } if candidate == instance_type)
        })
    {
        return Some(option);
    }
    options.iter().find(|option| allocation_cpus(option) == 8)
}

fn apply_aws_options(
    target_id: &str,
    result: std::result::Result<Vec<SessionResourceAllocation>, String>,
    options_by_target: &mut BTreeMap<String, Vec<SessionResourceAllocation>>,
    allocation: &mut Option<SessionResourceAllocation>,
    sizing_error: &mut Option<String>,
    previous: Option<&SessionResourceAllocation>,
) {
    match result {
        Ok(options) => {
            *allocation = preferred_aws_option(&options, previous).cloned();
            options_by_target.insert(target_id.to_owned(), options);
            *sizing_error = None;
        }
        Err(error) => {
            *allocation = None;
            *sizing_error = Some(error);
        }
    }
}

fn adjust_resources(
    allocation: &mut Option<SessionResourceAllocation>,
    aws_options: Option<&Vec<SessionResourceAllocation>>,
    limits: Option<(u64, u64)>,
    code: KeyCode,
) {
    let Some(current) = allocation.clone() else {
        return;
    };
    match current {
        SessionResourceAllocation::Container { cpus, memory_bytes } => {
            let next = match code {
                KeyCode::Char('r') => clamp_resources(BASELINE_CPUS, BASELINE_MEMORY_BYTES, limits),
                KeyCode::Char('+') => {
                    let Some((max_cpus, max_memory)) = limits else {
                        return;
                    };
                    (
                        cpus.saturating_mul(2).min(max_cpus.max(1)),
                        memory_bytes.saturating_mul(2).min(max_memory.max(1)),
                    )
                }
                KeyCode::Char('c') => {
                    let Some((max_cpus, _)) = limits else {
                        return;
                    };
                    (cpus.saturating_add(8).min(max_cpus.max(1)), memory_bytes)
                }
                KeyCode::Char('m') => {
                    let Some((_, max_memory)) = limits else {
                        return;
                    };
                    (
                        cpus,
                        memory_bytes
                            .saturating_add(memory_bytes / 2)
                            .min(max_memory.max(1)),
                    )
                }
                KeyCode::Char('-') => {
                    let next_cpus = if cpus > FLOOR_CPUS {
                        (cpus / 2).max(FLOOR_CPUS)
                    } else {
                        cpus
                    };
                    let next_memory = if memory_bytes > FLOOR_MEMORY_BYTES {
                        (memory_bytes / 2).max(FLOOR_MEMORY_BYTES)
                    } else {
                        memory_bytes
                    };
                    (next_cpus, next_memory)
                }
                _ => return,
            };
            *allocation = Some(SessionResourceAllocation::Container {
                cpus: next.0,
                memory_bytes: next.1,
            });
        }
        SessionResourceAllocation::AwsEc2 {
            vcpus,
            memory_bytes,
            ..
        } => {
            let Some(options) = aws_options else {
                return;
            };
            let desired = match code {
                KeyCode::Char('+') => (Some(vcpus.saturating_mul(2)), None),
                KeyCode::Char('-') if vcpus > 1 => (Some(vcpus / 2), None),
                KeyCode::Char('r') => (Some(BASELINE_CPUS), None),
                KeyCode::Char('c') => (Some(vcpus.saturating_mul(2)), Some(memory_bytes)),
                KeyCode::Char('m') => (Some(vcpus), Some(memory_bytes.saturating_mul(2))),
                _ => return,
            };
            if let Some(next) = options.iter().find(|option| {
                desired.0.is_none_or(|cpus| allocation_cpus(option) == cpus)
                    && desired
                        .1
                        .is_none_or(|memory| allocation_memory(option) == memory)
            }) {
                *allocation = Some(next.clone());
            }
        }
    }
}

impl NewWizard {
    fn add_new_bundle_repository(&mut self) -> bool {
        let source = self.new_bundle_source.trim();
        if source.is_empty() {
            return false;
        }
        self.new_bundle_repositories.push(source.to_owned());
        self.new_bundle_selected = self.new_bundle_repositories.len() - 1;
        self.new_bundle_source.clear();
        self.form.get_mut().focus(WizardControl::NewBundleSource);
        true
    }

    fn remove_selected_new_bundle_repository(&mut self) -> bool {
        if self.new_bundle_repositories.is_empty() {
            return false;
        }
        self.new_bundle_repositories.remove(
            self.new_bundle_selected
                .min(self.new_bundle_repositories.len() - 1),
        );
        self.new_bundle_selected = self
            .new_bundle_selected
            .min(self.new_bundle_repositories.len().saturating_sub(1));
        self.form
            .get_mut()
            .focus(if self.new_bundle_repositories.is_empty() {
                WizardControl::NewBundleSource
            } else {
                WizardControl::NewBundleRepositories
            });
        true
    }

    fn new_bundle_sources_for_submit(&self) -> Vec<String> {
        let mut sources = self.new_bundle_repositories.clone();
        let current = self.new_bundle_source.trim();
        if !current.is_empty() {
            sources.push(current.to_owned());
        }
        sources
    }
}

impl ResumeWizard {}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PickerNavigation {
    pub(crate) has_back: bool,
    /// Row the keyboard is on, highlighted while the content has focus.
    pub(crate) selected: usize,
    pub(crate) control: WizardControl,
    pub(crate) next_enabled: bool,
    /// A secondary action pinned to the right edge of the action row, e.g. the
    /// bundle step's "New bundle…" opener.
    pub(crate) pinned_action: Option<(WizardControl, &'static str, bool)>,
    /// Muted line drawn in place of an empty list, so the step never renders a
    /// blank picker.
    pub(crate) empty_hint: Option<&'static str>,
}

/// One cell of a picker table row.
#[derive(Debug, Clone)]
pub(crate) struct PickerCell {
    text: String,
    style: Style,
}

impl PickerCell {
    pub(crate) fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: Style::default(),
        }
    }

    fn styled(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
        }
    }

    /// Empty cell that keeps the columns after it aligned on rows that need no
    /// entry here.
    fn blank() -> Self {
        Self::text("")
    }

    /// The yellow triangle that marks a row the picker footnote explains.
    fn warning_marker() -> Self {
        Self::styled("⚠", Style::default().fg(theme::palette().warning))
    }

    fn width(&self) -> usize {
        Line::raw(self.text.as_str()).width()
    }
}

/// One picker row. A disabled row stays in the list so row numbers keep
/// matching the underlying map order; it is greyed out and refuses Enter.
///
/// Rows of several cells draw as a table: every cell but the row's last is
/// padded to its column, so the columns line up down the list.
#[derive(Debug, Clone)]
pub(crate) struct PickerChoice {
    cells: Vec<PickerCell>,
    disabled: bool,
    /// Heading rows name the columns and carry no item, so the picker leaves
    /// them out of its row map and item indexes keep naming their own rows.
    heading: bool,
}

impl PickerChoice {
    /// A single cell of plain, unstyled text.
    pub(crate) fn text(text: impl Into<String>) -> Self {
        Self::table(vec![PickerCell::text(text)])
    }

    /// A row of table cells padded into aligned columns.
    pub(crate) fn table(cells: Vec<PickerCell>) -> Self {
        Self {
            cells,
            disabled: false,
            heading: false,
        }
    }

    /// A greyed row that keeps its place in the list but refuses Enter.
    pub(crate) fn disabled(text: impl Into<String>) -> Self {
        Self {
            disabled: true,
            ..Self::text(text)
        }
    }

    /// A non-selectable heading row.
    fn heading(cells: Vec<PickerCell>) -> Self {
        Self {
            heading: true,
            ..Self::table(cells)
        }
    }

    /// Appends a trailing note, e.g. the resume step's lossy-transcript
    /// marker, after the padded columns.
    fn with_note(mut self, note: impl Into<String>) -> Self {
        self.cells.push(PickerCell::text(note));
        self
    }

    fn line(&self, widths: &[usize]) -> Line<'static> {
        let mut spans = Vec::new();
        for (index, cell) in self.cells.iter().enumerate() {
            let last = index + 1 == self.cells.len();
            let padding = if last {
                0
            } else {
                widths[index]
                    .saturating_add(COLUMN_GAP)
                    .saturating_sub(cell.width())
            };
            spans.push(Span::styled(cell.text.clone(), cell.style));
            if padding > 0 {
                spans.push(Span::raw(" ".repeat(padding)));
            }
        }
        Line::from(spans)
    }
}

/// Blank cells between adjacent table columns.
const COLUMN_GAP: usize = 2;

/// Widest cell of every column across the rows, so each cell can be padded to
/// its column.
fn picker_columns(choices: &[PickerChoice]) -> Vec<usize> {
    let mut widths: Vec<usize> = Vec::new();
    for choice in choices {
        for (index, cell) in choice.cells.iter().enumerate() {
            let width = cell.width();
            match widths.get_mut(index) {
                Some(column) => *column = (*column).max(width),
                None => widths.push(width),
            }
        }
    }
    widths
}

/// Heading of the profile tables, in the same columns as their rows.
fn profile_headings() -> PickerChoice {
    let style = theme::muted().add_modifier(Modifier::BOLD);
    PickerChoice::heading(vec![
        PickerCell::blank(),
        PickerCell::styled("PROFILE", style),
        PickerCell::styled("HARNESS", style),
        PickerCell::styled("QUOTA", style),
    ])
}

/// Profiles whose harness cannot guard risky actions carry a warning triangle
/// in the table and the footnote `guardian_footnote` draws below it.
fn needs_guardian_warning(harness: HarnessKind) -> bool {
    !harness.supports_guardian_approvals()
}

/// The marker cell of a profile row: the warning triangle for a harness that
/// cannot guard risky actions, and a blank cell that keeps the columns aligned
/// for one that can.
pub(crate) fn guardian_warning_marker(harness: HarnessKind) -> PickerCell {
    if needs_guardian_warning(harness) {
        PickerCell::warning_marker()
    } else {
        PickerCell::blank()
    }
}

/// The row below a profile table that explains its warning triangles.
fn guardian_footnote() -> Line<'static> {
    Line::from(vec![
        Span::styled("⚠  ", Style::default().fg(theme::palette().warning)),
        Span::styled(
            "No guardian approval mode; do not run on a raw, unsandboxed target.",
            theme::muted(),
        ),
    ])
}

/// The profile tables of the new-session and resume wizards: the column
/// headings followed by `rows`. An empty list keeps its hint instead of
/// rendering a bare heading.
fn profile_table(rows: Vec<PickerChoice>) -> Vec<PickerChoice> {
    if rows.is_empty() {
        return rows;
    }
    let mut table = Vec::with_capacity(rows.len() + 1);
    table.push(profile_headings());
    table.extend(rows);
    table
}

/// Muted help row of a picker step.
fn picker_help(text: &str) -> Line<'static> {
    Line::styled(text.to_owned(), theme::muted())
}

// The form and surface registry are distinct rendering owners.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_picker(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    choices: Vec<PickerChoice>,
    help: Vec<Line<'static>>,
    navigation: PickerNavigation,
    form: &mut Dialog<WizardControl>,
    surfaces: &mut FrameSurfaces,
) {
    let width_percent = if area.width < 64 { 100 } else { 68 };
    let popup = centered_modal(
        frame,
        surfaces,
        width_percent,
        (choices.len() as u16 + help.len() as u16 + 6).clamp(9, 19),
        area,
    );
    let content = popup.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 1,
    });
    let list_height = u16::try_from(choices.len())
        .unwrap_or(u16::MAX)
        .max(u16::from(
            choices.is_empty() && navigation.empty_hint.is_some(),
        ))
        .min(content.height.saturating_sub(help.len() as u16 + 2));
    let list_area = Rect::new(content.x, content.y, content.width, list_height);
    let widths = picker_columns(&choices);
    let rows = choices
        .iter()
        .map(|choice| choice.line(&widths))
        .collect::<Vec<_>>();
    let mut row_map = Vec::with_capacity(choices.len());
    let mut items = 0usize;
    for choice in &choices {
        if choice.heading {
            row_map.push(None);
        } else {
            row_map.push(Some(items));
            items += 1;
        }
    }
    let row_enabled = choices
        .iter()
        .map(|choice| !choice.disabled)
        .collect::<Vec<_>>();
    let help_y = list_area.y.saturating_add(list_area.height);
    let help_height = (help.len() as u16).min(content.bottom().saturating_sub(help_y + 1));
    let help_area = Rect::new(content.x, help_y, content.width, help_height);
    let button_area = mj_chat::components::DialogShell::layout(content, 0).actions;
    let title_line = dismissible_modal_title(form, popup, title.trim(), theme::title(true), true);
    frame.render_widget(theme::modal().title(title_line), popup);
    ChoiceList::render_with_rows(
        frame,
        list_area,
        &rows,
        navigation.selected,
        &row_map,
        &row_enabled,
        form,
        navigation.control,
    );
    if choices.is_empty()
        && let Some(hint) = navigation.empty_hint
    {
        frame.render_widget(
            Paragraph::new(Line::styled(
                hint,
                Style::default().fg(theme::palette().muted),
            )),
            list_area,
        );
    }
    frame.render_widget(Paragraph::new(help), help_area);
    let mut buttons = vec![(WizardControl::Cancel, "Cancel", true)];
    if navigation.has_back {
        buttons.push((WizardControl::Back, "Back", true));
    }
    buttons.push((WizardControl::Next, "Next", navigation.next_enabled));
    let row_width = |buttons: &[(WizardControl, &str, bool)]| {
        buttons
            .iter()
            .map(|(_, label, _)| Line::raw(*label).width() + 4)
            .sum::<usize>()
            .saturating_add(buttons.len().saturating_sub(1))
    };
    match navigation.pinned_action {
        // The pinned action keeps its own right-aligned row when it fits next
        // to the navigation buttons, matching the Workspaces action row.
        Some(pinned)
            if row_width(&buttons) + 1 + Line::raw(pinned.1).width() + 4
                <= usize::from(button_area.width) =>
        {
            Dialog::render_actions(frame, button_area, &buttons, form);
            mj_chat::components::ButtonRow::render_aligned(
                frame,
                button_area,
                &[pinned],
                form,
                mj_chat::components::RowAlign::Right,
            );
        }
        pinned => {
            if let Some(pinned) = pinned {
                buttons.insert(buttons.len() - 1, pinned);
            }
            Dialog::render_actions(frame, button_area, &buttons, form);
        }
    }
}

fn step_initial(step: WizardStep) -> WizardControl {
    match step {
        WizardStep::Profile => WizardControl::ProfileList,
        WizardStep::Target => WizardControl::TargetList,
        WizardStep::Bundle => WizardControl::BundleList,
        WizardStep::ProjectDirectory => WizardControl::ProjectDirectory,
        WizardStep::NewBundle => WizardControl::NewBundleSource,
        WizardStep::Mounts => WizardControl::MountSource,
        WizardStep::Review => WizardControl::Submit,
    }
}

fn begin_form_frame(form: &mut Dialog<WizardControl>, _initial: WizardControl) {
    form.begin_frame();
}

pub(crate) fn render_new_wizard(
    frame: &mut Frame,
    area: Rect,
    dashboard: &DashboardState,
    wizard: &NewWizard,
    surfaces: &mut FrameSurfaces,
) {
    let mut form = wizard.form.borrow_mut();
    let initial = step_initial(wizard.step);
    begin_form_frame(&mut form, initial);
    if wizard.step == WizardStep::Review {
        let target_id = nth_key(&dashboard.config.targets, wizard.target);
        let raw_project = is_bare_project_target(&dashboard.config.targets[&target_id]);
        let bundle_id = (!raw_project)
            .then(|| nth_bundle_key(&dashboard.config, &dashboard.state, wizard.bundle));
        render_review_wizard(
            frame,
            area,
            dashboard,
            ReviewWizardView {
                subagents: wizard
                    .subagent_choice_applies(&dashboard.config)
                    .then_some(wizard.mjolnir_subagents),
                // Isolated targets always provide the workspace, so the choice
                // only exists for a bare project directory.
                worktree: raw_project.then(|| {
                    (
                        wizard.create_managed_worktree,
                        wizard
                            .selected_worktree_options(&dashboard.config)
                            .is_some_and(|options| options.available),
                    )
                }),
                profile_id: &nth_enabled_profile(&dashboard.config, wizard.profile),
                project_label: if raw_project {
                    "Project directory"
                } else {
                    "Project"
                },
                project: if raw_project {
                    wizard.project_directory.trim()
                } else {
                    bundle_id.as_deref().expect("bundle selected")
                },
                project_note: "",
                target_id: &target_id,
                allocation: wizard.resource_allocation.as_ref(),
                mounts: &wizard.mounts,
                title: " New session · 4/4 review ",
                submit_label: if wizard.remote_preflight_error.is_some() {
                    "Retry"
                } else {
                    "Create"
                },
                moving: false,
                preparing: false,
                preparation_error: None,
                submit_enabled: !raw_project
                    || wizard
                        .selected_worktree_options(&dashboard.config)
                        .is_some()
                    || wizard.remote_preflight_error.is_some(),
                active_interruption: false,
                source_unavailable: false,
                clear_resource_allocation: false,
                queue: None,
                queued_entries: &[],
                prepared_entries: &[],
                remote_repositories: wizard.remote_repositories.as_deref(),
                remote_preflight_in_flight: wizard.remote_preflight_in_flight,
                remote_preflight_error: wizard.remote_preflight_error.as_deref(),
                local_changes_excluded: !raw_project,
                conversion: None,
            },
            &mut form,
            surfaces,
        );
        form.end_frame(initial);
        return;
    }
    if wizard.step == WizardStep::ProjectDirectory {
        let target_id = nth_key(&dashboard.config.targets, wizard.target);
        let local = matches!(
            dashboard.config.targets[&target_id],
            TargetTemplate::LocalBare
        );
        let mut lines = vec![
            Line::raw(if local {
                "Absolute project directory on this machine:"
            } else {
                "Absolute project directory on the remote machine:"
            }),
            Line::raw(""),
        ];
        if let Some(error) = &wizard.project_directory_error {
            lines.push(Line::styled(
                format!("Error: {error}"),
                Style::default().fg(theme::palette().error),
            ));
            lines.push(Line::raw(""));
        }
        if !wizard.project_history.is_empty() {
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                "Recent on this host (↑/↓ selects):",
                Style::default().fg(theme::palette().muted),
            ));
            lines.extend(wizard.project_history.iter().take(5).enumerate().map(
                |(index, directory)| {
                    Line::styled(
                        format!(
                            "{} {}",
                            if index == wizard.project_history_index {
                                "›"
                            } else {
                                " "
                            },
                            directory.display()
                        ),
                        if index == wizard.project_history_index {
                            Style::default().fg(theme::palette().text)
                        } else {
                            Style::default().fg(theme::palette().muted)
                        },
                    )
                },
            ));
        }
        lines.push(Line::styled(
            "Enter validates · Tab moves · Back returns · Esc cancels",
            Style::default().fg(theme::palette().muted),
        ));
        let popup = centered_modal(
            frame,
            surfaces,
            76,
            (lines.len() as u16 + 2).clamp(9, 16),
            area,
        );
        let content = popup.inner(ratatui::layout::Margin {
            horizontal: 1,
            vertical: 1,
        });
        let title_line = dismissible_modal_title(
            &mut form,
            popup,
            if local {
                "New session · 3/4 local project"
            } else {
                "New session · 3/4 remote project"
            },
            theme::title(true),
            true,
        );
        frame.render_widget(theme::modal().title(title_line), popup);
        let intro = lines.iter().take(2).cloned().collect::<Vec<_>>();
        let details = lines.iter().skip(2).cloned().collect::<Vec<_>>();
        frame.render_widget(
            Paragraph::new(intro),
            Rect::new(content.x, content.y, content.width, 2.min(content.height)),
        );
        let field_y = content.y.saturating_add(2);
        let button_y = content.bottom().saturating_sub(1);
        frame.render_widget(
            Paragraph::new(details),
            Rect::new(
                content.x,
                field_y.saturating_add(1),
                content.width,
                button_y.saturating_sub(field_y.saturating_add(1)),
            ),
        );
        PathField::render(
            frame,
            Rect::new(content.x, field_y, content.width, 1.min(content.height)),
            &wizard.project_directory,
            &mut form,
            WizardControl::ProjectDirectory,
        );
        Dialog::render_actions(
            frame,
            mj_chat::components::DialogShell::layout(content, 0).actions,
            &[
                (WizardControl::Cancel, "Cancel", true),
                (WizardControl::Back, "Back", true),
                (WizardControl::Next, "Next", true),
            ],
            &mut form,
        );
        form.end_frame(initial);
        return;
    }
    if wizard.step == WizardStep::Mounts {
        render_mount_wizard(
            frame,
            area,
            dashboard,
            wizard.target,
            &wizard.mounts,
            &mut form,
            " Add attached directory ",
            surfaces,
        );
        form.end_frame(initial);
        return;
    }
    if wizard.step == WizardStep::NewBundle {
        let popup_height = u16::try_from(wizard.new_bundle_repositories.len())
            .unwrap_or(u16::MAX)
            .saturating_add(8)
            .clamp(10, 24);
        let popup = centered_modal(frame, surfaces, 76, popup_height, area);
        let content = popup.inner(ratatui::layout::Margin {
            horizontal: 1,
            vertical: 1,
        });
        let title_line = dismissible_modal_title(
            &mut form,
            popup,
            "New bundle",
            theme::title(true),
            !wizard.bundle_creation_in_flight,
        );
        frame.render_widget(theme::modal().title(title_line), popup);
        frame.render_widget(
            Paragraph::new("Repositories (first is primary):"),
            Rect::new(content.x, content.y, content.width, 1.min(content.height)),
        );
        let list_y = content.y.saturating_add(1);
        let list_height = if wizard.new_bundle_repositories.is_empty() {
            1.min(content.height.saturating_sub(5))
        } else {
            u16::try_from(wizard.new_bundle_repositories.len())
                .unwrap_or(u16::MAX)
                .min(content.height.saturating_sub(6))
        };
        let list_area = Rect::new(content.x, list_y, content.width, list_height);
        if wizard.new_bundle_repositories.is_empty() {
            frame.render_widget(
                Paragraph::new(Line::styled(
                    "No repositories added yet.",
                    Style::default().fg(theme::palette().muted),
                )),
                list_area,
            );
        } else {
            let rows = wizard
                .new_bundle_repositories
                .iter()
                .enumerate()
                .map(|(index, source)| {
                    if index == 0 {
                        Line::raw(format!("primary  {source}"))
                    } else {
                        Line::raw(format!("         {source}"))
                    }
                })
                .collect::<Vec<_>>();
            ChoiceList::render(
                frame,
                list_area,
                &rows,
                wizard.new_bundle_selected,
                &mut form,
                WizardControl::NewBundleRepositories,
            );
            if wizard.bundle_creation_in_flight {
                form.declare_with_enabled(
                    WizardControl::NewBundleRepositories,
                    ControlKind::ChoiceList {
                        len: wizard.new_bundle_repositories.len(),
                        selected: wizard.new_bundle_selected,
                    },
                    false,
                );
            }
        }
        let source_label_y = list_y.saturating_add(list_height);
        frame.render_widget(
            Paragraph::new("GitHub source or local Git path with a network remote:"),
            Rect::new(
                content.x,
                source_label_y,
                content.width,
                1.min(content.height),
            ),
        );
        PathField::render(
            frame,
            Rect::new(
                content.x,
                source_label_y.saturating_add(1),
                content.width,
                1.min(content.height),
            ),
            &wizard.new_bundle_source,
            &mut form,
            WizardControl::NewBundleSource,
        );
        let help_y = content.bottom().saturating_sub(3);
        frame.render_widget(
            Paragraph::new(Line::styled(
                if wizard.bundle_creation_in_flight {
                    "Creating bundle…"
                } else {
                    "Enter adds · Delete removes · Tab moves focus · Esc cancels"
                },
                Style::default().fg(theme::palette().muted),
            )),
            Rect::new(content.x, help_y, content.width, 1.min(content.height)),
        );
        let action_enabled =
            !wizard.bundle_creation_in_flight && !wizard.new_bundle_source.trim().is_empty();
        Dialog::render_actions(
            frame,
            Rect::new(
                content.x,
                content.bottom().saturating_sub(2),
                content.width,
                1.min(content.height),
            ),
            &[
                (WizardControl::Add, "Add repository", action_enabled),
                (
                    WizardControl::NewBundleRemove,
                    "Remove selected repository",
                    !wizard.bundle_creation_in_flight && !wizard.new_bundle_repositories.is_empty(),
                ),
            ],
            &mut form,
        );
        Dialog::render_actions(
            frame,
            Rect::new(
                content.x,
                content.bottom().saturating_sub(1),
                content.width,
                1.min(content.height),
            ),
            &[
                (
                    WizardControl::Cancel,
                    "Cancel",
                    !wizard.bundle_creation_in_flight,
                ),
                (
                    WizardControl::Back,
                    "Back",
                    !wizard.bundle_creation_in_flight,
                ),
                (
                    WizardControl::Next,
                    if wizard.bundle_creation_in_flight {
                        "Creating…"
                    } else {
                        "Create bundle"
                    },
                    !wizard.bundle_creation_in_flight
                        && !wizard.new_bundle_sources_for_submit().is_empty(),
                ),
            ],
            &mut form,
        );
        form.end_frame(initial);
        return;
    }
    let (title, choices, selected): (_, Vec<PickerChoice>, _) = match wizard.step {
        WizardStep::Profile => (
            " New session · 1/4 profile ",
            profile_table(
                dashboard
                    .config
                    .enabled_profiles()
                    .map(|(id, profile)| dashboard.profile_choice(id, profile.kind))
                    .collect(),
            ),
            wizard.profile,
        ),
        WizardStep::Bundle => (
            " New session · 3/4 project bundle ",
            bundle_ids_by_recent_creation(&dashboard.config, &dashboard.state)
                .into_iter()
                .map(|id| {
                    let bundle = &dashboard.config.bundles[id];
                    PickerChoice::text(format!("{id}  {} repositories", bundle.repositories.len()))
                })
                .collect(),
            wizard.bundle,
        ),
        WizardStep::Target => (
            " New session · 2/4 target ",
            dashboard
                .config
                .targets
                .iter()
                .map(|(id, target)| {
                    let size = if id == &nth_key(&dashboard.config.targets, wizard.target) {
                        resource_allocation_label(
                            wizard.resource_allocation.as_ref(),
                            wizard.sizing_error.as_deref(),
                        )
                    } else {
                        String::new()
                    };
                    let label = format!("{id}  {}{size}", target_label(target));
                    match dashboard.target_readiness_rejection(id) {
                        Some(reason) => PickerChoice::disabled(format!("{label} · {reason}")),
                        None => PickerChoice::text(label),
                    }
                })
                .collect(),
            wizard.target,
        ),
        WizardStep::Review => unreachable!("review was rendered above"),
        WizardStep::Mounts => unreachable!("mount input was rendered above"),
        WizardStep::NewBundle => unreachable!("bundle input was rendered above"),
        WizardStep::ProjectDirectory => unreachable!("project directory input was rendered above"),
    };
    let mut help = vec![if wizard.step == WizardStep::Target {
        picker_help("+ double · - halve · c +8 CPU · m +50% memory · r reset · F5 recheck")
    } else {
        picker_help("↑/↓ select · Tab moves focus · Enter activates")
    }];
    if wizard.step == WizardStep::Profile
        && dashboard
            .config
            .enabled_profiles()
            .any(|(_, profile)| needs_guardian_warning(profile.kind))
    {
        help.push(guardian_footnote());
    }
    render_picker(
        frame,
        area,
        title,
        choices,
        help,
        PickerNavigation {
            has_back: wizard.step != WizardStep::Profile,
            selected,
            control: match wizard.step {
                WizardStep::Profile => WizardControl::ProfileList,
                WizardStep::Bundle => WizardControl::BundleList,
                WizardStep::Target => WizardControl::TargetList,
                _ => unreachable!("picker step has a list control"),
            },
            next_enabled: match wizard.step {
                WizardStep::Target => {
                    dashboard
                        .target_readiness_rejection(&nth_key(
                            &dashboard.config.targets,
                            wizard.target,
                        ))
                        .is_none()
                        && (wizard.resource_allocation.is_some()
                            || !matches!(
                                dashboard
                                    .config
                                    .targets
                                    .get(&nth_key(&dashboard.config.targets, wizard.target)),
                                Some(TargetTemplate::AwsEc2 { .. })
                            ))
                }
                // Without a bundle there is nothing to review; the pinned
                // action is the only way forward.
                WizardStep::Bundle => !dashboard.config.bundles.is_empty(),
                _ => true,
            },
            pinned_action: (wizard.step == WizardStep::Bundle).then_some((
                WizardControl::Add,
                "New bundle…",
                true,
            )),
            empty_hint: (wizard.step == WizardStep::Bundle && dashboard.config.bundles.is_empty())
                .then_some("No bundles yet."),
        },
        &mut form,
        surfaces,
    );
    form.end_frame(match wizard.step {
        WizardStep::Profile => WizardControl::ProfileList,
        WizardStep::Bundle => WizardControl::BundleList,
        WizardStep::Target => WizardControl::TargetList,
        _ => unreachable!("picker step has a list control"),
    });
}

struct ReviewWizardView<'a> {
    worktree: Option<(bool, bool)>,
    /// `Some(checked)` shows the Mjolnir sub-agent checkbox; `None` hides it.
    subagents: Option<bool>,
    pub(crate) profile_id: &'a str,
    pub(crate) project_label: &'a str,
    pub(crate) project: &'a str,
    pub(crate) project_note: &'a str,
    pub(crate) target_id: &'a str,
    pub(crate) allocation: Option<&'a SessionResourceAllocation>,
    pub(crate) mounts: &'a MountWizard,
    pub(crate) title: &'a str,
    submit_label: &'a str,
    moving: bool,
    preparing: bool,
    preparation_error: Option<&'a str>,
    submit_enabled: bool,
    active_interruption: bool,
    source_unavailable: bool,
    clear_resource_allocation: bool,
    queue: Option<(usize, bool)>,
    queued_entries: &'a [mj_core::relay::QueuedPrompt],
    prepared_entries: &'a [MaterializedQueuedPrompt],
    remote_repositories: Option<&'a [RemoteRepositoryPreview]>,
    remote_preflight_in_flight: bool,
    remote_preflight_error: Option<&'a str>,
    local_changes_excluded: bool,
    /// Present only when a move converts a local checkout into an isolated
    /// workspace, so the review can say what travels before the confirmation.
    conversion: Option<&'a mj_core::state::RawConversionPreview>,
}

fn render_review_wizard(
    frame: &mut Frame,
    area: Rect,
    dashboard: &DashboardState,
    view: ReviewWizardView<'_>,
    form: &mut Dialog<WizardControl>,
    surfaces: &mut FrameSurfaces,
) {
    let ReviewWizardView {
        worktree,
        subagents,
        profile_id,
        project_label,
        project,
        project_note,
        target_id,
        allocation,
        mounts,
        title,
        submit_label,
        moving,
        preparing,
        preparation_error,
        submit_enabled,
        active_interruption,
        source_unavailable,
        clear_resource_allocation,
        queue,
        queued_entries,
        prepared_entries,
        remote_repositories,
        remote_preflight_in_flight,
        remote_preflight_error,
        local_changes_excluded,
        conversion,
    } = view;
    let target = &dashboard.config.targets[target_id];
    let can_attach = mount_history_host(target).is_some();
    let mut lines = vec![
        Line::from(vec![
            Span::styled("Profile: ", theme::muted()),
            Span::styled(
                profile_id,
                Style::default()
                    .fg(theme::palette().secondary)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled(format!("{project_label}: "), theme::muted()),
            Span::styled(
                project,
                Style::default()
                    .fg(theme::palette().text)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(project_note, theme::muted()),
        ]),
        Line::from(vec![
            Span::styled("Target: ", theme::muted()),
            Span::styled(target_id, Style::default().fg(theme::palette().accent)),
            Span::styled(format!(" ({})", target_label(target)), theme::muted()),
        ]),
        Line::from(vec![
            Span::styled("Compute:", theme::muted()),
            Span::raw(resource_allocation_label(allocation, None)),
        ]),
    ];
    if moving && source_unavailable {
        lines.push(Line::styled(
            "Source is unavailable; Move will recover its saved data without starting its old harness.",
            Style::default().fg(theme::palette().warning),
        ));
    }
    if moving && active_interruption {
        lines.push(Line::styled(
            "Active work will be interrupted; the session is restored into a fresh environment.",
            Style::default().fg(theme::palette().warning),
        ));
        if clear_resource_allocation {
            lines.push(Line::styled(
                "Fixed/default destination resources will replace the source sizing.",
                Style::default().fg(theme::palette().warning),
            ));
        }
    }
    if let Some(conversion) = conversion.filter(|_| moving) {
        lines.push(Line::raw(conversion.summary_line()));
        for warning in conversion.warning_lines() {
            lines.push(Line::styled(
                warning,
                Style::default().fg(theme::palette().warning),
            ));
        }
    }
    if moving {
        if preparing {
            lines.push(Line::styled(
                "Checking move destination…",
                Style::default().fg(theme::palette().muted),
            ));
        } else if let Some(error) = preparation_error {
            lines.push(Line::styled(
                format!("Move preparation failed: {error}"),
                Style::default().fg(theme::palette().error),
            ));
            lines.push(Line::styled(
                "Press Retry to check the destination again.",
                Style::default().fg(theme::palette().muted),
            ));
        }
    }
    if remote_preflight_in_flight {
        lines.push(Line::styled(
            "Checking prerequisites…",
            Style::default().fg(theme::palette().muted),
        ));
    } else if let Some(error) = remote_preflight_error {
        lines.push(Line::styled(
            format!("Prerequisite check failed: {error}"),
            Style::default().fg(theme::palette().error),
        ));
    } else if let Some(repositories) = remote_repositories {
        lines.push(Line::styled(
            if local_changes_excluded {
                "Network clone plan (local commits and dirty files excluded):"
            } else {
                "Network clone plan:"
            },
            theme::muted(),
        ));
        for repository in repositories {
            let pushes = if repository.push_urls.is_empty() {
                "none".to_owned()
            } else {
                repository.push_urls.join(", ")
            };
            lines.push(Line::raw(format!(
                "  {}: fetch {} @ {}; push {}",
                repository.repository_id, repository.fetch_url, repository.default_branch, pushes
            )));
        }
    }
    let worktree_row = worktree.map(|(checked, available)| {
        let row = lines.len() as u16;
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            if checked && available {
                "Create a separate session-owned checkout from the selected checkout's HEAD."
            } else {
                "Use the selected directory directly."
            },
            theme::muted(),
        ));
        row
    });
    let subagent_row = subagents.map(|checked| {
        let row = lines.len() as u16;
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            if checked {
                "Delegation goes to Mjolnir sub-agents that share this session's files."
            } else {
                "Unchecked keeps the harness's own Agent or spawn_agent tools."
            },
            theme::muted(),
        ));
        row
    });
    let queue_label = queue.map(|(count, _)| format!("Queued prompts: {count}"));
    if let Some(label) = &queue_label {
        lines.push(Line::raw(label.clone()));
    }
    // Guardian targets rely on the harness's own approval mode rather than
    // Hel-managed isolation.
    if (matches!(target, TargetTemplate::LocalBare)
        || target.permission_mode() == Some(mj_core::config::PermissionMode::Guardian))
        && let Some(kind) = dashboard
            .config
            .profiles
            .get(profile_id)
            .map(|profile| profile.kind)
        && let Some(warning) = kind.unsandboxed_guardian_warning()
    {
        lines.push(Line::styled(
            format!("⚠ {warning}"),
            Style::default()
                .fg(theme::palette().error)
                .add_modifier(Modifier::BOLD),
        ));
    }
    lines.push(Line::raw(""));
    if can_attach {
        lines.push(Line::from(vec![
            Span::styled("Attached directories: ", theme::muted()),
            Span::styled(mounts.mounts.len().to_string(), theme::title(false)),
        ]));
    }
    lines.push(Line::styled(
        if can_attach {
            "Tab moves focus · Enter edits selected directory · Delete removes it"
        } else {
            "Tab moves focus · Enter activates"
        },
        Style::default().fg(theme::palette().muted),
    ));
    let summary_height = u16::try_from(lines.len()).unwrap_or(u16::MAX);
    let list_height = if can_attach {
        u16::try_from(mounts.mounts.len()).unwrap_or(u16::MAX)
    } else {
        0
    };
    let queue_height = if queue.is_some() {
        1_u16.saturating_add(
            u16::try_from(queued_entries.len().saturating_add(prepared_entries.len()))
                .unwrap_or(u16::MAX),
        )
    } else {
        0
    };
    let total_height = summary_height
        .saturating_add(list_height)
        .saturating_add(queue_height);
    let popup = centered_modal(
        frame,
        surfaces,
        84,
        (total_height.min(16) + 3).clamp(13, 26),
        area,
    );
    let inner = popup.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 1,
    });
    let title_line = dismissible_modal_title(form, popup, title.trim(), theme::title(true), true);
    frame.render_widget(theme::modal().title(title_line), popup);
    let body = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner.height.saturating_sub(1),
    );
    let focused_row = match form.focused() {
        Some(WizardControl::CreateManagedWorktree) => worktree_row,
        Some(WizardControl::MjolnirSubagents) => subagent_row,
        Some(WizardControl::ReviewAttachments) => Some(
            summary_height.saturating_add(
                mounts
                    .history_index
                    .min(mounts.mounts.len().saturating_sub(1)) as u16,
            ),
        ),
        Some(WizardControl::DiscardQueue) => Some(summary_height.saturating_add(list_height)),
        _ => None,
    };
    let viewport = FormViewport::new(body, total_height, 0, focused_row);
    for (index, line) in lines.iter().enumerate() {
        frame.render_widget(
            Paragraph::new(line.clone()),
            viewport.row(u16::try_from(index).unwrap_or(u16::MAX), 1),
        );
    }
    if let Some(((checked, available), row)) = worktree.zip(worktree_row) {
        Checkbox::render(
            frame,
            viewport.row(row, 1),
            "Create managed worktree",
            checked && available,
            available,
            form,
            WizardControl::CreateManagedWorktree,
        );
    }
    if let Some((checked, row)) = subagents.zip(subagent_row) {
        Checkbox::render(
            frame,
            viewport.row(row, 1),
            "Use Mjolnir sub-agents",
            checked,
            true,
            form,
            WizardControl::MjolnirSubagents,
        );
    }
    if can_attach && !mounts.mounts.is_empty() {
        let list_area = viewport.row(summary_height, list_height);
        let rows = mounts
            .mounts
            .iter()
            .map(|mount| {
                Line::raw(format!(
                    "{} → {}{}",
                    mount.source.display(),
                    mount.destination.display(),
                    access_marker(mount.access)
                ))
            })
            .collect::<Vec<_>>();
        ChoiceList::render(
            frame,
            list_area,
            &rows,
            mounts.history_index,
            form,
            WizardControl::ReviewAttachments,
        );
    }
    if let Some((count, discard)) = queue {
        let queue_area = viewport.row(summary_height.saturating_add(list_height), 1);
        Checkbox::render(
            frame,
            queue_area,
            &format!(
                "{} {count} queued command{} {}",
                if discard { "Discard" } else { "Start" },
                if count == 1 { "" } else { "s" },
                if moving { "after move" } else { "on resume" },
            ),
            discard,
            true,
            form,
            WizardControl::DiscardQueue,
        );
        for (index, entry) in queued_entries.iter().enumerate() {
            let text = if entry.text.trim().is_empty() {
                "[empty command]".to_owned()
            } else {
                entry.text.replace('\n', " ")
            };
            let attachment_count = entry.attachments.len();
            let attachment_note = match attachment_count {
                0 => String::new(),
                1 => " · 1 attachment".to_owned(),
                count => format!(" · {count} attachments"),
            };
            let text = crate::widgets::truncate_text(&text, inner.width.saturating_sub(4) as usize);
            frame.render_widget(
                Paragraph::new(Line::styled(
                    format!("  {}. {text}{attachment_note}", index + 1),
                    Style::default().fg(theme::palette().muted),
                )),
                viewport.row(
                    summary_height
                        .saturating_add(list_height)
                        .saturating_add(1)
                        .saturating_add(u16::try_from(index).unwrap_or(u16::MAX)),
                    1,
                ),
            );
        }
        for (index, entry) in prepared_entries.iter().enumerate() {
            let (kind, text) = match &entry.kind {
                mj_core::state::QueuedCommandKind::Prompt => (
                    "prompt",
                    mj_core::transcript::materialized_content_text(&entry.content),
                ),
                mj_core::state::QueuedCommandKind::SetConfig { key, value } => {
                    ("config", mj_core::state::config_command_text(key, value))
                }
            };
            let text = if text.trim().is_empty() {
                format!("[{kind}]")
            } else {
                format!("{kind}: {}", text.replace('\n', " "))
            };
            let text = crate::widgets::truncate_text(&text, inner.width.saturating_sub(4) as usize);
            let row = queued_entries.len().saturating_add(index);
            frame.render_widget(
                Paragraph::new(Line::styled(
                    format!("  {}. {text}", row + 1),
                    Style::default().fg(theme::palette().muted),
                )),
                viewport.row(
                    summary_height
                        .saturating_add(list_height)
                        .saturating_add(1)
                        .saturating_add(u16::try_from(row).unwrap_or(u16::MAX)),
                    1,
                ),
            );
        }
    }
    let mut buttons = vec![
        (WizardControl::Cancel, "Cancel", true),
        (WizardControl::Back, "Back", true),
    ];
    if can_attach {
        buttons.push((WizardControl::Add, "Add directory…", true));
    }
    buttons.push((
        WizardControl::Submit,
        submit_label,
        submit_enabled
            && !remote_preflight_in_flight
            && (remote_repositories.is_some()
                || !local_changes_excluded
                || remote_preflight_error.is_some())
            && (allocation.is_some() || !matches!(target, TargetTemplate::AwsEc2 { .. })),
    ));
    Dialog::render_actions(
        frame,
        mj_chat::components::DialogShell::layout(inner, 0).actions,
        &buttons,
        form,
    );
}

/// Suffix that shows an attached directory's access mode in a list row.
pub(crate) fn access_marker(access: MountAccess) -> String {
    format!(" · {}", access.label())
}

/// The access modes offered for an attachment, as the shared rule in
/// [`MountAccess::offered`] defines them.
pub(crate) fn access_choices(overlay_unavailable: bool) -> Vec<MountAccess> {
    MountAccess::offered(!overlay_unavailable)
}

fn access_description(access: MountAccess) -> &'static str {
    match access {
        MountAccess::Ro => "ro · read-only",
        MountAccess::Cow => "cow · container-private copy-on-write",
        MountAccess::Rw => "rw · writes reach the host directory",
    }
}

/// The form control kind for an access-mode combobox.
pub(crate) fn access_combo_kind<K: Copy + Eq>(
    combo: &ComboBoxState<K>,
    choices: &[MountAccess],
    access: MountAccess,
    id: K,
) -> ControlKind {
    ControlKind::ComboBox {
        len: choices.len(),
        selected: combo.selection(id, access_index(choices, access)),
        expanded: combo.is_open(id),
    }
}

pub(crate) fn access_index(choices: &[MountAccess], access: MountAccess) -> usize {
    choices
        .iter()
        .position(|choice| *choice == access)
        .unwrap_or(0)
}

/// Draw the access-mode combobox for an attachment under edit. Screens call
/// this once in place and, while it is open, again after everything else so
/// the popup stays on top.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_access_combo<K: Copy + Eq>(
    frame: &mut Frame<'_>,
    bounds: Rect,
    field: Rect,
    access: MountAccess,
    choices: &[MountAccess],
    combo: &ComboBoxState<K>,
    expanded: bool,
    form: &mut Form<K>,
    id: K,
) {
    let selected = combo.selection(id, access_index(choices, access));
    let value = choices
        .get(selected)
        .map(|choice| ComboBox::display_value(access_description(*choice)))
        .unwrap_or_default();
    let options = choices
        .iter()
        .map(|choice| Line::raw(access_description(*choice)))
        .collect::<Vec<_>>();
    ComboBox::render(
        frame,
        bounds,
        field,
        &value,
        &options,
        selected,
        expanded,
        true,
        " access · ↑/↓ select · Enter accept ",
        PopupSide::Below,
        form,
        id,
    );
}

// Domain mount data, navigation, and the shared form are separate inputs.
#[allow(clippy::too_many_arguments)]
fn render_mount_wizard(
    frame: &mut Frame,
    area: Rect,
    dashboard: &DashboardState,
    target_index: usize,
    mounts: &MountWizard,
    form: &mut Dialog<WizardControl>,
    title: &str,
    surfaces: &mut FrameSurfaces,
) {
    let target_id = nth_key(&dashboard.config.targets, target_index);
    let target = dashboard
        .config
        .targets
        .get(&target_id)
        .expect("selected target index is present in config");
    let protection = match target {
        TargetTemplate::AppleContainer { .. } => {
            "Apple Container has no :O overlay mode; each extra bind is read-only."
        }
        TargetTemplate::LocalPodman { .. } | TargetTemplate::SshPodman { .. } => {
            "Podman uses :O copy-on-write overlays; read-only skips the overlay."
        }
        TargetTemplate::LocalDocker { .. } | TargetTemplate::SshDocker { .. } => {
            "Docker uses session-owned OverlayFS volumes on the Docker host; read-only skips the overlay."
        }
        TargetTemplate::AwsEc2 { .. } => {
            "EC2 directories stream as tar.gz through one SSH connection into the destination."
        }
        TargetTemplate::LocalBare | TargetTemplate::SshBare { .. } => {
            unreachable!("bare targets do not attach resources")
        }
    };
    let mut lines = vec![
        Line::raw(format!("Target: {target_id} ({})", target_label(target))),
        Line::styled(protection, Style::default().fg(theme::palette().warning)),
    ];
    if !mounts.mounts.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::raw("Already attached:"));
        lines.extend(mounts.mounts.iter().map(|mount| {
            Line::raw(format!(
                "  {} → {}{}",
                mount.source.display(),
                mount.destination.display(),
                access_marker(mount.access)
            ))
        }));
    }
    if form.is_focused(WizardControl::MountSource)
        && mounts.source.is_empty()
        && !mounts.history.is_empty()
    {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "Recent sources (↑/↓ when Source is empty):",
            Style::default().fg(theme::palette().muted),
        ));
        lines.extend(
            mounts
                .history
                .iter()
                .take(5)
                .enumerate()
                .map(|(index, source)| {
                    let marker = if index == mounts.history_index {
                        "› "
                    } else {
                        "  "
                    };
                    Line::raw(format!("{marker}{}", source.display()))
                }),
        );
    }
    if !mounts.completion_candidates.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "Matches (↑/↓ select · Enter choose):",
            Style::default().fg(theme::palette().muted),
        ));
        lines.extend(mounts.completion_candidates.iter().take(5).enumerate().map(
            |(index, candidate)| {
                Line::raw(format!(
                    "{}{}",
                    if index == mounts.completion_index {
                        "› "
                    } else {
                        "  "
                    },
                    candidate
                ))
            },
        ));
    }
    if let Some(error) = &mounts.error {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            error,
            Style::default().fg(theme::palette().error),
        ));
    }
    lines.push(Line::styled(
        "Ctrl-Space completes · Tab moves focus · Space toggles read-only · Enter continues/adds",
        Style::default().fg(theme::palette().muted),
    ));
    let info_height = u16::try_from(lines.len()).unwrap_or(u16::MAX);
    let total_height = info_height.saturating_add(3);
    let popup = centered_modal(
        frame,
        surfaces,
        84,
        (total_height.min(16) + 3).clamp(13, 25),
        area,
    );
    let inner = popup.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 1,
    });
    let title_line = dismissible_modal_title(form, popup, title.trim(), theme::title(true), true);
    frame.render_widget(theme::modal().title(title_line), popup);
    let body = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner.height.saturating_sub(1),
    );
    let focused_row = match form.focused() {
        Some(WizardControl::MountSource) => Some(info_height),
        Some(WizardControl::MountDestination) => Some(info_height.saturating_add(1)),
        Some(WizardControl::MountAccess) => Some(info_height.saturating_add(2)),
        _ => None,
    };
    let viewport = FormViewport::new(body, total_height, 0, focused_row);
    for (index, line) in lines.iter().enumerate() {
        let row = viewport.row(u16::try_from(index).unwrap_or(u16::MAX), 1);
        frame.render_widget(Paragraph::new(line.clone()), row);
    }
    let field_width = inner.width.saturating_sub(12);
    let source_row = viewport.row(info_height, 1);
    frame.render_widget(
        Paragraph::new("Source:"),
        Rect::new(
            source_row.x,
            source_row.y,
            10.min(source_row.width),
            source_row.height,
        ),
    );
    PathField::render(
        frame,
        Rect::new(
            source_row.x.saturating_add(10),
            source_row.y,
            field_width,
            source_row.height,
        ),
        &mounts.source,
        form,
        WizardControl::MountSource,
    );
    let destination_row = viewport.row(info_height.saturating_add(1), 1);
    frame.render_widget(
        Paragraph::new("Destination:"),
        Rect::new(
            destination_row.x,
            destination_row.y,
            10.min(destination_row.width),
            destination_row.height,
        ),
    );
    PathField::render(
        frame,
        Rect::new(
            destination_row.x.saturating_add(10),
            destination_row.y,
            field_width,
            destination_row.height,
        ),
        &mounts.destination,
        form,
        WizardControl::MountDestination,
    );
    let access_row = viewport.row(info_height.saturating_add(2), 1);
    frame.render_widget(
        Paragraph::new("Access:"),
        Rect::new(
            access_row.x,
            access_row.y,
            10.min(access_row.width),
            access_row.height,
        ),
    );
    let access_field = Rect::new(
        access_row.x.saturating_add(10),
        access_row.y,
        field_width,
        access_row.height,
    );
    let access_choices = mounts.access_choices();
    render_access_combo(
        frame,
        inner,
        access_field,
        mounts.access,
        &access_choices,
        &mounts.access_combo,
        false,
        form,
        WizardControl::MountAccess,
    );
    Dialog::render_actions(
        frame,
        mj_chat::components::DialogShell::layout(inner, 0).actions,
        &[
            (WizardControl::Cancel, "Cancel", true),
            (WizardControl::Back, "Back", true),
            (WizardControl::Add, "Add directory", true),
        ],
        form,
    );
    if mounts.access_combo.is_open(WizardControl::MountAccess) {
        render_access_combo(
            frame,
            inner,
            access_field,
            mounts.access,
            &access_choices,
            &mounts.access_combo,
            true,
            form,
            WizardControl::MountAccess,
        );
    }
}

pub(crate) fn render_resume_wizard(
    frame: &mut Frame,
    area: Rect,
    dashboard: &DashboardState,
    wizard: &ResumeWizard,
    surfaces: &mut FrameSurfaces,
) {
    let mut form = wizard.form.borrow_mut();
    let initial = step_initial(wizard.step);
    begin_form_frame(&mut form, initial);
    if wizard.step == WizardStep::Review {
        let profile_id = dashboard
            .compatible_profiles(&wizard.session_id)
            .get(wizard.profile)
            .map(|(id, _)| id.as_str())
            .unwrap_or("unknown");
        let session = dashboard.state.sessions.get(&wizard.session_id);
        let bundle_id = session
            .map(|session| session.bundle_id.as_str())
            .unwrap_or("unknown");
        let target_id = nth_key(&dashboard.config.targets, wizard.target);
        let reused_project_directory = session
            .filter(|session| {
                mj_client::target::resume_compatibility(session, &dashboard.config, &target_id)
                    == Ok(mj_client::target::ResumePlan::InPlace)
            })
            .and_then(|session| session.project_directory.as_deref())
            .map(|directory| directory.display().to_string());
        let (project_label, project, project_note) =
            if let Some(directory) = reused_project_directory.as_deref() {
                ("Project directory", directory, " (reused)")
            } else {
                ("Project", bundle_id, "")
            };
        render_review_wizard(
            frame,
            area,
            dashboard,
            ReviewWizardView {
                worktree: None,
                subagents: None,
                profile_id,
                project_label,
                project,
                project_note,
                target_id: &target_id,
                allocation: wizard.resource_allocation.as_ref(),
                mounts: &wizard.mounts,
                title: if wizard.moving {
                    " Move · 3/3 confirm "
                } else {
                    " Resume · 3/3 review "
                },
                submit_label: if wizard.moving && wizard.preparation_error.is_some() {
                    "Retry"
                } else if wizard.moving {
                    "Move"
                } else {
                    "Resume"
                },
                moving: wizard.moving,
                preparing: wizard.preparing,
                preparation_error: wizard.preparation_error.as_deref(),
                submit_enabled: !wizard.moving
                    || wizard.preparation.is_some()
                    || wizard.preparation_error.is_some(),
                source_unavailable: wizard
                    .preparation
                    .as_ref()
                    .is_some_and(|p| p.source_unavailable),
                active_interruption: wizard
                    .preparation
                    .as_ref()
                    .map_or(wizard.moving, |preparation| preparation.active),
                clear_resource_allocation: wizard.preparation.as_ref().map_or_else(
                    || {
                        wizard.moving
                            && dashboard
                                .state
                                .sessions
                                .get(&wizard.session_id)
                                .is_some_and(|session| session.resource_allocation.is_some())
                            && matches!(
                                dashboard.config.targets.get(&target_id),
                                Some(
                                    mj_core::config::TargetTemplate::LocalBare
                                        | mj_core::config::TargetTemplate::SshBare { .. }
                                )
                            )
                    },
                    |preparation| preparation.selection.clear_resource_allocation,
                ),
                queue: wizard.preparation.as_ref().map_or_else(
                    || {
                        dashboard
                            .session_details
                            .get(&wizard.session_id)
                            .map(|detail| detail.queued_prompts.len())
                            .filter(|count| *count > 0)
                            .map(|count| (count, wizard.discard_queue))
                    },
                    |preparation| {
                        (!preparation.queued_commands.is_empty())
                            .then_some((preparation.queued_commands.len(), wizard.discard_queue))
                    },
                ),
                queued_entries: if wizard.preparation.is_some() {
                    &[][..]
                } else {
                    dashboard
                        .session_details
                        .get(&wizard.session_id)
                        .map_or(&[][..], |detail| detail.queued_prompts.as_slice())
                },
                prepared_entries: wizard.preparation.as_ref().map_or(&[][..], |preparation| {
                    preparation.queued_commands.as_slice()
                }),
                remote_repositories: None,
                remote_preflight_in_flight: false,
                remote_preflight_error: None,
                local_changes_excluded: false,
                conversion: wizard
                    .preparation
                    .as_ref()
                    .and_then(|preparation| preparation.conversion.as_deref()),
            },
            &mut form,
            surfaces,
        );
        form.end_frame(initial);
        return;
    }
    if wizard.step == WizardStep::Mounts {
        render_mount_wizard(
            frame,
            area,
            dashboard,
            wizard.target,
            &wizard.mounts,
            &mut form,
            " Add attached directory ",
            surfaces,
        );
        form.end_frame(initial);
        return;
    }
    let (title, choices, selected, help) = match wizard.step {
        WizardStep::Profile => {
            let profiles = dashboard.compatible_profiles(&wizard.session_id);
            let session_harness = dashboard
                .state
                .sessions
                .get(&wizard.session_id)
                .map(|session| session.harness_kind);
            let rows = profiles
                .iter()
                .map(|(id, harness)| {
                    let choice = dashboard.profile_choice(id, *harness);
                    if session_harness.is_some_and(|current| current != *harness) {
                        choice.with_note("(lossy: text-only transcript)")
                    } else {
                        choice
                    }
                })
                .collect();
            let mut help = vec![
                picker_help("↑/↓ select · Tab moves focus · Enter activates"),
                picker_help("Lossy: text only; tool calls + reasoning dropped."),
            ];
            if profiles
                .iter()
                .any(|(_, harness)| needs_guardian_warning(*harness))
            {
                help.push(guardian_footnote());
            }
            (
                if wizard.moving {
                    " Move · 1/3 profile (cross-harness supported) "
                } else {
                    " Resume · 1/3 profile (cross-harness supported) "
                },
                profile_table(rows),
                wizard.profile,
                help,
            )
        }
        WizardStep::Target => (
            if wizard.moving {
                " Move · 2/3 new target "
            } else {
                " Resume · 2/3 new target "
            },
            dashboard
                .config
                .targets
                .iter()
                .map(|(id, target)| {
                    let size = if id == &nth_key(&dashboard.config.targets, wizard.target) {
                        resource_allocation_label(
                            wizard.resource_allocation.as_ref(),
                            wizard.sizing_error.as_deref(),
                        )
                    } else {
                        String::new()
                    };
                    match dashboard.resume_target_rejection(&wizard.session_id, id) {
                        Some(reason) => PickerChoice::disabled(format!(
                            "{id}  {}  · {reason}",
                            target_label(target)
                        )),
                        None => PickerChoice::text(format!("{id}  {}{size}", target_label(target))),
                    }
                })
                .collect(),
            wizard.target,
            vec![picker_help(
                "+ double · - halve · c +8 CPU · m +50% memory · r reset · F5 recheck",
            )],
        ),
        WizardStep::Bundle => unreachable!("resume does not select a bundle"),
        WizardStep::Review => unreachable!("review was rendered above"),
        WizardStep::Mounts => unreachable!("mount input was rendered above"),
        WizardStep::NewBundle => unreachable!("resume does not create bundles"),
        WizardStep::ProjectDirectory => unreachable!("resume does not select a project directory"),
    };
    render_picker(
        frame,
        area,
        title,
        choices,
        help,
        PickerNavigation {
            has_back: wizard.step != WizardStep::Profile,
            selected,
            control: match wizard.step {
                WizardStep::Profile => WizardControl::ProfileList,
                WizardStep::Target => WizardControl::TargetList,
                _ => unreachable!("resume picker step has a list control"),
            },
            next_enabled: wizard.step != WizardStep::Target || wizard.can_advance_target(dashboard),
            pinned_action: None,
            empty_hint: None,
        },
        &mut form,
        surfaces,
    );
    form.end_frame(match wizard.step {
        WizardStep::Profile => WizardControl::ProfileList,
        WizardStep::Target => WizardControl::TargetList,
        _ => unreachable!("resume picker step has a list control"),
    });
}

fn nth_bundle_key(config: &Config, state: &State, index: usize) -> String {
    bundle_ids_by_recent_creation(config, state)
        .get(index)
        .expect("wizard is only opened for non-empty configuration")
        .to_string()
}

fn most_recent_configured_session<'a>(
    config: &Config,
    state: &'a State,
) -> Option<&'a SessionRecord> {
    state
        .sessions
        .values()
        .filter(|session| {
            config.enabled_profile(&session.last_profile).is_some()
                && config.bundles.contains_key(&session.bundle_id)
                && config.targets.contains_key(&session.target_template_id)
        })
        .max_by_key(|session| {
            chrono::DateTime::parse_from_rfc3339(&session.created_at)
                .ok()
                .map(|timestamp| timestamp.timestamp_millis())
        })
}

fn bundle_ids_by_recent_creation<'a>(config: &'a Config, state: &State) -> Vec<&'a str> {
    let mut latest_created_at = BTreeMap::<&str, i64>::new();
    for session in state.sessions.values() {
        if !config.bundles.contains_key(&session.bundle_id) {
            continue;
        }
        let Some(created_at) = chrono::DateTime::parse_from_rfc3339(&session.created_at)
            .ok()
            .map(|timestamp| timestamp.timestamp_millis())
        else {
            continue;
        };
        latest_created_at
            .entry(&session.bundle_id)
            .and_modify(|latest| *latest = (*latest).max(created_at))
            .or_insert(created_at);
    }

    let mut bundle_ids = config
        .bundles
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    bundle_ids.sort_by(|left, right| {
        latest_created_at
            .get(right)
            .cmp(&latest_created_at.get(left))
            .then_with(|| left.cmp(right))
    });
    bundle_ids
}

fn target_label(target: &TargetTemplate) -> &'static str {
    match target {
        TargetTemplate::LocalBare => "raw localhost",
        TargetTemplate::LocalPodman { .. } => "local Podman",
        TargetTemplate::LocalDocker { .. } => "local Docker",
        TargetTemplate::AppleContainer { .. } => "Apple container",
        TargetTemplate::AwsEc2 { .. } => "AWS EC2",
        TargetTemplate::SshBare { .. } => "named SSH machine",
        TargetTemplate::SshPodman { .. } => "Podman over SSH",
        TargetTemplate::SshDocker { .. } => "Docker over SSH",
    }
}

fn resource_allocation_label(
    allocation: Option<&SessionResourceAllocation>,
    error: Option<&str>,
) -> String {
    let allocation = match allocation {
        Some(SessionResourceAllocation::Container { cpus, memory_bytes }) => {
            format!(" · {cpus} CPU / {}", format_resource_bytes(*memory_bytes))
        }
        Some(SessionResourceAllocation::AwsEc2 {
            instance_type,
            vcpus,
            memory_bytes,
        }) => format!(
            " · {instance_type} · {vcpus} CPU / {}",
            format_resource_bytes(*memory_bytes)
        ),
        None => " · fixed/default resources".into(),
    };
    match error {
        Some(error) => format!("{allocation} · {error}"),
        None => allocation,
    }
}

fn default_resource_destination(
    target: &TargetTemplate,
    source: &std::path::Path,
    existing: &[AdditionalMount],
) -> std::path::PathBuf {
    let default = default_mount_destination(source, existing);
    let TargetTemplate::AwsEc2 { ssh_user, .. } = target else {
        return default;
    };
    let basename = default
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("resource"));
    let home = if ssh_user == "root" {
        std::path::PathBuf::from("/root")
    } else {
        std::path::PathBuf::from("/home").join(ssh_user)
    };
    let base = home.join("mj-resources").join(basename);
    if !existing.iter().any(|resource| resource.destination == base) {
        return base;
    }
    for number in 2.. {
        let candidate = home
            .join("mj-resources")
            .join(format!("{}-{number}", basename.to_string_lossy()));
        if !existing
            .iter()
            .any(|resource| resource.destination == candidate)
        {
            return candidate;
        }
    }
    unreachable!()
}

fn apply_mount_completions(wizard: &mut MountWizard, prefix: &str, candidates: Vec<String>) {
    wizard
        .completion_cache
        .insert(prefix.to_owned(), candidates.clone());
    if let Some(completed) = path_completion(prefix, &candidates) {
        wizard.source = completed.into();
    }
    if candidates.len() > 1 {
        wizard.completion_candidates = candidates.into_iter().take(5).collect();
        wizard.completion_index = 0;
    } else {
        wizard.completion_candidates.clear();
    }
}

mod dashboard;

#[cfg(test)]
mod tests;
