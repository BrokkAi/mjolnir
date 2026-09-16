//! Modal dialogs: session import, confirmations, and the rename editor.

mod container;
#[cfg(test)]
use container::ContainerEditFocus;
pub(crate) use container::{ContainerEditor, render_container_editor};

use std::cell::RefCell;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use mj_chat::theme;
use qrcode::QrCode;
use qrcode::types::{Color as QrColor, EcLevel};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use std::path::PathBuf;

use mj_core::config::{HarnessKind, mount_history_host};
use mj_core::state::{MoveOperation, MovePhase, ResumeQueueDisposition};

use mj_chat::components::{
    Button, Checkbox, ChoiceList, ControlKind, Dialog, Interaction, Outcome, TextField,
};
use mj_chat::selection::FrameSurfaces;
use mj_chat::text_input::TextInput;
use mj_chat::{components::PathField, path_input::PathInput};
use mj_core::targets::{AdditionalMount, default_mount_destination, validate_additional_mounts};

use crate::widgets::{
    centered_modal, centered_modal_fixed, dismissible_modal_title, modal_area, popup_height,
    truncate_text,
};
use crate::wizards::read_only_marker;
use crate::{
    DashboardAction, DashboardState, Mode, WebListenerProcess, WebViewerAccess, WebViewerRecovery,
};

const IMPORT_STALL_WARNING_AFTER: Duration = Duration::from_secs(10);

