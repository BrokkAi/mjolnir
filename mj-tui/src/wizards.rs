//! New-session and resume wizards, including their mount and review steps.

use std::cell::RefCell;
use std::collections::BTreeMap;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use mj_chat::theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use hel::hel_config::{
    HelConfig, TargetTemplate, container_size_host, is_bare_project_target, mount_history_host,
    project_history_host, raw_project_context_id,
};
use hel::hel_state::{
    HelState, MaterializedQueuedPrompt, MoveOperation, MovePreparation, ResumeQueueDisposition,
    SessionRecord, SessionResourceAllocation, SessionState, allocation_cpus, allocation_memory,
};
use hel::hel_targets::{AdditionalMount, default_mount_destination, path_completion};
use mj_chat::components::{
    ButtonRow, Checkbox, ChoiceList, ConsumedEvent, ControlKind, FieldEdit, Form, FormViewport,
    Interaction, Outcome, TextField,
};
use mj_chat::hel_selection::FrameSurfaces;
use mj_chat::hel_text_input::TextInput;

use crate::widgets::{centered_modal, format_resource_bytes};
use crate::{
    DashboardAction, DashboardState, Mode, RemoteRepositoryPreview, cycle_control, move_index,
    nth_key,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WizardFocus {
    Content,
    Cancel,
    Back,
    Next,
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
    MountReadOnly,
    ReviewAttachments,
    DiscardQueue,
    Cancel,
    Back,
    Next,
    Add,
    Submit,
}

#[derive(Debug, Clone)]
pub(crate) struct NewWizard {
    /// Creation stays in this workspace even if the visible tab changes.
    pub(crate) workspace_id: String,
    pub(crate) step: WizardStep,
    pub(crate) focus: WizardFocus,
    pub(crate) profile: usize,
    bundle: usize,
    pub(crate) target: usize,
    pub(crate) mounts: MountWizard,
    review_focus: ReviewFocus,
    pub(crate) new_bundle_focus: NewBundleFocus,
    pub(crate) new_bundle_selected: usize,
    pub(crate) new_bundle_repositories: Vec<String>,
    pub(crate) new_bundle_source: TextInput,
    pub(crate) bundle_creation_in_flight: bool,
    pub(crate) project_directory: TextInput,
    pub(crate) project_directory_error: Option<String>,
    project_history: Vec<std::path::PathBuf>,
    project_history_index: usize,
    pub(crate) resource_allocation: Option<SessionResourceAllocation>,
    aws_options: BTreeMap<String, Vec<SessionResourceAllocation>>,
    pub(crate) sizing_error: Option<String>,
    /// Network clone destinations returned by the asynchronous creation
    /// preflight. A nonempty value is concrete evidence the review is ready.
    pub(crate) remote_repositories: Option<Vec<RemoteRepositoryPreview>>,
    pub(crate) remote_preflight_in_flight: bool,
    pub(crate) remote_preflight_error: Option<String>,
    pub(crate) form: RefCell<Form<WizardControl>>,
}

impl PartialEq for NewWizard {
    fn eq(&self, other: &Self) -> bool {
        self.workspace_id == other.workspace_id
            && self.step == other.step
            && self.focus == other.focus
            && self.profile == other.profile
            && self.bundle == other.bundle
            && self.target == other.target
            && self.mounts == other.mounts
            && self.review_focus == other.review_focus
            && self.new_bundle_focus == other.new_bundle_focus
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NewBundleFocus {
    Repositories,
    Source,
    Add,
    Remove,
    Cancel,
    Back,
    Create,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MountFocus {
    Source,
    Destination,
    ReadOnly,
    Cancel,
    Back,
    Add,
}

/// Tab order for the mount editor, shared by the new-session and resume paths.
const MOUNT_FOCUS_ORDER: [MountFocus; 6] = [
    MountFocus::Source,
    MountFocus::Destination,
    MountFocus::ReadOnly,
    MountFocus::Cancel,
    MountFocus::Back,
    MountFocus::Add,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReviewFocus {
    Attachments,
    Cancel,
    Back,
    Add,
    Submit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MountWizard {
    pub(crate) source: TextInput,
    pub(crate) destination: TextInput,
    pub(crate) focus: MountFocus,
    pub(crate) read_only: bool,
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
            source: TextInput::new(),
            destination: TextInput::new(),
            focus: MountFocus::Source,
            read_only: false,
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

    /// Why the source under edit can only be attached read-only, if it can.
    pub(crate) fn forced_read_only(&self) -> Option<&str> {
        self.forced_sources
            .get(self.source.trim())
            .map(String::as_str)
    }

    /// Space and Enter toggle the checkbox, except where the host's filesystem
    /// has already settled the answer.
    fn toggle_read_only(&mut self) {
        if self.forced_read_only().is_some() {
            return;
        }
        self.read_only = !self.read_only;
    }

    fn add_validated_mount(&mut self) {
        let mount = AdditionalMount {
            source: self.source.to_string().into(),
            destination: self.destination.to_string().into(),
            read_only: self.read_only,
        };
        if let Some(index) = self.editing_mount.take() {
            self.mounts[index] = mount;
        } else {
            self.mounts.push(mount);
        }
        self.source.clear();
        self.destination.clear();
        self.read_only = false;
        self.focus = MountFocus::Source;
        self.completion_candidates.clear();
        self.error = None;
    }
}

impl NewWizard {
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
        self.step == WizardStep::ProjectDirectory
            || self.step == WizardStep::NewBundle
                && self.new_bundle_focus == NewBundleFocus::Source
                && !self.bundle_creation_in_flight
            || self.step == WizardStep::Mounts
                && matches!(
                    self.mounts.focus,
                    MountFocus::Source | MountFocus::Destination
                )
    }
}

impl ResumeWizard {
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
            && matches!(
                self.mounts.focus,
                MountFocus::Source | MountFocus::Destination
            )
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
    pub(crate) focus: WizardFocus,
    pub(crate) profile: usize,
    pub(crate) target: usize,
    pub(crate) mounts: MountWizard,
    review_focus: ReviewFocus,
    pub(crate) resource_allocation: Option<SessionResourceAllocation>,
    aws_options: BTreeMap<String, Vec<SessionResourceAllocation>>,
    pub(crate) sizing_error: Option<String>,
    pub(crate) discard_queue: bool,
    pub(crate) form: RefCell<Form<WizardControl>>,
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
            && self.focus == other.focus
            && self.profile == other.profile
            && self.target == other.target
            && self.mounts == other.mounts
            && self.review_focus == other.review_focus
            && self.resource_allocation == other.resource_allocation
            && self.aws_options == other.aws_options
            && self.sizing_error == other.sizing_error
            && self.discard_queue == other.discard_queue
    }
}

impl Eq for ResumeWizard {}

fn cycle_wizard_focus(current: WizardFocus, has_back: bool, reverse: bool) -> WizardFocus {
    if has_back {
        cycle_control(
            current,
            &[
                WizardFocus::Content,
                WizardFocus::Cancel,
                WizardFocus::Back,
                WizardFocus::Next,
            ],
            reverse,
        )
    } else {
        cycle_control(
            current,
            &[WizardFocus::Content, WizardFocus::Cancel, WizardFocus::Next],
            reverse,
        )
    }
}

fn review_focus_order(can_attach: bool, has_attachments: bool) -> Vec<ReviewFocus> {
    let mut order = Vec::new();
    if has_attachments {
        order.push(ReviewFocus::Attachments);
    }
    order.extend([ReviewFocus::Cancel, ReviewFocus::Back]);
    if can_attach {
        order.push(ReviewFocus::Add);
    }
    order.push(ReviewFocus::Submit);
    order
}

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
    mounts.read_only = false;
    mounts.focus = MountFocus::Source;
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
    mounts.read_only = mount.read_only || mounts.forced_read_only().is_some();
    mounts.focus = MountFocus::Source;
    mounts.error = None;
    mounts.editing_mount = Some(index);
    mounts.completion_candidates.clear();
    *step = WizardStep::Mounts;
}

fn begin_mount_editor(wizard: &mut NewWizard) {
    prepare_mount_editor(&mut wizard.step, &mut wizard.mounts);
}

fn edit_selected_mount(wizard: &mut NewWizard) {
    prepare_selected_mount_editor(&mut wizard.step, &mut wizard.mounts);
}

fn begin_resume_mount_editor(wizard: &mut ResumeWizard) {
    prepare_mount_editor(&mut wizard.step, &mut wizard.mounts);
}

fn edit_selected_resume_mount(wizard: &mut ResumeWizard) {
    prepare_selected_mount_editor(&mut wizard.step, &mut wizard.mounts);
}

fn validate_mount_entry(mounts: &MountWizard) -> Option<String> {
    let mount = AdditionalMount {
        source: mounts.source.to_string().into(),
        destination: mounts.destination.to_string().into(),
        read_only: mounts.read_only,
    };
    if let Err(error) = hel::hel_targets::validate_additional_mounts(std::slice::from_ref(&mount)) {
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
    fn active_index_mut(&mut self) -> &mut usize {
        match self.step {
            WizardStep::Profile => &mut self.profile,
            WizardStep::Bundle => &mut self.bundle,
            WizardStep::Target => &mut self.target,
            WizardStep::ProjectDirectory => unreachable!("project directory has no picker index"),
            WizardStep::Review => unreachable!("review input has no picker index"),
            WizardStep::Mounts => unreachable!("mount input has no picker index"),
            WizardStep::NewBundle => unreachable!("bundle input has no picker index"),
        }
    }

    fn add_new_bundle_repository(&mut self) -> bool {
        let source = self.new_bundle_source.trim();
        if source.is_empty() {
            return false;
        }
        self.new_bundle_repositories.push(source.to_owned());
        self.new_bundle_selected = self.new_bundle_repositories.len() - 1;
        self.new_bundle_source.clear();
        self.new_bundle_focus = NewBundleFocus::Source;
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
        self.new_bundle_focus = if self.new_bundle_repositories.is_empty() {
            NewBundleFocus::Source
        } else {
            NewBundleFocus::Repositories
        };
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

impl ResumeWizard {
    fn active_index_mut(&mut self) -> &mut usize {
        match self.step {
            WizardStep::Profile => &mut self.profile,
            WizardStep::Target => &mut self.target,
            WizardStep::Review => unreachable!("review input has no picker index"),
            WizardStep::Bundle => unreachable!("resume does not select a bundle"),
            WizardStep::Mounts => unreachable!("resume does not select mounts"),
            WizardStep::NewBundle => unreachable!("resume does not create bundles"),
            WizardStep::ProjectDirectory => {
                unreachable!("resume does not select a project directory")
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PickerNavigation {
    pub(crate) has_back: bool,
    /// Row the keyboard is on, highlighted while the content has focus.
    pub(crate) selected: usize,
    pub(crate) control: WizardControl,
    pub(crate) next_enabled: bool,
}

/// One picker row. A disabled row stays in the list so row numbers keep
/// matching the underlying map order; it is greyed out and refuses Enter.
#[derive(Debug, Clone)]
pub(crate) struct PickerChoice {
    pub(crate) text: String,
    pub(crate) disabled: bool,
}

impl From<String> for PickerChoice {
    fn from(text: String) -> Self {
        Self {
            text,
            disabled: false,
        }
    }
}

// The form and surface registry are distinct rendering owners.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_picker(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    choices: Vec<PickerChoice>,
    help: &[&str],
    navigation: PickerNavigation,
    form: &mut Form<WizardControl>,
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
        .min(content.height.saturating_sub(help.len() as u16 + 2));
    let list_area = Rect::new(content.x, content.y, content.width, list_height);
    let rows = choices
        .iter()
        .map(|choice| Line::raw(choice.text.clone()))
        .collect::<Vec<_>>();
    let row_map = (0..rows.len()).map(Some).collect::<Vec<_>>();
    let row_enabled = choices
        .iter()
        .map(|choice| !choice.disabled)
        .collect::<Vec<_>>();
    let help_y = list_area.y.saturating_add(list_area.height);
    let help_height = (help.len() as u16).min(content.bottom().saturating_sub(help_y + 1));
    let help_area = Rect::new(content.x, help_y, content.width, help_height);
    let button_y = content.bottom().saturating_sub(1);
    let button_area = Rect::new(content.x, button_y, content.width, 1.min(content.height));
    frame.render_widget(theme::modal().title(title), popup);
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
    frame.render_widget(
        Paragraph::new(
            help.iter()
                .map(|line| Line::styled(*line, Style::default().fg(theme::palette().muted)))
                .collect::<Vec<_>>(),
        ),
        help_area,
    );
    let mut buttons = vec![(WizardControl::Cancel, "Cancel", true)];
    if navigation.has_back {
        buttons.push((WizardControl::Back, "Back", true));
    }
    buttons.push((WizardControl::Next, "Next", navigation.next_enabled));
    ButtonRow::render(frame, button_area, &buttons, form);
}

fn begin_form_frame(form: &mut Form<WizardControl>, initial: WizardControl) {
    let previous = form.focused();
    form.begin_frame();
    if previous != Some(initial) && !form.captures_pointer() {
        // A domain step transition may choose a control not registered yet.
        // Its pending focus becomes visible as soon as that control renders.
        form.focus(initial);
    }
}

fn wizard_control(
    step: WizardStep,
    focus: WizardFocus,
    review_focus: ReviewFocus,
    mounts: &MountWizard,
) -> WizardControl {
    match step {
        WizardStep::Profile => match focus {
            WizardFocus::Content => WizardControl::ProfileList,
            WizardFocus::Cancel => WizardControl::Cancel,
            WizardFocus::Back => WizardControl::Back,
            WizardFocus::Next => WizardControl::Next,
        },
        WizardStep::Bundle => match focus {
            WizardFocus::Content => WizardControl::BundleList,
            WizardFocus::Cancel => WizardControl::Cancel,
            WizardFocus::Back => WizardControl::Back,
            WizardFocus::Next => WizardControl::Next,
        },
        WizardStep::Target => match focus {
            WizardFocus::Content => WizardControl::TargetList,
            WizardFocus::Cancel => WizardControl::Cancel,
            WizardFocus::Back => WizardControl::Back,
            WizardFocus::Next => WizardControl::Next,
        },
        WizardStep::ProjectDirectory => match focus {
            WizardFocus::Content => WizardControl::ProjectDirectory,
            WizardFocus::Cancel => WizardControl::Cancel,
            WizardFocus::Back => WizardControl::Back,
            WizardFocus::Next => WizardControl::Next,
        },
        WizardStep::NewBundle => match focus {
            WizardFocus::Content => WizardControl::NewBundleSource,
            WizardFocus::Cancel => WizardControl::Cancel,
            WizardFocus::Back => WizardControl::Back,
            WizardFocus::Next => WizardControl::Next,
        },
        WizardStep::Mounts => match mounts.focus {
            MountFocus::Source => WizardControl::MountSource,
            MountFocus::Destination => WizardControl::MountDestination,
            MountFocus::ReadOnly => WizardControl::MountReadOnly,
            MountFocus::Cancel => WizardControl::Cancel,
            MountFocus::Back => WizardControl::Back,
            MountFocus::Add => WizardControl::Add,
        },
        WizardStep::Review => match review_focus {
            ReviewFocus::Attachments => WizardControl::ReviewAttachments,
            ReviewFocus::Cancel => WizardControl::Cancel,
            ReviewFocus::Back => WizardControl::Back,
            ReviewFocus::Add => WizardControl::Add,
            ReviewFocus::Submit => WizardControl::Submit,
        },
    }
}

fn new_wizard_control(wizard: &NewWizard) -> WizardControl {
    if wizard.step == WizardStep::NewBundle {
        return match wizard.new_bundle_focus {
            NewBundleFocus::Repositories => WizardControl::NewBundleRepositories,
            NewBundleFocus::Source => WizardControl::NewBundleSource,
            NewBundleFocus::Add => WizardControl::Add,
            NewBundleFocus::Remove => WizardControl::NewBundleRemove,
            NewBundleFocus::Cancel => WizardControl::Cancel,
            NewBundleFocus::Back => WizardControl::Back,
            NewBundleFocus::Create => WizardControl::Next,
        };
    }
    wizard_control(
        wizard.step,
        wizard.focus,
        wizard.review_focus,
        &wizard.mounts,
    )
}

pub(crate) fn render_new_wizard(
    frame: &mut Frame,
    area: Rect,
    dashboard: &DashboardState,
    wizard: &NewWizard,
    surfaces: &mut FrameSurfaces,
) {
    let mut form = wizard.form.borrow_mut();
    let initial = new_wizard_control(wizard);
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
                profile_id: &nth_key(&dashboard.config.profiles, wizard.profile),
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
                submit_enabled: true,
                active_interruption: false,
                clear_resource_allocation: false,
                queue: None,
                queued_entries: &[],
                prepared_entries: &[],
                remote_repositories: wizard.remote_repositories.as_deref(),
                remote_preflight_in_flight: wizard.remote_preflight_in_flight,
                remote_preflight_error: wizard.remote_preflight_error.as_deref(),
                local_changes_excluded: !raw_project,
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
            "Enter validates · Backspace on empty goes back · Esc cancels",
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
        frame.render_widget(
            theme::modal().title(if local {
                " New session · 3/4 local project "
            } else {
                " New session · 3/4 remote project "
            }),
            popup,
        );
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
        TextField::render(
            frame,
            Rect::new(content.x, field_y, content.width, 1.min(content.height)),
            &wizard.project_directory,
            &mut form,
            WizardControl::ProjectDirectory,
        );
        ButtonRow::render(
            frame,
            Rect::new(content.x, button_y, content.width, 1.min(content.height)),
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
        frame.render_widget(theme::modal().title(" New bundle "), popup);
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
        TextField::render(
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
        ButtonRow::render(
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
        ButtonRow::render(
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
    let (title, choices, selected): (_, Vec<String>, _) = match wizard.step {
        WizardStep::Profile => (
            " New session · 1/4 profile ",
            dashboard
                .config
                .profiles
                .iter()
                .map(|(id, profile)| dashboard.profile_choice(id, profile.kind))
                .collect(),
            wizard.profile,
        ),
        WizardStep::Bundle => (
            " New session · 3/4 project bundle ",
            bundle_ids_by_recent_creation(&dashboard.config, &dashboard.state)
                .into_iter()
                .map(|id| {
                    let bundle = &dashboard.config.bundles[id];
                    format!("{id}  {} repositories", bundle.repositories.len())
                })
                .chain(["New bundle…".to_owned()])
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
                    format!("{id}  {}{size}", target_label(target))
                })
                .collect(),
            wizard.target,
        ),
        WizardStep::Review => unreachable!("review was rendered above"),
        WizardStep::Mounts => unreachable!("mount input was rendered above"),
        WizardStep::NewBundle => unreachable!("bundle input was rendered above"),
        WizardStep::ProjectDirectory => unreachable!("project directory input was rendered above"),
    };
    let help = if wizard.step == WizardStep::Target {
        "+ double · - halve · c +8 CPU · m +50% memory · r reset"
    } else {
        "↑/↓ select · Tab moves focus · Enter activates"
    };
    render_picker(
        frame,
        area,
        title,
        choices.into_iter().map(PickerChoice::from).collect(),
        &[help],
        PickerNavigation {
            has_back: wizard.step != WizardStep::Profile,
            selected,
            control: match wizard.step {
                WizardStep::Profile => WizardControl::ProfileList,
                WizardStep::Bundle => WizardControl::BundleList,
                WizardStep::Target => WizardControl::TargetList,
                _ => unreachable!("picker step has a list control"),
            },
            next_enabled: wizard.step != WizardStep::Target
                || wizard.resource_allocation.is_some()
                || !matches!(
                    dashboard
                        .config
                        .targets
                        .get(&nth_key(&dashboard.config.targets, wizard.target)),
                    Some(TargetTemplate::AwsEc2 { .. })
                ),
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
    clear_resource_allocation: bool,
    queue: Option<(usize, bool)>,
    queued_entries: &'a [hel::hel_worker::QueuedPrompt],
    prepared_entries: &'a [MaterializedQueuedPrompt],
    remote_repositories: Option<&'a [RemoteRepositoryPreview]>,
    remote_preflight_in_flight: bool,
    remote_preflight_error: Option<&'a str>,
    local_changes_excluded: bool,
}

fn render_review_wizard(
    frame: &mut Frame,
    area: Rect,
    dashboard: &DashboardState,
    view: ReviewWizardView<'_>,
    form: &mut Form<WizardControl>,
    surfaces: &mut FrameSurfaces,
) {
    let ReviewWizardView {
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
        clear_resource_allocation,
        queue,
        queued_entries,
        prepared_entries,
        remote_repositories,
        remote_preflight_in_flight,
        remote_preflight_error,
        local_changes_excluded,
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
            "Resolving network repositories…",
            Style::default().fg(theme::palette().muted),
        ));
    } else if let Some(error) = remote_preflight_error {
        lines.push(Line::styled(
            format!("Repository preflight failed: {error}"),
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
    let queue_label = queue.map(|(count, _)| format!("Queued prompts: {count}"));
    if let Some(label) = &queue_label {
        lines.push(Line::raw(label.clone()));
    }
    // Guardian targets rely on the harness's own approval mode rather than
    // Hel-managed isolation.
    if (matches!(target, TargetTemplate::LocalBare)
        || target.permission_mode() == Some(hel::hel_config::PermissionMode::Guardian))
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
    frame.render_widget(theme::modal().title(title), popup);
    let body = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner.height.saturating_sub(1),
    );
    let focused_row = match form.focused() {
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
                    read_only_marker(mount.read_only)
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
                hel::hel_state::QueuedCommandKind::Prompt => (
                    "prompt",
                    hel::hel_transcript::materialized_content_text(&entry.content),
                ),
                hel::hel_state::QueuedCommandKind::SetConfig { key, value } => {
                    ("config", hel::hel_state::config_command_text(key, value))
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
    let button_y = inner.bottom().saturating_sub(1);
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
    ButtonRow::render(
        frame,
        Rect::new(inner.x, button_y, inner.width, 1.min(inner.height)),
        &buttons,
        form,
    );
}

/// Suffix that marks an attached directory as read-only in a list row.
pub(crate) fn read_only_marker(read_only: bool) -> &'static str {
    if read_only { " · ro" } else { "" }
}

// Domain mount data, navigation, and the shared form are separate inputs.
#[allow(clippy::too_many_arguments)]
fn render_mount_wizard(
    frame: &mut Frame,
    area: Rect,
    dashboard: &DashboardState,
    target_index: usize,
    mounts: &MountWizard,
    form: &mut Form<WizardControl>,
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
                read_only_marker(mount.read_only)
            ))
        }));
    }
    if mounts.focus == MountFocus::Source && mounts.source.is_empty() && !mounts.history.is_empty()
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
    frame.render_widget(theme::modal().title(title), popup);
    let body = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner.height.saturating_sub(1),
    );
    let focused_row = match form.focused() {
        Some(WizardControl::MountSource) => Some(info_height),
        Some(WizardControl::MountDestination) => Some(info_height.saturating_add(1)),
        Some(WizardControl::MountReadOnly) => Some(info_height.saturating_add(2)),
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
    TextField::render(
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
    TextField::render(
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
    let readonly_row = viewport.row(info_height.saturating_add(2), 1);
    let readonly_label = if mounts.forced_read_only().is_some() {
        "Read-only (locked)"
    } else {
        "Read-only"
    };
    Checkbox::render(
        frame,
        readonly_row,
        readonly_label,
        mounts.read_only,
        mounts.forced_read_only().is_none(),
        form,
        WizardControl::MountReadOnly,
    );
    let button_y = inner.bottom().saturating_sub(1);
    ButtonRow::render(
        frame,
        Rect::new(inner.x, button_y, inner.width, 1.min(inner.height)),
        &[
            (WizardControl::Cancel, "Cancel", true),
            (WizardControl::Back, "Back", true),
            (WizardControl::Add, "Add directory", true),
        ],
        form,
    );
}

pub(crate) fn render_resume_wizard(
    frame: &mut Frame,
    area: Rect,
    dashboard: &DashboardState,
    wizard: &ResumeWizard,
    surfaces: &mut FrameSurfaces,
) {
    let mut form = wizard.form.borrow_mut();
    let initial = wizard_control(
        wizard.step,
        wizard.focus,
        wizard.review_focus,
        &wizard.mounts,
    );
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
                                    hel::hel_config::TargetTemplate::LocalBare
                                        | hel::hel_config::TargetTemplate::SshBare { .. }
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
        WizardStep::Profile => (
            if wizard.moving {
                " Move · 1/3 profile (cross-harness supported) "
            } else {
                " Resume · 1/3 profile (cross-harness supported) "
            },
            dashboard
                .compatible_profiles(&wizard.session_id)
                .into_iter()
                .map(|(id, harness)| {
                    let mut choice = dashboard.profile_choice(id, harness);
                    if dashboard
                        .state
                        .sessions
                        .get(&wizard.session_id)
                        .is_some_and(|session| session.harness_kind != harness)
                    {
                        choice.insert_str(id.len(), "  (lossy: text-only transcript)");
                    }
                    PickerChoice::from(choice)
                })
                .collect(),
            wizard.profile,
            &[
                "↑/↓ select · Tab moves focus · Enter activates",
                "Lossy: text only; tool calls + reasoning dropped.",
            ][..],
        ),
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
                        Some(reason) => PickerChoice {
                            text: format!("{id}  {}  · {reason}", target_label(target)),
                            disabled: true,
                        },
                        None => PickerChoice::from(format!("{id}  {}{size}", target_label(target))),
                    }
                })
                .collect(),
            wizard.target,
            &["+ double · - halve · c +8 CPU · m +50% memory · r reset"][..],
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

fn nth_bundle_key(config: &HelConfig, state: &HelState, index: usize) -> String {
    bundle_ids_by_recent_creation(config, state)
        .get(index)
        .expect("wizard is only opened for non-empty configuration")
        .to_string()
}

fn most_recent_configured_session<'a>(
    config: &HelConfig,
    state: &'a HelState,
) -> Option<&'a SessionRecord> {
    state
        .sessions
        .values()
        .filter(|session| {
            config.profiles.contains_key(&session.last_profile)
                && config.bundles.contains_key(&session.bundle_id)
                && config.targets.contains_key(&session.target_template_id)
        })
        .max_by_key(|session| {
            chrono::DateTime::parse_from_rfc3339(&session.created_at)
                .ok()
                .map(|timestamp| timestamp.timestamp_millis())
        })
}

fn bundle_ids_by_recent_creation<'a>(config: &'a HelConfig, state: &HelState) -> Vec<&'a str> {
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
