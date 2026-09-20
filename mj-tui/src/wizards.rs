//! New-session and resume wizards, including their mount and review steps.
mod picker;
mod render;
pub(crate) use picker::*;
pub(crate) use render::*;

use mj_chat::path_input::PathInput;

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent};
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
    Checkbox, ChoiceList, ComboBox, ComboBoxState, ControlKind, Dialog, EditOutcome, FieldEdit,
    Form, FormViewport, Interaction, PopupSide,
};
use mj_chat::selection::FrameSurfaces;
use mj_core::targets::{AdditionalMount, MountAccess, default_mount_destination};

use crate::widgets::{
    Truncate, centered_modal, dismissible_modal_title, format_resource_bytes, truncate_to_cells,
};
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
    RecentProject(usize),
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
        self.source.dismiss_completion();
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
        self.selected_profile_kind(config)
            .is_some_and(HarnessKind::supports_delegation_tools)
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
}

/// What the resume wizard will start.
///
/// The controls are the same either way — a profile, a target, attachments —
/// but a record is resumed from its checkpoint while an archived SessionWiki
/// session has no record and no checkpoint and is restored into a brand-new
/// session carrying a summary of its transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResumeSource {
    Session,
    Archive,
}

#[derive(Debug, Clone)]
pub(crate) struct ResumeWizard {
    /// The Mjolnir session being resumed or moved, or, for an archive, the
    /// SessionWiki id being restored. `source` says which.
    pub(crate) session_id: String,
    pub(crate) source: ResumeSource,
    /// Shown in the wizard's title for an archive, which has no record to
    /// read a title from.
    pub(crate) title: String,
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
            && self.source == other.source
            && self.title == other.title
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

/// Empties the attachment editor for a new entry. The caller moves the
/// wizard to [`WizardStep::Mounts`].
fn prepare_mount_editor(mounts: &mut MountWizard) {
    mounts.source.clear();
    mounts.destination.clear();
    mounts.access = MountAccess::Ro;
    mounts.error = None;
    mounts.editing_mount = None;
    mounts.source.dismiss_completion();
}

/// Loads the selected attachment into the editor. Answers false when there is
/// nothing to edit, in which case the wizard must stay on its current step.
fn prepare_selected_mount_editor(mounts: &mut MountWizard) -> bool {
    if mounts.mounts.is_empty() {
        return false;
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
    mounts.source.dismiss_completion();
    true
}

fn begin_mount_editor<W: WizardDraft>(wizard: &mut W) {
    prepare_mount_editor(wizard.mounts_mut());
    wizard.set_step(WizardStep::Mounts);
    open_mount_editor(wizard);
}

fn edit_selected_mount<W: WizardDraft>(wizard: &mut W) {
    if prepare_selected_mount_editor(wizard.mounts_mut()) {
        wizard.set_step(WizardStep::Mounts);
    }
    open_mount_editor(wizard);
}

fn open_mount_editor<W: WizardDraft>(wizard: &mut W) {
    let form = wizard.form_mut();
    form.forget_draft_part("attachment editor");
    form.focus(WizardControl::MountSource);
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

mod dashboard;
mod draft;

pub(crate) use dashboard::paths::{CompletesPaths, route_path_completion};
pub(crate) use draft::{DraftChange, WizardDraft, target_advance_enabled};

#[cfg(test)]
mod tests;