/// Stable control identities used by the standard dashboard dialogs.
///
/// Each dialog owns its own [`Dialog`], so the shared identities can be reused
/// across modes while retaining focus and pointer state during redraws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DialogControl {
    Field,
    Cancel,
    Save,
    Primary,
    TargetList,
    TargetRename,
    TargetTest,
    TargetSettings,
    ConfirmButton(usize),
    ImportIgnore,
    ImportManagedWorktree,
    ImportCancel,
    ImportContinue,
    WebRetry,
    WebAnotherPort,
    WebInspect,
    WebNextProcess,
    WebStop,
    WebConfirmStop,
    WebCancelStop,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportSessionOption {
    pub native_session_id: String,
    pub title: String,
    pub project_directory: String,
    pub details: String,
    pub unavailable_reason: Option<String>,
    /// When the harness last wrote this session's file, in epoch milliseconds.
    /// The resume dialog sorts hel records and native sessions against each
    /// other and renders their activity in one column, so the raw instant
    /// travels alongside the details.
    pub last_activity_ms: i64,
    /// Archived inside the harness itself; only Codex reports this. Hel
    /// mirrors it one way and never writes the harness home back.
    pub natively_archived: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportProfileOption {
    pub profile_id: String,
    pub harness_kind: HarnessKind,
    pub sessions: Vec<ImportSessionOption>,
    pub scan_progress: Option<(usize, usize)>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RenameEditor {
    pub(crate) session_id: String,
    pub(crate) title: TextInput,
    pub(crate) form: RefCell<Dialog<DialogControl>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfigEntryKind {
    Profile,
    Target,
}

impl ConfigEntryKind {
    fn label(self) -> &'static str {
        match self {
            Self::Profile => "profile",
            Self::Target => "target",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfigIdEditor {
    pub(crate) return_to: Option<Box<TargetActionsDialog>>,
    pub(crate) kind: ConfigEntryKind,
    pub(crate) old_id: String,
    pub(crate) value: TextInput,
    pub(crate) form: RefCell<Dialog<DialogControl>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TargetActionsDialog {
    pub(crate) target_ids: Vec<String>,
    pub(crate) target_index: usize,
    pub(crate) form: RefCell<Dialog<DialogControl>>,
    pub(crate) testing: Option<String>,
    pub(crate) result: Option<(String, Result<(), String>)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WebDialog {
    pub(crate) loading: bool,
    pub(crate) viewer_url: Option<String>,
    pub(crate) viewer_code: Option<String>,
    pub(crate) fallback_reason: Option<String>,
    pub(crate) message: Option<String>,
    pub(crate) qr: Option<String>,
    pub(crate) failed_address: Option<std::net::SocketAddr>,
    pub(crate) port_conflict: bool,
    pub(crate) inspecting: bool,
    pub(crate) listeners: Vec<WebListenerProcess>,
    pub(crate) listener_index: usize,
    pub(crate) inspection_message: Option<String>,
    pub(crate) confirm_stop: Option<WebListenerProcess>,
    pub(crate) form: RefCell<Dialog<DialogControl>>,
}

impl WebDialog {
    pub(crate) fn loading() -> Self {
        Self {
            loading: true,
            viewer_url: None,
            viewer_code: None,
            fallback_reason: None,
            message: None,
            qr: None,
            failed_address: None,
            port_conflict: false,
            inspecting: false,
            listeners: Vec::new(),
            listener_index: 0,
            inspection_message: None,
            confirm_stop: None,
            form: dialog_form(&[DialogControl::WebRetry], DialogControl::WebRetry),
        }
    }
}

impl WebDialog {
    fn button_rows(&self) -> Vec<Vec<(DialogControl, &'static str, bool)>> {
        use DialogControl::*;
        if self.confirm_stop.is_some() {
            return vec![vec![
                (WebCancelStop, "Cancel", true),
                (WebConfirmStop, "Stop and retry", true),
            ]];
        }
        if self.loading || self.failed_address.is_none() {
            return vec![Vec::new()];
        }
        let mut rows = vec![vec![
            (WebAnotherPort, "Use another port", !self.inspecting),
            (WebRetry, "Retry", !self.inspecting),
        ]];
        if let Some(process) = self.listeners.get(self.listener_index) {
            let mut buttons = vec![(
                WebStop,
                "Stop server…",
                process.stop_disabled_reason.is_none(),
            )];
            if self.listeners.len() > 1 {
                buttons.push((WebNextProcess, "Next process", true));
            }
            rows.push(buttons);
        }
        let mut footer = Vec::new();
        if self.port_conflict {
            footer.push((WebInspect, "Inspect port", !self.inspecting));
        }
        rows.push(footer);
        rows
    }

    fn default_control(&self) -> DialogControl {
        if self.confirm_stop.is_some() {
            DialogControl::WebCancelStop
        } else if self.failed_address.is_some() && !self.loading && !self.inspecting {
            DialogControl::WebAnotherPort
        } else {
            DialogControl::WebRetry
        }
    }

    fn reset_form(&mut self) {
        let controls = self
            .button_rows()
            .into_iter()
            .flatten()
            .map(|(id, _, _)| id)
            .collect::<Vec<_>>();
        self.form = dialog_form(&controls, self.default_control());
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepositoryOriginDialog {
    pub(crate) session_id: String,
    pub(crate) repository_id: String,
    pub(crate) missing_commit: String,
    pub(crate) archived_origin: String,
    pub(crate) configured_origin: String,
    pub(crate) replacement: PathInput,
    pub(crate) error: Option<String>,
    pub(crate) form: RefCell<Dialog<DialogControl>>,
    pub(crate) launch: Box<DashboardAction>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Confirmation {
    RepairRepositoryRemotes {
        action: Box<DashboardAction>,
        repairs: Vec<mj_core::local_git::LocalRemoteRepair>,
        previous: Box<Mode>,
    },
    ConfigurationRepair {
        session_id: String,
        error: String,
        previous: Box<Mode>,
    },
    LaunchFailed {
        error: String,
        retry: Option<Box<DashboardAction>>,
        previous: Box<Mode>,
    },
    /// A standard modal dismissal that may need to protect an in-memory draft
    /// or a running operation. The boxed mode is restored by the safe answer.
    Dismiss {
        mode: Box<Mode>,
        intent: DismissalIntent,
    },
    CloseFailed {
        session_id: String,
        error: String,
    },
    StopWithSubagents {
        session_id: String,
        count: usize,
    },
    ForceDestroy {
        session_id: String,
    },
    DestroyStopped {
        session_id: String,
        /// The resume dialog to restore afterwards, so confirming or
        /// cancelling destruction leaves the user where they were.
        reopen: Option<Box<crate::resume::ResumeDialog>>,
    },
    /// Enter on a failed session. Opening its conversation and recovering it
    /// are both reasonable answers, and recovery replaces the target, so the
    /// surface asks rather than guessing.
    RecoverFailed {
        session_id: String,
        error: Option<String>,
        /// Whether a verified recovery copy exists to resume from. Without one
        /// there is nothing to recover and only the transcript is on offer.
        recoverable: bool,
    },
    /// A resume that moves a local checkout into an isolated workspace. The
    /// preflight already produced the receipt and the preview; this asks the
    /// person to agree to what travels before anything is stopped.
    ConvertRawCheckout {
        launch: Box<DashboardAction>,
        receipt: Box<mj_core::state::ResumeRepositorySourceReceipt>,
        preview: Box<mj_core::state::RawConversionPreview>,
        previous: Box<Mode>,
    },
    /// A Move left a verified checkpoint or a ready destination that needs an
    /// explicit same-destination retry. Resume with the source settings is
    /// deliberately hidden while queue admission may already have run.
    RecoverMove {
        operation: Box<MoveOperation>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DismissalIntent {
    DiscardSetup,
    CancelImport,
}

/// A confirmation dialog and its persistent standard-control state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfirmDialog {
    scroll: u16,
    max_scroll: std::cell::Cell<u16>,
    pub(crate) confirmation: Confirmation,
    pub(crate) form: RefCell<Dialog<DialogControl>>,
}

impl ConfirmDialog {
    pub(crate) fn new(confirmation: Confirmation) -> Self {
        Self {
            scroll: 0,
            max_scroll: std::cell::Cell::new(0),
            form: confirmation_form(&confirmation),
            confirmation,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImportProgress {
    pub(crate) session_title: String,
    pub(crate) step: usize,
    total: Option<usize>,
    pub(crate) message: String,
    last_updated: Instant,
    pub(crate) form: RefCell<Dialog<DialogControl>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImportBundleConfirmation {
    managed_worktree: mj_core::state::ManagedWorktreeOptions,
    create_managed_worktree: bool,
    dirty_git_roots: Vec<String>,
    omitted_non_git_dirs: Vec<String>,
    scratch_git_roots: Vec<String>,
    has_untracked_files: bool,
    ignore_untracked: bool,
    pub(crate) form: RefCell<Dialog<DialogControl>>,
}

fn dialog_form(
    controls: &[DialogControl],
    initial: DialogControl,
) -> RefCell<Dialog<DialogControl>> {
    let mut form = Dialog::new();
    for id in controls {
        let kind = match id {
            DialogControl::Field => ControlKind::TextField,
            DialogControl::TargetList => ControlKind::ChoiceList {
                len: 0,
                selected: 0,
            },
            DialogControl::ImportIgnore | DialogControl::ImportManagedWorktree => {
                ControlKind::Checkbox
            }
            _ => ControlKind::Button,
        };
        form.declare(*id, kind);
    }
    form.end_frame(initial);
    RefCell::new(form)
}

fn confirmation_form(confirmation: &Confirmation) -> RefCell<Dialog<DialogControl>> {
    let buttons = confirmation_buttons(confirmation);
    let mut form = Dialog::new();
    for index in 0..buttons.len() {
        form.declare(DialogControl::ConfirmButton(index), ControlKind::Button);
    }
    let initial = initial_confirmation_button(confirmation, buttons);
    form.end_frame(DialogControl::ConfirmButton(initial));
    RefCell::new(form)
}

fn target_actions_form(
    target_count: usize,
    selected: usize,
    initial: DialogControl,
) -> RefCell<Dialog<DialogControl>> {
    let mut form = Dialog::new();
    form.declare(
        DialogControl::TargetList,
        ControlKind::ChoiceList {
            len: target_count,
            selected: selected.min(target_count.saturating_sub(1)),
        },
    );
    form.declare(DialogControl::TargetRename, ControlKind::Button);
    form.declare(DialogControl::TargetTest, ControlKind::Button);
    form.declare(DialogControl::TargetSettings, ControlKind::Button);
    form.end_frame(initial);
    RefCell::new(form)
}

fn sync_target_actions_form(dialog: &mut TargetActionsDialog) {
    let form = dialog.form.get_mut();
    form.declare(
        DialogControl::TargetList,
        ControlKind::ChoiceList {
            len: dialog.target_ids.len(),
            selected: dialog.target_index,
        },
    );
    form.declare(DialogControl::TargetRename, ControlKind::Button);
    form.declare_with_enabled(
        DialogControl::TargetTest,
        ControlKind::Button,
        dialog.testing.is_none(),
    );
    form.declare(DialogControl::TargetSettings, ControlKind::Button);
    form.end_frame(DialogControl::TargetList);
}

fn clear_dialog_form_geometry(form: &mut Dialog<DialogControl>) {
    // Keep declarations available for keyboard input while the modal is
    // clipped, but discard hitboxes and any in-flight mouse gesture.
    form.cancel_pointer();
    form.reset_geometry();
}

/// Button labels for a confirmation dialog, ordered Cancel first and the primary
/// action last. This is the single declaration used by both key handling and
/// rendering.
pub(crate) fn confirmation_buttons(confirmation: &Confirmation) -> &'static [&'static str] {
    match confirmation {
        Confirmation::RepairRepositoryRemotes { .. } => &["Cancel", "Repair and continue"],
        Confirmation::ConfigurationRepair { .. } => {
            &["Dismiss", "Open transcript", "Open settings"]
        }
        Confirmation::LaunchFailed { retry: Some(_), .. } => {
            &["Dismiss", "Retry launch", "Settings"]
        }
        Confirmation::LaunchFailed { .. } => &["Dismiss", "Settings"],
        Confirmation::Dismiss {
            intent: DismissalIntent::DiscardSetup,
            ..
        } => &["Keep editing", "Discard settings"],
        Confirmation::Dismiss {
            intent: DismissalIntent::CancelImport,
            ..
        } => &["Keep importing", "Cancel import"],
        Confirmation::ConvertRawCheckout { .. } => &["Cancel", "Confirm"],
        Confirmation::DestroyStopped { .. } => &["No", "Yes"],
        Confirmation::CloseFailed { .. } => &["Cancel", "Force stop", "Retry stop"],
        Confirmation::StopWithSubagents { .. } => &["Cancel", "Stop children and parent"],
        Confirmation::RecoverFailed {
            recoverable: true, ..
        } => &["Cancel", "Open transcript", "Recover"],
        Confirmation::RecoverFailed { .. } => &["Cancel", "Open transcript"],
        Confirmation::RecoverMove { operation }
            if operation.queue_admission_started && !operation.queue_admission_finished =>
        {
            &["Cancel", "Open transcript", "Retry move"]
        }
        Confirmation::RecoverMove { .. } => &[
            "Cancel",
            "Open transcript",
            "Retry move",
            "Resume previous settings",
        ],
        Confirmation::ForceDestroy { .. } => &["No", "Yes"],
    }
}

/// Index of the primary (rightmost) button, which is focused when a dialog opens.
fn primary_button(labels: &[&str]) -> usize {
    labels.len().saturating_sub(1)
}

fn initial_confirmation_button(confirmation: &Confirmation, labels: &[&str]) -> usize {
    if matches!(
        confirmation,
        Confirmation::Dismiss { .. }
            | Confirmation::ConfigurationRepair { .. }
            | Confirmation::LaunchFailed { .. }
            | Confirmation::ForceDestroy { .. }
            | Confirmation::DestroyStopped { .. }
            | Confirmation::CloseFailed { .. }
            | Confirmation::StopWithSubagents { .. }
            | Confirmation::RepairRepositoryRemotes { .. }
            | Confirmation::ConvertRawCheckout { .. }
    ) {
        0
    } else {
        primary_button(labels)
    }
}

pub(crate) fn import_progress_status(progress: &ImportProgress) -> Line<'static> {
    let stalled_for = progress.last_updated.elapsed();
    if stalled_for >= IMPORT_STALL_WARNING_AFTER {
        Line::styled(
            format!(
                "No progress for {}s; the filesystem may be stalled.",
                stalled_for.as_secs()
            ),
            Style::default().fg(theme::palette().warning),
        )
    } else {
        Line::styled(
            "The dashboard remains responsive while the import runs.",
            Style::default().fg(theme::palette().muted),
        )
    }
}

/// Finds an active import even while a cancellation confirmation temporarily
/// owns the foreground. Progress replies must keep updating the preserved
/// dialog so rejecting the confirmation returns to current state.
pub(crate) fn import_progress_mut(mode: &mut Mode) -> Option<&mut ImportProgress> {
    match mode {
        Mode::Importing(progress) => Some(progress),
        Mode::Help(overlay) => import_progress_mut(overlay.return_to.as_mut()),
        Mode::Confirm(dialog) => match &mut dialog.confirmation {
            Confirmation::Dismiss { mode, .. } => import_progress_mut(mode.as_mut()),
            _ => None,
        },
        _ => None,
    }
}

pub(crate) fn render_import_progress(
    frame: &mut Frame,
    area: Rect,
    progress: &ImportProgress,
    surfaces: &mut FrameSurfaces,
) {
    let total = progress
        .total
        .map_or_else(|| "?".into(), |total| total.to_string());
    let status = import_progress_status(progress);
    let paragraph = Paragraph::new(vec![
        Line::styled(
            truncate_text(&progress.session_title, 60),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Line::raw(""),
        Line::raw(progress.message.clone()),
        status,
        Line::raw(""),
    ])
    .wrap(Wrap { trim: false });
    let popup = centered_modal(
        frame,
        surfaces,
        76,
        popup_height(&paragraph, 76, 11, area),
        area,
    );
    let inner = popup.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 1,
    });
    if inner.height == 0 {
        clear_dialog_form_geometry(&mut progress.form.borrow_mut());
        return;
    }
    frame.render_widget(
        paragraph,
        Rect::new(
            inner.x,
            inner.y,
            inner.width,
            inner.height.saturating_sub(1),
        ),
    );
    let mut form = progress.form.borrow_mut();
    form.begin_frame();
    let title = dismissible_modal_title(
        &mut form,
        popup,
        format!("Importing session · progress {}/{}", progress.step, total),
        theme::title(true),
        true,
    );
    frame.render_widget(theme::modal().title(title), popup);
    Button::render(
        frame,
        Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1),
        "Cancel",
        true,
        &mut form,
        DialogControl::Cancel,
    );
    form.end_frame(DialogControl::Cancel);
}

pub(crate) fn render_import_bundle_confirmation(
    frame: &mut Frame,
    area: Rect,
    confirmation: &ImportBundleConfirmation,
    surfaces: &mut FrameSurfaces,
) {
    let mut lines = Vec::new();
    if !confirmation.dirty_git_roots.is_empty() {
        lines.push(Line::raw(
            "These Git roots have local changes; Mjolnir will archive tracked changes:",
        ));
        lines.extend(
            confirmation.dirty_git_roots.iter().map(|root| {
                Line::styled(root.clone(), Style::default().fg(theme::palette().warning))
            }),
        );
    }
    if !confirmation.omitted_non_git_dirs.is_empty() {
        if !lines.is_empty() {
            lines.push(Line::raw(""));
        }
        lines.push(Line::raw(
            "These edited directories are outside Git and cannot be included:",
        ));
        lines.extend(confirmation.omitted_non_git_dirs.iter().map(|directory| {
            Line::styled(
                directory.clone(),
                Style::default().fg(theme::palette().warning),
            )
        }));
    }
    if !confirmation.scratch_git_roots.is_empty() {
        if !lines.is_empty() {
            lines.push(Line::raw(""));
        }
        lines.push(Line::raw(
            "These scratch repositories are under temporary directories and stay out of the workspace:",
        ));
        lines.extend(
            confirmation.scratch_git_roots.iter().map(|root| {
                Line::styled(root.clone(), Style::default().fg(theme::palette().warning))
            }),
        );
    }
    lines.push(Line::raw(""));
    if confirmation.has_untracked_files {
        lines.push(Line::raw("Space toggles the checkbox."));
    }
    lines.push(Line::raw(if confirmation.managed_worktree.available {
        if confirmation.create_managed_worktree {
            "On resume, create a separate session-owned checkout."
        } else {
            "On resume, use the imported session's directory directly."
        }
    } else {
        "This import uses an isolated workspace."
    }));
    let control_lines = usize::from(confirmation.has_untracked_files)
        + usize::from(confirmation.managed_worktree.available)
        + 2;
    let body_paragraph = Paragraph::new(lines.clone()).wrap(Wrap { trim: false });
    let height = popup_height(
        &body_paragraph,
        76,
        u16::try_from(control_lines)
            .unwrap_or(u16::MAX)
            .saturating_add(10),
        area,
    );
    let popup = centered_modal(frame, surfaces, 76, height, area);
    let inner = popup.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 1,
    });
    if inner.height == 0 {
        clear_dialog_form_geometry(&mut confirmation.form.borrow_mut());
        return;
    }
    let body_height = inner
        .height
        .saturating_sub(u16::try_from(control_lines).unwrap_or(u16::MAX));
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }),
        Rect::new(inner.x, inner.y, inner.width, body_height),
    );
    let mut form = confirmation.form.borrow_mut();
    form.begin_frame();
    let title =
        dismissible_modal_title(&mut form, popup, "Confirm import", theme::title(true), true);
    frame.render_widget(theme::modal().title(title), popup);
    let y = inner.y.saturating_add(body_height);
    // The worktree choice only exists when the import can create one.
    if confirmation.managed_worktree.available {
        Checkbox::render(
            frame,
            Rect::new(inner.x, y, inner.width, 1),
            "Create managed worktree",
            confirmation.create_managed_worktree,
            true,
            &mut form,
            DialogControl::ImportManagedWorktree,
        );
    }
    let y = y.saturating_add(u16::from(confirmation.managed_worktree.available));
    if confirmation.has_untracked_files {
        Checkbox::render(
            frame,
            Rect::new(inner.x, y, inner.width, 1),
            "Ignore untracked files",
            confirmation.ignore_untracked,
            true,
            &mut form,
            DialogControl::ImportIgnore,
        );
    }
    let footer = Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1);
    Dialog::render_actions(
        frame,
        footer,
        &[
            (DialogControl::ImportCancel, "Cancel", true),
            (DialogControl::ImportContinue, "Continue", true),
        ],
        &mut form,
    );
    form.end_frame(DialogControl::ImportContinue);
}

/// Editable per-session container provisioning inputs: the size overrides and
/// the attached host directories. Nothing here is written to config.toml.
pub(crate) fn render_rename_editor(
    frame: &mut Frame,
    area: Rect,
    editor: &RenameEditor,
    surfaces: &mut FrameSurfaces,
) {
    let popup = centered_modal(frame, surfaces, 60, 8, area);
    let inner = popup.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 1,
    });
    if inner.height == 0 {
        clear_dialog_form_geometry(&mut editor.form.borrow_mut());
        return;
    }
    frame.render_widget(
        Paragraph::new(format!("Session: {}", editor.session_id)),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    let field = Rect::new(inner.x, inner.y.saturating_add(2), inner.width, 1);
    let footer = Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1);
    let mut form = editor.form.borrow_mut();
    form.begin_frame();
    let title =
        dismissible_modal_title(&mut form, popup, "Rename session", theme::title(true), true);
    frame.render_widget(theme::modal().title(title), popup);
    TextField::render(frame, field, &editor.title, &mut form, DialogControl::Field);
    Dialog::render_actions(
        frame,
        footer,
        &[
            (DialogControl::Cancel, "Cancel", true),
            (DialogControl::Save, "Save", true),
        ],
        &mut form,
    );
    form.end_frame(DialogControl::Field);
}

pub(crate) fn render_config_id_editor(
    frame: &mut Frame,
    area: Rect,
    editor: &ConfigIdEditor,
    surfaces: &mut FrameSurfaces,
) {
    let popup = centered_modal(frame, surfaces, 60, 8, area);
    let inner = popup.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 1,
    });
    if inner.height == 0 {
        clear_dialog_form_geometry(&mut editor.form.borrow_mut());
        return;
    }
    frame.render_widget(
        Paragraph::new(format!(
            "Current {} ID: {}",
            editor.kind.label(),
            editor.old_id
        )),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    let field = Rect::new(inner.x, inner.y.saturating_add(2), inner.width, 1);
    let footer = Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1);
    let mut form = editor.form.borrow_mut();
    form.begin_frame();
    let title = dismissible_modal_title(
        &mut form,
        popup,
        format!("Rename {} ID", editor.kind.label()),
        theme::title(true),
        true,
    );
    frame.render_widget(theme::modal().title(title), popup);
    TextField::render(frame, field, &editor.value, &mut form, DialogControl::Field);
    Dialog::render_actions(
        frame,
        footer,
        &[
            (DialogControl::Cancel, "Cancel", true),
            (DialogControl::Save, "Save", true),
        ],
        &mut form,
    );
    form.end_frame(DialogControl::Field);
}

pub(crate) fn render_target_actions(
    frame: &mut Frame,
    area: Rect,
    dashboard: &DashboardState,
    dialog: &TargetActionsDialog,
    surfaces: &mut FrameSurfaces,
) {
    let rows = dialog
        .target_ids
        .iter()
        .map(|id| {
            let kind = dashboard
                .config
                .targets
                .get(id)
                .map(target_kind_label)
                .unwrap_or("missing");
            Line::from(vec![
                Span::styled(format!("{id:<24} "), theme::title(false)),
                Span::styled(kind, theme::muted()),
            ])
        })
        .collect::<Vec<_>>();
    let list_rows = if rows.is_empty() {
        vec![Line::styled(
            "No targets configured.",
            Style::default().fg(theme::palette().muted),
        )]
    } else {
        rows
    };
    let height = u16::try_from(list_rows.len())
        .unwrap_or(u16::MAX)
        .saturating_add(8)
        .max(12);
    let popup = centered_modal(frame, surfaces, 72, height, area);
    let inner = popup.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 1,
    });
    if inner.height == 0 {
        clear_dialog_form_geometry(&mut dialog.form.borrow_mut());
        return;
    }
    let list_height = u16::try_from(list_rows.len())
        .unwrap_or(u16::MAX)
        .min(inner.height.saturating_sub(4));
    let list_area = Rect::new(inner.x, inner.y, inner.width, list_height.max(1));
    let status_y = list_area.bottom().saturating_add(1);
    if let Some(target_id) = &dialog.testing {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                mj_chat::spinner::compact_span(
                    dashboard.config.spinner,
                    mj_chat::spinner::elapsed_ms(),
                ),
                Span::styled(
                    format!(" Testing {target_id}…"),
                    Style::default().fg(theme::palette().accent),
                ),
                Span::styled(" Alt-X cancels test", theme::muted()),
            ])),
            Rect::new(inner.x, status_y, inner.width, 1),
        );
    } else if let Some((target_id, result)) = &dialog.result {
        frame.render_widget(
            Paragraph::new(match result {
                Ok(()) => format!("{target_id}: ready"),
                Err(error) => format!("{target_id}: {error}"),
            })
            .style(Style::default().fg(if result.is_ok() {
                theme::palette().success
            } else {
                theme::palette().warning
            })),
            Rect::new(inner.x, status_y, inner.width, 1),
        );
    }
    let hint = Rect::new(inner.x, inner.bottom().saturating_sub(2), inner.width, 1);
    frame.render_widget(
        Paragraph::new("Up/Down selects target · Tab selects action · Esc closes")
            .style(Style::default().fg(theme::palette().muted)),
        hint,
    );
    let footer = Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1);
    let mut form = dialog.form.borrow_mut();
    form.begin_frame();
    let title =
        dismissible_modal_title(&mut form, popup, "Target actions", theme::title(true), true);
    frame.render_widget(theme::modal().title(title), popup);
    ChoiceList::render(
        frame,
        list_area,
        &list_rows,
        dialog.target_index,
        &mut form,
        DialogControl::TargetList,
    );
    Dialog::render_actions(
        frame,
        footer,
        &[
            (DialogControl::TargetRename, "Rename", true),
            (DialogControl::TargetTest, "Test", dialog.testing.is_none()),
            (
                DialogControl::TargetSettings,
                "Settings",
                dialog.testing.is_none(),
            ),
        ],
        &mut form,
    );
    form.end_frame(DialogControl::TargetList);
}

fn target_kind_label(target: &mj_core::config::TargetTemplate) -> &'static str {
    match target {
        mj_core::config::TargetTemplate::LocalBare => "local bare",
        mj_core::config::TargetTemplate::LocalPodman { .. } => "local Podman",
        mj_core::config::TargetTemplate::LocalDocker { .. } => "local Docker",
        mj_core::config::TargetTemplate::AppleContainer { .. } => "Apple container",
        mj_core::config::TargetTemplate::AwsEc2 { .. } => "AWS EC2",
        mj_core::config::TargetTemplate::SshBare { .. } => "SSH bare",
        mj_core::config::TargetTemplate::SshPodman { .. } => "SSH Podman",
        mj_core::config::TargetTemplate::SshDocker { .. } => "SSH Docker",
    }
}

pub(crate) fn render_web_dialog(
    frame: &mut Frame,
    area: Rect,
    dialog: &WebDialog,
    surfaces: &mut FrameSurfaces,
) {
    // Text that names the natural body width. The box hugs the QR, and longer
    // URLs wrap beneath it rather than stretching the dialog across the screen.
    const MIN_INNER_WIDTH: usize = 40;

    // The QR is the widest single element, so it decides the box width and only
    // shows when the terminal can hold it plus a border and the footer rows.
    let qr_lines: Vec<&str> = dialog
        .qr
        .as_deref()
        .map(|qr| qr.lines().collect())
        .unwrap_or_default();
    let qr_width = qr_lines.iter().map(|line| line.chars().count()).max();
    // Size against the region a modal may occupy so the QR fit and the box width
    // both respect the screen-edge margin that centering will enforce.
    let inner_area = modal_area(area);
    let max_inner = usize::from(inner_area.width).saturating_sub(2);
    let max_qr_height = usize::from(inner_area.height).saturating_sub(6);
    let show_qr =
        matches!(qr_width, Some(width) if width <= max_inner) && qr_lines.len() <= max_qr_height;

    let mut inner_width = MIN_INNER_WIDTH;
    let mut lines = Vec::new();
    if let Some(process) = &dialog.confirm_stop {
        inner_width = 60;
        lines.push(Line::styled(
            "Stop this Mjolnir server?",
            Style::default().fg(theme::palette().warning),
        ));
        lines.push(Line::raw(format!("{} · PID {}", process.name, process.pid)));
        lines.push(Line::raw(process.executable.display().to_string()));
        lines.push(Line::raw(""));
        lines.push(Line::raw("Other viewers and dashboards using that server will be disconnected. Mjolnir will request a graceful stop, then retry this port."));
    } else if dialog.loading {
        lines.push(Line::styled(
            "Starting web viewer…",
            Style::default().fg(theme::palette().warning),
        ));
    } else if let Some(message) = &dialog.message {
        inner_width = 60;
        lines.extend(message.lines().map(|line| {
            Line::styled(
                line.to_owned(),
                Style::default().fg(theme::palette().warning),
            )
        }));
        if let Some(address) = dialog.failed_address {
            lines.push(Line::raw(format!("Address: {address}")));
            lines.push(Line::raw(""));
            lines.push(Line::raw("Use another port to get connected now. The new port lasts until the daemon restarts."));
            if dialog.port_conflict {
                lines.push(Line::raw(
                    "Inspect the port to see which process is using it.",
                ));
            }
        }
        if dialog.inspecting {
            lines.push(Line::styled(
                "Inspecting listener…",
                Style::default().fg(theme::palette().accent),
            ));
        }
        if let Some(message) = &dialog.inspection_message {
            lines.push(Line::raw(""));
            lines.extend(message.lines().map(|line| Line::raw(line.to_owned())));
        }
        if let Some(process) = dialog.listeners.get(dialog.listener_index) {
            lines.push(Line::raw(""));
            lines.push(Line::raw(format!(
                "Process {} of {}: {} (PID {})",
                dialog.listener_index + 1,
                dialog.listeners.len(),
                process.name,
                process.pid
            )));
            lines.push(Line::raw(process.executable.display().to_string()));
            if let Some(reason) = &process.stop_disabled_reason {
                lines.push(Line::raw(reason.clone()));
            }
        }
    } else {
        if show_qr {
            inner_width = inner_width.max(qr_width.unwrap_or(0));
            lines.extend(
                qr_lines
                    .iter()
                    .map(|line| Line::raw((*line).to_owned()).centered()),
            );
            lines.push(Line::raw(""));
        } else if qr_width.is_some() {
            lines.push(
                Line::styled(
                    "Terminal is too small for a scannable QR code.",
                    Style::default().fg(theme::palette().warning),
                )
                .centered(),
            );
            lines.push(Line::raw(""));
        }
        if let Some(url) = &dialog.viewer_url {
            // The QR encodes this URL; the text is the fallback for hand entry,
            // so it wraps within the box instead of widening it.
            lines.push(Line::from(vec![
                Span::styled("Web: ", Style::default().fg(theme::palette().muted)),
                Span::styled(url.clone(), Style::default().fg(theme::palette().accent)),
            ]));
        }
        if let Some(code) = &dialog.viewer_code {
            lines.push(Line::from(vec![
                Span::styled("Viewer code: ", Style::default().fg(theme::palette().muted)),
                Span::styled(code.clone(), Style::default().fg(theme::palette().accent)),
            ]));
        }
        if let Some(reason) = &dialog.fallback_reason {
            lines.push(Line::styled(
                format!("Local fallback: {reason}"),
                Style::default().fg(theme::palette().warning),
            ));
        }
    }
    lines.push(Line::raw(""));

    let inner_width = inner_width.min(max_inner).max(1);
    let box_width = u16::try_from(inner_width + 2).unwrap_or(u16::MAX);
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let wrapped =
        u16::try_from(paragraph.line_count(box_width.saturating_sub(2))).unwrap_or(u16::MAX);
    let button_rows = dialog.button_rows();
    let footer_height = u16::try_from(button_rows.len()).unwrap_or(u16::MAX);
    let box_height = wrapped
        .saturating_add(2 + footer_height)
        .min(inner_area.height);
    let popup = centered_modal_fixed(frame, surfaces, box_width, box_height, area);
    let inner = popup.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 1,
    });
    if inner.height == 0 {
        clear_dialog_form_geometry(&mut dialog.form.borrow_mut());
        return;
    }
    let body = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner.height.saturating_sub(footer_height),
    );
    frame.render_widget(paragraph, body);
    let mut form = dialog.form.borrow_mut();
    form.begin_frame();
    let title = dismissible_modal_title(&mut form, popup, "Web viewer", theme::title(true), true);
    frame.render_widget(theme::modal().title(title), popup);
    let footer_top = inner.bottom().saturating_sub(footer_height).max(inner.y);
    for (index, buttons) in button_rows.iter().enumerate() {
        let y = footer_top.saturating_add(index as u16);
        let footer = if y < inner.bottom() {
            Rect::new(inner.x, y, inner.width, 1)
        } else {
            Rect::default()
        };
        Dialog::render_actions(frame, footer, buttons, &mut form);
    }
    form.end_frame(dialog.default_control());
}

fn render_qr(data: &str) -> Result<String, String> {
    const QUIET_ZONE: usize = 4;
    let qr = QrCode::with_error_correction_level(data.as_bytes(), EcLevel::L)
        .map_err(|error| format!("encode web login QR: {error}"))?;
    let total = qr.width() + QUIET_ZONE * 2;
    let mut output = String::new();
    for y in (0..total).step_by(2) {
        for x in 0..total {
            let module = |x: usize, y: usize| {
                let Some(x) = x.checked_sub(QUIET_ZONE) else {
                    return false;
                };
                let Some(y) = y.checked_sub(QUIET_ZONE) else {
                    return false;
                };
                x < qr.width() && y < qr.width() && qr[(x, y)] == QrColor::Dark
            };
            output.push(match (module(x, y), module(x, y + 1)) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            });
        }
        output.push('\n');
    }
    Ok(output)
}

pub(crate) fn render_repository_origin(
    frame: &mut Frame,
    area: Rect,
    dialog: &RepositoryOriginDialog,
    surfaces: &mut FrameSurfaces,
) {
    let mut lines = vec![
        Line::raw(format!("Repository: {}", dialog.repository_id)),
        Line::raw(""),
        Line::raw(format!(
            "The configured source does not contain checkpoint base {}.",
            dialog.missing_commit
        )),
        Line::raw(format!("Checkpoint origin: {}", dialog.archived_origin)),
        Line::raw(format!(
            "Configured source checked: {}",
            dialog.configured_origin
        )),
        Line::raw(""),
        Line::raw("Enter a GitHub origin or absolute local path that contains this history:"),
    ];
    if let Some(error) = &dialog.error {
        lines.push(Line::styled(
            error.clone(),
            Style::default().fg(theme::palette().warning),
        ));
    }
    let body_paragraph = Paragraph::new(lines.clone()).wrap(Wrap { trim: false });
    let popup_height = popup_height(&body_paragraph, 76, 14, area);
    let popup = centered_modal(frame, surfaces, 76, popup_height, area);
    let inner = popup.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 1,
    });
    if inner.height == 0 {
        clear_dialog_form_geometry(&mut dialog.form.borrow_mut());
        return;
    }
    let controls_height = 5;
    let text = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner.height.saturating_sub(controls_height),
    );
    frame.render_widget(body_paragraph, text);
    let field_y = inner.y.saturating_add(text.height);
    frame.render_widget(
        Paragraph::new("Source:"),
        Rect::new(inner.x, field_y, 8.min(inner.width), 1),
    );
    let field_x = inner.x.saturating_add(8.min(inner.width));
    let field = Rect::new(field_x, field_y, inner.width.saturating_sub(8), 1);
    let hint_y = inner.bottom().saturating_sub(3);
    frame.render_widget(
        Paragraph::new("Type or paste into Source · Tab moves · Enter checks")
            .style(Style::default().fg(theme::palette().muted)),
        Rect::new(inner.x, hint_y, inner.width, 1),
    );
    let footer = Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1);
    let mut form = dialog.form.borrow_mut();
    form.begin_frame();
    let title = dismissible_modal_title(
        &mut form,
        popup,
        "Repository history is missing",
        theme::title(true),
        true,
    );
    frame.render_widget(theme::modal().title(title), popup);
    PathField::render(
        frame,
        field,
        &dialog.replacement,
        &mut form,
        DialogControl::Field,
    );
    Dialog::render_actions(
        frame,
        footer,
        &[
            (DialogControl::Cancel, "Cancel", true),
            (DialogControl::Primary, "Check origin", true),
        ],
        &mut form,
    );
    form.end_frame(DialogControl::Field);
}

/// Title and body of one confirmation, without its buttons.
///
/// Split out so the wording a dialog shows can be asserted without
/// rendering a frame and reading cells back.
fn confirmation_body(confirmation: &Confirmation) -> (&'static str, Vec<Line<'static>>) {
    match confirmation {
        Confirmation::RepairRepositoryRemotes { repairs, .. } => (
            "Repair Git tracking?",
            repairs
                .iter()
                .flat_map(|repair| {
                    vec![
                        Line::from(repair.path.display().to_string()),
                        Line::from(format!(
                            "Branch {} tracks missing remote {}.",
                            repair.branch, repair.missing_remote
                        )),
                        Line::from(format!(
                            "Set its tracking remote to {}.",
                            repair.replacement_remote
                        )),
                        Line::from(format!("Fetch: {}", repair.fetch_url)),
                        Line::from(format!("Push: {}", repair.push_urls.join(", "))),
                        Line::from(""),
                    ]
                })
                .collect(),
        ),
        Confirmation::ConfigurationRepair { error, .. } => (
            " Configuration repair ",
            vec![
                Line::raw(error.clone()),
                Line::raw(""),
                Line::raw(
                    "Open settings to restore the named entries. The session and its history are retained.",
                ),
                Line::raw("PgUp/PgDn scroll the full details. Esc dismisses."),
            ],
        ),
        Confirmation::LaunchFailed { error, retry, .. } => {
            let mut lines = vec![
                Line::raw("The session could not start. This message stays until you dismiss it."),
                Line::raw(if retry.is_some() {
                    "Resolve the problem below, then Retry launch with the same settings."
                } else {
                    "Dismiss, resolve the problem below, then select the failed session to retry or remove it."
                }),
                Line::raw("PgUp/PgDn scroll the full details. Esc dismisses."),
                Line::raw(""),
            ];
            if error.contains("Operation not permitted") && error.contains("chmod") {
                lines.push(Line::raw("The container user cannot change the uploaded worker's permissions. If using a custom image, try the standard Mjolnir agent image. Include the diagnostic below when reporting this problem."));
                lines.push(Line::raw(""));
            }
            lines.extend(error.lines().map(|line| Line::raw(line.to_owned())));
            (" Launch failed ", lines)
        }
        Confirmation::Dismiss {
            intent: DismissalIntent::DiscardSetup,
            ..
        } => (
            " Discard Settings changes? ",
            vec![
                Line::raw("Settings has unsaved changes."),
                Line::raw("Keep editing to preserve them, or discard the draft."),
            ],
        ),
        Confirmation::Dismiss {
            intent: DismissalIntent::CancelImport,
            ..
        } => (
            " Cancel import? ",
            vec![
                Line::raw("The active import has not finished."),
                Line::raw("Keep importing, or cancel the operation?"),
            ],
        ),
        Confirmation::ConvertRawCheckout { preview, .. } => {
            let mut lines = vec![Line::raw(preview.summary_line()), Line::raw("")];
            for warning in preview.warning_lines() {
                lines.push(Line::styled(
                    warning,
                    Style::default().fg(theme::palette().warning),
                ));
            }
            (" Move this checkout into the target? ", lines)
        }
        Confirmation::DestroyStopped { session_id, .. } => (
            " Permanently destroy stopped session? ",
            vec![
                Line::raw(format!("Session: {session_id}")),
                Line::raw(""),
                Line::raw(
                    "Mjolnir will permanently destroy the recovery archive and session record.",
                ),
                Line::raw(
                    "Any Mjolnir-managed worktree and generated branch will also be removed.",
                ),
            ],
        ),
        Confirmation::CloseFailed { session_id, error } => (
            " Stop could not complete ",
            vec![
                Line::raw(format!("Session: {session_id}")),
                Line::raw(""),
                Line::styled(
                    format!("Stop failed: {error}"),
                    Style::default().fg(theme::palette().warning),
                ),
            ],
        ),
        Confirmation::StopWithSubagents { session_id, count } => (
            " Stop parent and sub-agents? ",
            vec![
                Line::raw(format!("Session: {session_id}")),
                Line::raw(""),
                Line::styled(
                    format!("This session has {count} active sub-agent(s)."),
                    Style::default().fg(theme::palette().warning),
                ),
                Line::raw("Mjolnir will stop the children first, then save and stop the parent."),
            ],
        ),
        Confirmation::RecoverFailed {
            session_id,
            error,
            recoverable,
        } => {
            let mut lines = vec![Line::raw(format!("Session: {session_id}")), Line::raw("")];
            match error {
                Some(error) => lines.push(Line::styled(
                    format!("Failed: {error}"),
                    Style::default().fg(theme::palette().warning),
                )),
                None => lines.push(Line::raw("This session failed without a recorded error.")),
            }
            lines.push(Line::raw(""));
            if *recoverable {
                lines.push(Line::raw(
                    "Recover restores the session onto a fresh target from its recovery copy.",
                ));
                lines.push(Line::raw(
                    "Its transcript is readable either way; opening it changes nothing.",
                ));
            } else {
                lines.push(Line::raw(
                    "There is no verified recovery copy, so this session cannot be resumed.",
                ));
                lines.push(Line::raw("Its transcript is still readable."));
            }
            (" Session failed ", lines)
        }
        Confirmation::RecoverMove { operation } => {
            let mut lines = vec![
                Line::raw(format!("Session: {}", operation.selection.session_id)),
                Line::raw(""),
                Line::styled(
                    format!(
                        "Move {} at destination {} / {}.",
                        match operation.phase {
                            MovePhase::Failed => "failed",
                            MovePhase::Cancelled => "was cancelled",
                            _ => "needs recovery",
                        },
                        operation
                            .selection
                            .profile_id
                            .as_deref()
                            .unwrap_or("current profile"),
                        operation
                            .selection
                            .target_template_id
                            .as_deref()
                            .unwrap_or("current target")
                    ),
                    Style::default().fg(theme::palette().warning),
                ),
            ];
            if let Some(error) = &operation.error {
                lines.push(Line::styled(
                    format!("Error: {error}"),
                    Style::default().fg(theme::palette().warning),
                ));
            }
            lines.push(Line::raw(""));
            if operation.queue_admission_started && !operation.queue_admission_finished {
                lines.push(Line::raw(
                    "Some queued work may already have been accepted; only retry on this exact destination.",
                ));
                lines.push(Line::raw(
                    "Resume with previous settings is unavailable until queue admission finishes.",
                ));
            } else {
                lines.push(Line::raw(
                    "Retry move keeps the failed destination and queue choice.",
                ));
                lines.push(Line::raw(
                    "Resume with previous settings restores the source configuration instead.",
                ));
            }
            if operation.queue == ResumeQueueDisposition::Start {
                lines.push(Line::raw("Queued work was selected to run after the move."));
            } else {
                lines.push(Line::raw("Queued work was selected for discard."));
            }
            (" Move recovery ", lines)
        }
        Confirmation::ForceDestroy { session_id } => (
            " Delete session? ",
            vec![
                Line::raw(format!("Session: {session_id}")),
                Line::raw(""),
                Line::raw("Delete this session, its worktree, and its recovery archive?"),
                Line::raw("Y: Yes    N / Esc: No"),
            ],
        ),
    }
}

pub(crate) fn render_confirmation(
    frame: &mut Frame,
    area: Rect,
    dialog: &ConfirmDialog,
    surfaces: &mut FrameSurfaces,
) {
    let confirmation = &dialog.confirmation;
    // Minimum height per dialog; `popup_height` grows it to fit wrapped content.
    let nominal: u16 = match confirmation {
        Confirmation::ConfigurationRepair { .. }
        | Confirmation::LaunchFailed { .. }
        | Confirmation::RepairRepositoryRemotes { .. } => 16,
        Confirmation::Dismiss { .. } => 8,
        Confirmation::ConvertRawCheckout { .. } => 16,
        Confirmation::CloseFailed { .. } => 12,
        Confirmation::StopWithSubagents { .. } => 10,
        Confirmation::DestroyStopped { .. } => 10,
        Confirmation::RecoverFailed { .. } => 12,
        Confirmation::RecoverMove { .. } => 14,
        Confirmation::ForceDestroy { .. } => 11,
    };
    let (title, mut lines) = confirmation_body(confirmation);
    let buttons = confirmation_buttons(confirmation);
    lines.push(Line::raw(""));
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let extra = 1;
    let height = popup_height(&paragraph, 72, nominal.saturating_add(extra), area);
    let popup = centered_modal(frame, surfaces, 72, height, area);
    let inner = popup.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 1,
    });
    if inner.height == 0 {
        clear_dialog_form_geometry(&mut dialog.form.borrow_mut());
        return;
    }
    let controls_height = 1;
    let body = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner.height.saturating_sub(controls_height),
    );
    let max_scroll = u16::try_from(paragraph.line_count(body.width.max(1)))
        .unwrap_or(u16::MAX)
        .saturating_sub(body.height);
    dialog.max_scroll.set(max_scroll);
    frame.render_widget(paragraph.scroll((dialog.scroll.min(max_scroll), 0)), body);
    let mut form = dialog.form.borrow_mut();
    form.begin_frame();
    let title_line = dismissible_modal_title(
        &mut form,
        popup,
        title.trim(),
        Style::default()
            .fg(theme::palette().error)
            .add_modifier(Modifier::BOLD),
        true,
    );
    frame.render_widget(
        theme::modal()
            .border_style(Style::default().fg(theme::palette().error))
            .title_style(
                Style::default()
                    .fg(theme::palette().error)
                    .add_modifier(Modifier::BOLD),
            )
            .title(title_line),
        popup,
    );
    let footer = Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1);
    Dialog::render_actions(
        frame,
        footer,
        &buttons
            .iter()
            .enumerate()
            .map(|(index, label)| (DialogControl::ConfirmButton(index), *label, true))
            .collect::<Vec<_>>(),
        &mut form,
    );
    form.end_frame(DialogControl::ConfirmButton(initial_confirmation_button(
        confirmation,
        buttons,
    )));
}

impl DashboardState {
    pub fn apply_web_access(&mut self, access: WebViewerAccess) {
        let Mode::Web(current) = &mut self.mode else {
            return;
        };
        if matches!(access, WebViewerAccess::Starting) {
            let changed = !current.loading;
            current.loading = true;
            current.reset_form();
            if changed {
                self.mark_render_changed();
            }
            return;
        }
        let mut dialog = WebDialog::loading();
        dialog.loading = false;
        match access {
            WebViewerAccess::Starting => unreachable!(),
            WebViewerAccess::Ready {
                viewer_url,
                viewer_code,
                qr_login_url,
                fallback_reason,
            } => {
                dialog.viewer_url = Some(viewer_url);
                dialog.viewer_code = Some(viewer_code);
                dialog.fallback_reason = fallback_reason;
                if let Some(url) = qr_login_url {
                    match render_qr(&url) {
                        Ok(qr) => dialog.qr = Some(qr),
                        Err(error) => dialog.fallback_reason = Some(error),
                    }
                }
            }
            WebViewerAccess::Failed {
                address,
                message,
                port_conflict,
            } => {
                dialog.failed_address = Some(address);
                dialog.port_conflict = port_conflict;
                dialog.message = Some(message);
            }
            WebViewerAccess::Unavailable(message) => dialog.message = Some(message),
        }
        dialog.reset_form();
        let changed = current.loading != dialog.loading
            || current.viewer_url != dialog.viewer_url
            || current.viewer_code != dialog.viewer_code
            || current.fallback_reason != dialog.fallback_reason
            || current.message != dialog.message
            || current.qr != dialog.qr
            || current.failed_address != dialog.failed_address
            || current.port_conflict != dialog.port_conflict
            || current.inspecting != dialog.inspecting
            || current.listeners != dialog.listeners
            || current.listener_index != dialog.listener_index
            || current.inspection_message != dialog.inspection_message
            || current.confirm_stop != dialog.confirm_stop;
        self.mode = Mode::Web(dialog);
        if changed {
            self.mark_render_changed();
        }
    }

    pub fn apply_web_listeners(&mut self, result: Result<Vec<WebListenerProcess>, String>) {
        let Mode::Web(dialog) = &mut self.mode else {
            return;
        };
        let old_inspecting = dialog.inspecting;
        let old_listeners = dialog.listeners.clone();
        let old_listener_index = dialog.listener_index;
        let old_message = dialog.inspection_message.clone();
        dialog.inspecting = false;
        match result {
            Ok(processes) => {
                dialog.inspection_message = processes.is_empty().then(|| "No visible listener was found. It may have exited, or your account may not have permission to inspect it. Retry or use another port.".into());
                dialog.listeners = processes;
                dialog.listener_index = 0;
            }
            Err(error) => dialog.inspection_message = Some(error),
        }
        dialog.reset_form();
        if old_inspecting != dialog.inspecting
            || old_listeners != dialog.listeners
            || old_listener_index != dialog.listener_index
            || old_message != dialog.inspection_message
        {
            self.mark_render_changed();
        }
    }

    pub fn apply_web_error(&mut self, error: String) {
        let Mode::Web(dialog) = &mut self.mode else {
            return;
        };
        let old = (
            dialog.loading,
            dialog.confirm_stop.clone(),
            dialog.inspection_message.clone(),
            dialog.message.clone(),
        );
        dialog.loading = false;
        dialog.confirm_stop = None;
        dialog.inspection_message = Some(error.clone());
        if dialog.failed_address.is_none() {
            dialog.message = Some(error);
        }
        dialog.reset_form();
        let new = (
            dialog.loading,
            dialog.confirm_stop.clone(),
            dialog.inspection_message.clone(),
            dialog.message.clone(),
        );
        if old != new {
            self.mark_render_changed();
        }
    }

    pub(crate) fn handle_web_event(
        &mut self,
        event: Event,
        mut dialog: WebDialog,
    ) -> DashboardAction {
        let result = dialog.form.get_mut().handle(&event);
        crate::record_form_outcome_cells(
            &self.last_event_outcome,
            &self.render_changed,
            &self.render_change_revision,
            &result,
        );
        let interaction = result.action;
        let mut action = DashboardAction::None;
        match interaction {
            Some(Interaction::Cancel) if dialog.confirm_stop.is_some() => {
                dialog.confirm_stop = None;
                dialog.reset_form();
            }
            Some(Interaction::Cancel) => {
                self.cancel_modal();
                return DashboardAction::CancelWebAccess;
            }
            Some(Interaction::Activate(DialogControl::WebRetry)) if !dialog.loading => {
                action = DashboardAction::RecoverWebViewer(WebViewerRecovery::Retry);
            }
            Some(Interaction::Activate(DialogControl::WebAnotherPort)) if !dialog.loading => {
                action = DashboardAction::RecoverWebViewer(WebViewerRecovery::AnotherPort);
            }
            Some(Interaction::Activate(DialogControl::WebInspect)) if !dialog.inspecting => {
                dialog.inspecting = true;
                dialog.inspection_message = None;
                dialog.listeners.clear();
                dialog.reset_form();
                action = DashboardAction::InspectWebListener;
            }
            Some(Interaction::Activate(DialogControl::WebNextProcess))
                if !dialog.listeners.is_empty() =>
            {
                dialog.listener_index = (dialog.listener_index + 1) % dialog.listeners.len();
            }
            Some(Interaction::Activate(DialogControl::WebStop)) => {
                if let Some(process) = dialog.listeners.get(dialog.listener_index)
                    && process.stop_disabled_reason.is_none()
                {
                    dialog.confirm_stop = Some(process.clone());
                    dialog.reset_form();
                }
            }
            Some(Interaction::Activate(DialogControl::WebCancelStop)) => {
                dialog.confirm_stop = None;
                dialog.reset_form();
            }
            Some(Interaction::Activate(DialogControl::WebConfirmStop)) => {
                if let Some(process) = dialog.confirm_stop.take() {
                    action =
                        DashboardAction::RecoverWebViewer(WebViewerRecovery::StopAndRetry(process));
                }
            }
            _ => {}
        }
        if matches!(action, DashboardAction::RecoverWebViewer(_)) {
            dialog.loading = true;
            dialog.inspection_message = None;
            dialog.reset_form();
        }
        self.mode = Mode::Web(dialog);
        action
    }

    pub(crate) fn begin_profile_rename(&mut self) {
        let Some(old_id) = self
            .config
            .enabled_profiles()
            .nth(self.quota_index)
            .map(|(id, _)| id.to_owned())
        else {
            self.notices.set("No profile is selected.");
            return;
        };
        self.mode = Mode::ConfigId(ConfigIdEditor {
            kind: ConfigEntryKind::Profile,
            return_to: None,
            value: TextInput::from_value(old_id.clone()).with_max_chars(64),
            old_id,
            form: dialog_form(
                &[
                    DialogControl::Field,
                    DialogControl::Cancel,
                    DialogControl::Save,
                ],
                DialogControl::Field,
            ),
        });
        self.mark_render_changed();
    }

    pub(crate) fn begin_target_actions(&mut self) {
        let preferred = self
            .capacity_details
            .values()
            .nth(self.capacity_index)
            .and_then(|detail| detail.target.target_ids.first())
            .cloned();
        let target_ids = self.config.targets.keys().cloned().collect::<Vec<_>>();
        let target_index = preferred
            .as_ref()
            .and_then(|id| target_ids.iter().position(|candidate| candidate == id))
            .unwrap_or(0);
        let target_count = target_ids.len();
        self.mode = Mode::TargetActions(TargetActionsDialog {
            target_ids,
            target_index,
            form: target_actions_form(target_count, target_index, DialogControl::TargetList),
            testing: None,
            result: None,
        });
        self.mark_render_changed();
    }

    pub(crate) fn handle_target_actions_event(
        &mut self,
        event: Event,
        mut dialog: TargetActionsDialog,
    ) -> DashboardAction {
        // Alt-X is the surface's one cancel chord. The controller's chord
        // pre-filter deliberately leaves it alone while a dialog is open, so
        // here it cancels the test this dialog is running.
        if let Event::Key(key) = &event
            && dialog.testing.is_some()
            && key.modifiers.contains(KeyModifiers::ALT)
            && key.code == KeyCode::Char('x')
        {
            dialog.testing = None;
            dialog.result = Some(("Target test".into(), Err("cancelled".into())));
            sync_target_actions_form(&mut dialog);
            self.mode = Mode::TargetActions(dialog);
            self.mark_render_changed();
            return DashboardAction::CancelTargetTest;
        }
        // Preserve the convenient Up/Down target selection from the old
        // surface while letting the form own the selection metadata.
        if let Event::Key(key) = &event
            && matches!(key.kind, KeyEventKind::Press)
            && matches!(
                key.code,
                KeyCode::Up | KeyCode::Down | KeyCode::Char('j' | 'k')
            )
        {
            let form = dialog.form.get_mut();
            if !form.is_focused(DialogControl::TargetList) {
                form.focus(DialogControl::TargetList);
                self.mark_render_changed();
            }
        }
        let result = dialog.form.get_mut().handle(&event);
        crate::record_form_outcome_cells(
            &self.last_event_outcome,
            &self.render_changed,
            &self.render_change_revision,
            &result,
        );
        let interaction = result.action;
        match interaction {
            Some(Interaction::Cancel) => {
                let cancel_test = dialog.testing.is_some();
                self.cancel_modal();
                if cancel_test {
                    return DashboardAction::CancelTargetTest;
                }
            }
            Some(Interaction::Select(DialogControl::TargetList, index)) => {
                dialog.target_index = index.min(dialog.target_ids.len().saturating_sub(1));
                self.mode = Mode::TargetActions(dialog);
            }
            Some(Interaction::Activate(control)) => {
                let Some(target_id) = dialog.target_ids.get(dialog.target_index).cloned() else {
                    self.cancel_modal();
                    return DashboardAction::None;
                };
                match control {
                    DialogControl::TargetSettings if dialog.testing.is_none() => {
                        self.begin_settings_section("targets", Some(&target_id));
                        return DashboardAction::None;
                    }
                    DialogControl::TargetRename => {
                        self.mode = Mode::ConfigId(ConfigIdEditor {
                            kind: ConfigEntryKind::Target,
                            return_to: Some(Box::new(dialog)),
                            value: TextInput::from_value(target_id.clone()).with_max_chars(64),
                            old_id: target_id,
                            form: dialog_form(
                                &[
                                    DialogControl::Field,
                                    DialogControl::Cancel,
                                    DialogControl::Save,
                                ],
                                DialogControl::Field,
                            ),
                        });
                        self.mark_render_changed();
                        return DashboardAction::None;
                    }
                    DialogControl::TargetTest if dialog.testing.is_none() => {
                        dialog.testing = Some(target_id.clone());
                        dialog.result = None;
                        sync_target_actions_form(&mut dialog);
                        self.mode = Mode::TargetActions(dialog);
                        self.mark_render_changed();
                        return DashboardAction::TestTarget { target_id };
                    }
                    _ => {}
                }
                self.mode = Mode::TargetActions(dialog);
            }
            _ => self.mode = Mode::TargetActions(dialog),
        }
        DashboardAction::None
    }

    pub(crate) fn handle_config_id_event(
        &mut self,
        event: Event,
        mut editor: ConfigIdEditor,
    ) -> DashboardAction {
        let result = editor.form.get_mut().handle(&event);
        crate::record_form_outcome_cells(
            &self.last_event_outcome,
            &self.render_changed,
            &self.render_change_revision,
            &result,
        );
        let interaction = result.action;
        match interaction {
            Some(Interaction::Cancel | Interaction::Activate(DialogControl::Cancel)) => {
                if let Some(parent) = editor.return_to.take() {
                    self.mode = Mode::TargetActions(*parent);
                    self.mark_render_changed();
                } else {
                    self.cancel_modal();
                }
            }
            Some(Interaction::Edit(DialogControl::Field, edit)) => {
                if TextField::apply(&mut editor.value, edit) == Outcome::Changed {
                    crate::mark_render_changed_cells(
                        &self.render_changed,
                        &self.render_change_revision,
                    );
                }
                self.mode = Mode::ConfigId(editor);
            }
            Some(Interaction::Activate(DialogControl::Field | DialogControl::Save)) => {
                if editor.value.trim().is_empty() {
                    self.notices.set("Configuration ID cannot be empty.");
                    crate::mark_render_changed_cells(
                        &self.render_changed,
                        &self.render_change_revision,
                    );
                    self.mode = Mode::ConfigId(editor);
                } else {
                    self.cancel_modal();
                    match editor.kind {
                        ConfigEntryKind::Profile => {
                            return DashboardAction::RenameProfile {
                                old_id: editor.old_id,
                                new_id: editor.value.into_value(),
                            };
                        }
                        ConfigEntryKind::Target => {
                            return DashboardAction::RenameTarget {
                                old_id: editor.old_id,
                                new_id: editor.value.into_value(),
                            };
                        }
                    }
                }
            }
            _ => self.mode = Mode::ConfigId(editor),
        }
        DashboardAction::None
    }

    pub fn apply_target_test(&mut self, target_id: String, result: Result<(), String>) {
        if let Mode::TargetActions(dialog) = &mut self.mode
            && dialog.testing.as_deref() == Some(&target_id)
        {
            let old = (dialog.testing.clone(), dialog.result.clone());
            dialog.testing = None;
            dialog.result = Some((target_id, result));
            sync_target_actions_form(dialog);
            if old != (dialog.testing.clone(), dialog.result.clone()) {
                self.mark_render_changed();
            }
        }
    }

    pub fn show_repository_origin_dialog(
        &mut self,
        session_id: String,
        repository_id: String,
        missing_commit: String,
        archived_origin: String,
        configured_origin: String,
        launch: DashboardAction,
    ) {
        self.mode = Mode::RepositoryOrigin(RepositoryOriginDialog {
            session_id,
            repository_id,
            missing_commit,
            archived_origin,
            replacement: PathInput::new(),
            configured_origin,
            error: None,
            form: dialog_form(
                &[
                    DialogControl::Field,
                    DialogControl::Cancel,
                    DialogControl::Primary,
                ],
                DialogControl::Field,
            ),
            launch: Box::new(launch),
        });
        self.mark_render_changed();
    }

    pub fn apply_repository_origin_failure(&mut self, repository_id: &str, error: String) {
        if let Mode::RepositoryOrigin(dialog) = &mut self.mode
            && dialog.repository_id == repository_id
        {
            let old_error = dialog.error.clone();
            let old_focus = dialog.form.borrow().focused();
            dialog.error = Some(error);
            dialog.form.get_mut().focus(DialogControl::Field);
            if old_error != dialog.error || old_focus != dialog.form.borrow().focused() {
                self.mark_render_changed();
            }
        }
    }

    pub fn finish_resume_repository_preflight(&mut self) {
        self.cancel_modal();
    }

    pub(crate) fn handle_repository_origin_event(
        &mut self,
        event: Event,
        mut dialog: RepositoryOriginDialog,
    ) -> DashboardAction {
        let result = dialog.form.get_mut().handle(&event);
        crate::record_form_outcome_cells(
            &self.last_event_outcome,
            &self.render_changed,
            &self.render_change_revision,
            &result,
        );
        let interaction = result.action;
        match interaction {
            Some(Interaction::Cancel) | Some(Interaction::Activate(DialogControl::Cancel)) => {
                self.cancel_modal();
            }
            Some(Interaction::Edit(DialogControl::Field, edit)) => {
                if PathField::apply(&mut dialog.replacement, edit) == Outcome::Changed {
                    dialog.error = None;
                    crate::mark_render_changed_cells(
                        &self.render_changed,
                        &self.render_change_revision,
                    );
                }
                self.mode = Mode::RepositoryOrigin(dialog);
            }
            Some(Interaction::Activate(DialogControl::Field | DialogControl::Primary)) => {
                if dialog.replacement.trim().is_empty() {
                    dialog.error = Some("Enter the repository's new origin.".into());
                    dialog.form.get_mut().focus(DialogControl::Field);
                    crate::mark_render_changed_cells(
                        &self.render_changed,
                        &self.render_change_revision,
                    );
                    self.mode = Mode::RepositoryOrigin(dialog);
                } else {
                    let action = DashboardAction::ReplaceResumeRepositoryOrigin {
                        session_id: dialog.session_id.clone(),
                        repository_id: dialog.repository_id.clone(),
                        replacement: dialog.replacement.to_string(),
                        launch: dialog.launch.clone(),
                    };
                    self.mode = Mode::RepositoryOrigin(dialog);
                    return action;
                }
            }
            _ => self.mode = Mode::RepositoryOrigin(dialog),
        }
        DashboardAction::None
    }

    /// Keep launch errors independent of the transient shared status line.
    pub fn show_launch_failure(
        &mut self,
        error: impl Into<String>,
        retry: Option<DashboardAction>,
    ) {
        let previous = Box::new(self.mode.clone());
        self.mode = Mode::Confirm(ConfirmDialog::new(Confirmation::LaunchFailed {
            error: error.into(),
            retry: retry.map(Box::new),
            previous,
        }));
        self.mark_render_changed();
    }

    pub fn show_remote_repair_confirmation(
        &mut self,
        bundle_id: String,
        repairs: Vec<mj_core::local_git::LocalRemoteRepair>,
        retry: DashboardAction,
    ) {
        let previous = Box::new(self.mode.clone());
        let action = Box::new(DashboardAction::RepairRepositoryRemotes {
            bundle_id,
            repairs: repairs.clone(),
            retry: Box::new(retry),
        });
        self.mode = Mode::Confirm(ConfirmDialog::new(Confirmation::RepairRepositoryRemotes {
            action,
            repairs,
            previous,
        }));
        self.mark_render_changed();
    }

    /// Show the recovery choices after a checkpointed close could not finish.
    pub fn show_close_failure(&mut self, session_id: String, error: impl Into<String>) {
        self.mode = Mode::Confirm(ConfirmDialog::new(Confirmation::CloseFailed {
            session_id,
            error: error.into(),
        }));
        self.mark_render_changed();
    }

    pub fn show_import_progress(&mut self, session_title: String) {
        self.mode = Mode::Importing(ImportProgress {
            session_title,
            step: 1,
            total: None,
            message: "Locating native session…".into(),
            last_updated: Instant::now(),
            form: dialog_form(&[DialogControl::Cancel], DialogControl::Cancel),
        });
        self.mark_render_changed();
    }

    pub fn update_import_progress(&mut self, step: usize, total: Option<usize>, message: String) {
        let Some(progress) = import_progress_mut(&mut self.mode) else {
            return;
        };
        let previous_status = import_progress_status(progress);
        let changed =
            progress.step != step || progress.total != total || progress.message != message;
        progress.step = step;
        progress.total = total;
        progress.message = message;
        progress.last_updated = Instant::now();
        if changed || previous_status != import_progress_status(progress) {
            self.mark_render_changed();
        }
    }

    pub fn show_import_bundle_confirmation(
        &mut self,
        dirty_git_roots: Vec<String>,
        omitted_non_git_dirs: Vec<String>,
        scratch_git_roots: Vec<String>,
        has_untracked_files: bool,
        managed_worktree: mj_core::state::ManagedWorktreeOptions,
    ) {
        let mut form = if has_untracked_files {
            dialog_form(
                &[
                    DialogControl::ImportIgnore,
                    DialogControl::ImportCancel,
                    DialogControl::ImportContinue,
                ],
                DialogControl::ImportContinue,
            )
        } else {
            dialog_form(
                &[DialogControl::ImportCancel, DialogControl::ImportContinue],
                DialogControl::ImportContinue,
            )
        };
        if managed_worktree.available {
            form.get_mut().declare_with_enabled(
                DialogControl::ImportManagedWorktree,
                ControlKind::Checkbox,
                true,
            );
        }
        self.mode = Mode::ConfirmImportBundle(ImportBundleConfirmation {
            managed_worktree,
            create_managed_worktree: managed_worktree.default_create,
            dirty_git_roots,
            omitted_non_git_dirs,
            scratch_git_roots,
            has_untracked_files,
            ignore_untracked: has_untracked_files,
            form,
        });
        self.mark_render_changed();
    }

    /// Ask before a resume moves a local checkout into an isolated workspace.
    /// Cancelling returns to the wizard the preflight was started from.
    pub fn show_raw_conversion_confirmation(
        &mut self,
        launch: DashboardAction,
        receipt: mj_core::state::ResumeRepositorySourceReceipt,
        preview: mj_core::state::RawConversionPreview,
    ) {
        let previous = Box::new(self.mode.clone());
        self.mode = Mode::Confirm(ConfirmDialog::new(Confirmation::ConvertRawCheckout {
            launch: Box::new(launch),
            receipt: Box::new(receipt),
            preview: Box::new(preview),
            previous,
        }));
        self.mark_render_changed();
    }

    pub fn finish_import(&mut self) {
        self.cancel_modal();
    }

    pub(crate) fn handle_import_progress_event(
        &mut self,
        event: Event,
        mut progress: ImportProgress,
    ) -> DashboardAction {
        let result = progress.form.get_mut().handle(&event);
        crate::record_form_outcome_cells(
            &self.last_event_outcome,
            &self.render_changed,
            &self.render_change_revision,
            &result,
        );
        let interaction = result.action;
        match interaction {
            Some(Interaction::Cancel) | Some(Interaction::Activate(DialogControl::Cancel)) => {
                self.mode = Mode::Confirm(ConfirmDialog::new(Confirmation::Dismiss {
                    mode: Box::new(Mode::Importing(progress)),
                    intent: DismissalIntent::CancelImport,
                }));
                self.mark_render_changed();
                DashboardAction::None
            }
            _ => {
                self.mode = Mode::Importing(progress);
                DashboardAction::None
            }
        }
    }

    pub(crate) fn begin_rename(&mut self) {
        let Some(session) = self.selected_session() else {
            return;
        };
        self.mode = Mode::Rename(RenameEditor {
            session_id: session.id.clone(),
            title: TextInput::from_value(
                session
                    .session_title_override
                    .as_ref()
                    .or(session.acp_session_title.as_ref())
                    .cloned()
                    .unwrap_or_default(),
            )
            .with_max_chars(64),
            form: dialog_form(
                &[
                    DialogControl::Field,
                    DialogControl::Cancel,
                    DialogControl::Save,
                ],
                DialogControl::Field,
            ),
        });
        self.mark_render_changed();
    }

    pub(crate) fn handle_rename_event(
        &mut self,
        event: Event,
        mut editor: RenameEditor,
    ) -> DashboardAction {
        let result = editor.form.get_mut().handle(&event);
        crate::record_form_outcome_cells(
            &self.last_event_outcome,
            &self.render_changed,
            &self.render_change_revision,
            &result,
        );
        let interaction = result.action;
        match interaction {
            Some(Interaction::Cancel) | Some(Interaction::Activate(DialogControl::Cancel)) => {
                self.cancel_modal();
            }
            Some(Interaction::Edit(DialogControl::Field, edit)) => {
                if TextField::apply(&mut editor.title, edit) == Outcome::Changed {
                    crate::mark_render_changed_cells(
                        &self.render_changed,
                        &self.render_change_revision,
                    );
                }
                self.mode = Mode::Rename(editor);
            }
            Some(Interaction::Activate(DialogControl::Field | DialogControl::Save)) => {
                if editor.title.trim().is_empty() {
                    self.notices.set("Session name cannot be empty.");
                    editor.form.get_mut().focus(DialogControl::Field);
                    crate::mark_render_changed_cells(
                        &self.render_changed,
                        &self.render_change_revision,
                    );
                    self.mode = Mode::Rename(editor);
                } else {
                    self.cancel_modal();
                    return DashboardAction::RenameSession {
                        session_id: editor.session_id,
                        title: editor.title.into_value(),
                    };
                }
            }
            _ => self.mode = Mode::Rename(editor),
        }
        DashboardAction::None
    }

    pub(crate) fn handle_import_bundle_event(
        &mut self,
        event: Event,
        mut confirmation: ImportBundleConfirmation,
    ) -> DashboardAction {
        let result = confirmation.form.get_mut().handle(&event);
        crate::record_form_outcome_cells(
            &self.last_event_outcome,
            &self.render_changed,
            &self.render_change_revision,
            &result,
        );
        let interaction = result.action;
        match interaction {
            Some(Interaction::Cancel)
            | Some(Interaction::Activate(DialogControl::ImportCancel)) => {
                return DashboardAction::ConfirmImportBundle {
                    create_managed_worktree: None,
                    accepted: false,
                    include_untracked: false,
                };
            }
            Some(Interaction::Toggle(DialogControl::ImportManagedWorktree)) => {
                if confirmation.managed_worktree.available {
                    confirmation.create_managed_worktree = !confirmation.create_managed_worktree;
                }
                self.mode = Mode::ConfirmImportBundle(confirmation);
            }
            Some(Interaction::Toggle(DialogControl::ImportIgnore)) => {
                confirmation.ignore_untracked = !confirmation.ignore_untracked;
                self.mode = Mode::ConfirmImportBundle(confirmation);
            }
            Some(Interaction::Activate(DialogControl::ImportContinue)) => {
                return DashboardAction::ConfirmImportBundle {
                    create_managed_worktree: Some(
                        confirmation.managed_worktree.available
                            && confirmation.create_managed_worktree,
                    ),
                    accepted: true,
                    include_untracked: !confirmation.ignore_untracked,
                };
            }
            _ => self.mode = Mode::ConfirmImportBundle(confirmation),
        }
        DashboardAction::None
    }

    pub(crate) fn handle_confirmation_event(
        &mut self,
        event: Event,
        mut dialog: ConfirmDialog,
    ) -> DashboardAction {
        if matches!(
            dialog.confirmation,
            Confirmation::ForceDestroy { .. } | Confirmation::DestroyStopped { .. }
        ) && let Event::Key(key) = &event
            && key.kind != crossterm::event::KeyEventKind::Release
            && !key.modifiers.intersects(
                crossterm::event::KeyModifiers::CONTROL | crossterm::event::KeyModifiers::ALT,
            )
        {
            match key.code {
                crossterm::event::KeyCode::Char('y' | 'Y') => {
                    return self.activate_confirmation_button(dialog.confirmation, 1);
                }
                crossterm::event::KeyCode::Char('n' | 'N') => {
                    return self.activate_confirmation_button(dialog.confirmation, 0);
                }
                _ => {}
            }
        }
        if matches!(
            dialog.confirmation,
            Confirmation::ConfigurationRepair { .. }
                | Confirmation::LaunchFailed { .. }
                | Confirmation::RepairRepositoryRemotes { .. }
                | Confirmation::ConvertRawCheckout { .. }
        ) && let Event::Key(key) = &event
            && key.kind != crossterm::event::KeyEventKind::Release
        {
            match key.code {
                crossterm::event::KeyCode::PageDown => {
                    dialog.scroll = dialog.scroll.saturating_add(5).min(dialog.max_scroll.get())
                }
                crossterm::event::KeyCode::PageUp => {
                    dialog.scroll = dialog.scroll.saturating_sub(5)
                }
                crossterm::event::KeyCode::Home => dialog.scroll = 0,
                _ => return self.handle_confirmation_controls(dialog, event),
            }
            self.mode = Mode::Confirm(dialog);
            self.mark_render_changed();
            return DashboardAction::None;
        }
        self.handle_confirmation_controls(dialog, event)
    }

    fn handle_confirmation_controls(
        &mut self,
        mut dialog: ConfirmDialog,
        event: Event,
    ) -> DashboardAction {
        let result = dialog.form.get_mut().handle(&event);
        crate::record_form_outcome_cells(
            &self.last_event_outcome,
            &self.render_changed,
            &self.render_change_revision,
            &result,
        );
        let interaction = result.action;
        match interaction {
            Some(Interaction::Cancel) => match dialog.confirmation {
                Confirmation::Dismiss { mode, .. }
                | Confirmation::RepairRepositoryRemotes { previous: mode, .. }
                | Confirmation::ConfigurationRepair { previous: mode, .. }
                | Confirmation::LaunchFailed { previous: mode, .. }
                | Confirmation::ConvertRawCheckout { previous: mode, .. } => {
                    self.restore_dismissed_mode(mode);
                }
                Confirmation::DestroyStopped { reopen, .. } => {
                    self.restore_after_confirmation(reopen);
                }
                _ => self.cancel_modal(),
            },
            Some(Interaction::Activate(DialogControl::ConfirmButton(index))) => {
                return self.activate_confirmation_button(dialog.confirmation, index);
            }
            _ => self.mode = Mode::Confirm(dialog),
        }
        DashboardAction::None
    }

    /// Runs the button at `index` of `confirmation_buttons`, where index 0 is always Cancel.
    fn activate_confirmation_button(
        &mut self,
        confirmation: Confirmation,
        index: usize,
    ) -> DashboardAction {
        match (confirmation, index) {
            (
                Confirmation::ConfigurationRepair {
                    session_id,
                    previous,
                    ..
                },
                index,
            ) => {
                self.restore_dismissed_mode(previous);
                match index {
                    1 => DashboardAction::Open { session_id },
                    2 => {
                        self.begin_setup();
                        DashboardAction::None
                    }
                    _ => DashboardAction::None,
                }
            }

            (
                Confirmation::RepairRepositoryRemotes {
                    action, previous, ..
                },
                index,
            ) => {
                self.restore_dismissed_mode(previous);
                if index == 1 {
                    *action
                } else {
                    DashboardAction::None
                }
            }
            (
                Confirmation::LaunchFailed {
                    retry, previous, ..
                },
                index,
            ) => {
                self.restore_dismissed_mode(previous);
                if (retry.is_some() && index == 2) || (retry.is_none() && index == 1) {
                    self.begin_setup();
                    DashboardAction::None
                } else if index == 1 {
                    retry.map(|action| *action).unwrap_or(DashboardAction::None)
                } else {
                    DashboardAction::None
                }
            }
            (Confirmation::Dismiss { mode, intent: _ }, 0) => self.restore_dismissed_mode(mode),
            (Confirmation::Dismiss { intent, .. }, 1) => {
                self.cancel_modal();
                match intent {
                    DismissalIntent::DiscardSetup => DashboardAction::None,
                    DismissalIntent::CancelImport => DashboardAction::CancelImport,
                }
            }
            (
                Confirmation::ConvertRawCheckout {
                    launch,
                    receipt,
                    previous,
                    ..
                },
                index,
            ) => {
                if index == 1 {
                    self.cancel_modal();
                    DashboardAction::ConfirmRawConversion { launch, receipt }
                } else {
                    self.restore_dismissed_mode(previous)
                }
            }
            (Confirmation::ForceDestroy { session_id }, 1) => {
                self.cancel_modal();
                DashboardAction::ForceDestroy { session_id }
            }
            (Confirmation::DestroyStopped { session_id, reopen }, 1) => {
                self.restore_after_confirmation(reopen);
                DashboardAction::DestroyStopped { session_id }
            }
            (Confirmation::CloseFailed { session_id, .. }, 1) => {
                self.cancel_modal();
                DashboardAction::ForceStop { session_id }
            }
            (Confirmation::CloseFailed { session_id, .. }, 2) => {
                self.cancel_modal();
                DashboardAction::Close { session_id }
            }
            (Confirmation::StopWithSubagents { session_id, .. }, 1) => {
                self.cancel_modal();
                DashboardAction::Close { session_id }
            }
            (Confirmation::RecoverFailed { session_id, .. }, 1) => {
                self.cancel_modal();
                self.focus = crate::Focus::Prompt;
                DashboardAction::Open { session_id }
            }
            (Confirmation::RecoverFailed { session_id, .. }, 2) => {
                self.cancel_modal();
                self.begin_resume_for(&session_id)
            }
            (Confirmation::RecoverMove { operation }, 2) => {
                self.cancel_modal();
                DashboardAction::RetryMove { operation }
            }
            (Confirmation::RecoverMove { operation }, 3) => {
                self.cancel_modal();
                DashboardAction::ResumeMove { operation }
            }
            (Confirmation::DestroyStopped { reopen, .. }, _) => {
                self.restore_after_confirmation(reopen);
                DashboardAction::None
            }
            _ => {
                self.cancel_modal();
                DashboardAction::None
            }
        }
    }

    fn restore_dismissed_mode(&mut self, mode: Box<Mode>) -> DashboardAction {
        self.mode = *mode;
        self.mark_render_changed();
        DashboardAction::None
    }

    /// Returns to the resume dialog a confirmation interrupted, or to the
    /// dashboard when the confirmation did not come from one.
    fn restore_after_confirmation(&mut self, reopen: Option<Box<crate::resume::ResumeDialog>>) {
        match reopen {
            Some(dialog) => {
                self.mode = Mode::ResumeDialog(*dialog);
                self.rebuild_resume_rows();
            }
            None => self.cancel_modal(),
        }
    }
}

#[cfg(test)]
mod tests;
