//! Modal dialogs: session import, confirmations, and the rename editor.

pub(crate) mod render;
pub(crate) use render::*;

mod container;
pub(crate) use container::{ContainerEditFocus, ContainerEditor, render_container_editor};

use std::cell::RefCell;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEventKind};
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
    Button, Checkbox, ChoiceList, ControlKind, Dialog, EditOutcome, Interaction, TextField,
};
use mj_chat::selection::FrameSurfaces;
use mj_chat::text_input::TextInput;
use mj_chat::{components::PathField, path_input::PathInput};
use mj_core::targets::{
    AdditionalMount, MountAccess, default_mount_destination, validate_additional_mounts,
};

use crate::widgets::{
    Truncate, centered_modal, centered_modal_fixed, dismissible_modal_title, modal_area,
    popup_height, truncate_to_cells,
};
use crate::wizards::{access_marker, render_access_combo};
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
    ChangedFilesRefresh,
    ChangedFilesClose,
    NoticeLogClose,
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

/// The notice log: the last notices the footer showed, newest first, so a
/// burst of background failures that overwrote each other can still be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NoticeLogDialog {
    /// First wrapped line drawn.
    pub(crate) scroll: usize,
    /// The largest useful `scroll`, measured by the renderer at its width.
    pub(crate) max_scroll: std::cell::Cell<usize>,
    pub(crate) form: RefCell<Dialog<DialogControl>>,
}

/// The changed-files overlay for one session. The data lives on the
/// dashboard (`git_status`), so a probe that answers while the overlay is
/// open, or after it closed, lands in the same place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChangedFilesDialog {
    pub(crate) session_id: String,
    /// First listed file drawn, for a list longer than the overlay.
    pub(crate) scroll: usize,
    pub(crate) form: RefCell<Dialog<DialogControl>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RenameEditor {
    pub(crate) session_id: String,
    /// The name the Sessions list shows, which the dialog names it by.
    pub(crate) session_name: String,
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
        can_discard: bool,
    },
    SuspendSession {
        session_id: String,
        active_children: usize,
        interrupting: bool,
    },
    DiscardSinceCheckpoint {
        session_id: String,
        checkpoint: mj_core::state::CheckpointMetadata,
    },
    /// Stop or restart asked for while the agent is mid-turn. An idle session
    /// stops without asking; this exists for the one case a mis-click costs
    /// work in progress.
    InterruptWork {
        session_id: String,
        restart: bool,
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
    /// The name the Sessions list shows for the session the confirmation is
    /// about. The body names the session by it, and falls back to the id
    /// when it is not set.
    pub(crate) session_name: Option<String>,
    pub(crate) form: RefCell<Dialog<DialogControl>>,
}

impl ConfirmDialog {
    pub(crate) fn new(confirmation: Confirmation) -> Self {
        Self {
            scroll: 0,
            max_scroll: std::cell::Cell::new(0),
            form: confirmation_form(&confirmation),
            confirmation,
            session_name: None,
        }
    }

    /// Names the session in the body by `name` rather than by its id.
    pub(crate) fn naming_session(mut self, name: &str) -> Self {
        self.session_name = Some(name.to_owned());
        self
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

/// What one event did to a text-prompt dialog: a modal holding a single text
/// field with Cancel and Save.
pub(crate) enum TextPromptOutcome {
    /// Esc or Cancel. The dialog's owner decides where the cancel goes.
    Cancel,
    /// The dialog stays open, whether or not the event changed the value.
    Edited,
    /// Save was pressed on an empty value. The notice is already set; the
    /// dialog stays open.
    Rejected,
    /// Save was pressed on a value the dialog can act on. The owner builds
    /// its own action from it.
    Submit,
}

fn clear_dialog_form_geometry(form: &mut Dialog<DialogControl>) {
    // Keep declarations available for keyboard input while the modal is
    // clipped, but discard hitboxes and any in-flight mouse gesture.
    form.cancel_pointer();
    form.reset_geometry();
}

impl DashboardState {
    pub fn apply_web_access(&mut self, access: WebViewerAccess) {
        let Mode::Web(current) = &mut self.mode else {
            return;
        };
        if matches!(access, WebViewerAccess::Starting) {
            current.loading = true;
            current.reset_form();
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
        self.mode = Mode::Web(dialog);
    }

    pub fn apply_web_listeners(&mut self, result: Result<Vec<WebListenerProcess>, String>) {
        let Mode::Web(dialog) = &mut self.mode else {
            return;
        };
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
    }

    pub fn apply_web_error(&mut self, error: String) {
        let Mode::Web(dialog) = &mut self.mode else {
            return;
        };
        dialog.loading = false;
        dialog.confirm_stop = None;
        dialog.inspection_message = Some(error.clone());
        if dialog.failed_address.is_none() {
            dialog.message = Some(error);
        }
        dialog.reset_form();
    }

    pub(crate) fn handle_web_event(
        &mut self,
        event: Event,
        mut dialog: WebDialog,
    ) -> DashboardAction {
        let result = dialog.form.get_mut().handle(&event);
        self.last_event_consumed.set(result.consumed);
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
    }

    /// Whether the target-actions dialog is running a test right now.
    ///
    /// The cancel command answers over this dialog, which is the one modal it
    /// is allowed through, so both the availability gate and the dispatch arm
    /// ask this question.
    pub(crate) fn target_test_running(&self) -> bool {
        matches!(&self.mode, Mode::TargetActions(dialog) if dialog.testing.is_some())
    }

    /// Cancels the target test the dialog is running, if there is one.
    pub(crate) fn cancel_target_test(&mut self) -> Option<DashboardAction> {
        let Mode::TargetActions(dialog) = &mut self.mode else {
            return None;
        };
        dialog.testing.as_ref()?;
        dialog.testing = None;
        dialog.result = Some(("Target test".into(), Err("cancelled".into())));
        sync_target_actions_form(dialog);
        Some(DashboardAction::CancelTargetTest)
    }

    pub(crate) fn handle_target_actions_event(
        &mut self,
        event: Event,
        mut dialog: TargetActionsDialog,
    ) -> DashboardAction {
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
            }
        }
        // The field's kind carries its popup state, so it is re-declared on
        // every event before the form reads keys against it.
        let result = dialog.form.get_mut().handle(&event);
        self.last_event_consumed.set(result.consumed);
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
                        return DashboardAction::None;
                    }
                    DialogControl::TargetTest if dialog.testing.is_none() => {
                        dialog.testing = Some(target_id.clone());
                        dialog.result = None;
                        sync_target_actions_form(&mut dialog);
                        self.mode = Mode::TargetActions(dialog);
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

    /// Routes one event through a text-prompt dialog: the single text field,
    /// Cancel, and Save with the empty-value check both dialogs make. The
    /// caller keeps what only it knows: where a cancel goes and what a
    /// submitted value means.
    fn handle_text_prompt_event(
        &self,
        form: &mut Dialog<DialogControl>,
        value: &mut TextInput,
        event: &Event,
        empty_message: &str,
    ) -> TextPromptOutcome {
        let result = form.handle(event);
        self.last_event_consumed.set(result.consumed);
        match result.action {
            Some(Interaction::Cancel | Interaction::Activate(DialogControl::Cancel)) => {
                TextPromptOutcome::Cancel
            }
            Some(Interaction::Edit(DialogControl::Field, edit)) => {
                TextField::apply(value, edit);
                TextPromptOutcome::Edited
            }
            Some(Interaction::Activate(DialogControl::Field | DialogControl::Save)) => {
                if value.trim().is_empty() {
                    self.notices.set(empty_message);
                    TextPromptOutcome::Rejected
                } else {
                    TextPromptOutcome::Submit
                }
            }
            _ => TextPromptOutcome::Edited,
        }
    }

    pub(crate) fn handle_config_id_event(
        &mut self,
        event: Event,
        mut editor: ConfigIdEditor,
    ) -> DashboardAction {
        let outcome = self.handle_text_prompt_event(
            editor.form.get_mut(),
            &mut editor.value,
            &event,
            "Configuration ID cannot be empty.",
        );
        match outcome {
            TextPromptOutcome::Cancel => {
                // This editor can be reached from the target actions dialog,
                // which expects to come back when the rename is abandoned.
                if let Some(parent) = editor.return_to.take() {
                    self.mode = Mode::TargetActions(*parent);
                } else {
                    self.cancel_modal();
                }
            }
            TextPromptOutcome::Edited | TextPromptOutcome::Rejected => {
                self.mode = Mode::ConfigId(editor);
            }
            TextPromptOutcome::Submit => {
                self.cancel_modal();
                return match editor.kind {
                    ConfigEntryKind::Profile => DashboardAction::RenameProfile {
                        old_id: editor.old_id,
                        new_id: editor.value.into_value(),
                    },
                    ConfigEntryKind::Target => DashboardAction::RenameTarget {
                        old_id: editor.old_id,
                        new_id: editor.value.into_value(),
                    },
                };
            }
        }
        DashboardAction::None
    }

    pub fn apply_target_test(&mut self, target_id: String, result: Result<(), String>) {
        if let Mode::TargetActions(dialog) = &mut self.mode
            && dialog.testing.as_deref() == Some(&target_id)
        {
            dialog.testing = None;
            dialog.result = Some((target_id, result));
            sync_target_actions_form(dialog);
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
    }

    pub fn apply_repository_origin_failure(&mut self, repository_id: &str, error: String) {
        if let Mode::RepositoryOrigin(dialog) = &mut self.mode
            && dialog.repository_id == repository_id
        {
            dialog.error = Some(error);
            dialog.form.get_mut().focus(DialogControl::Field);
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
        // The field's kind carries its popup state, so it is re-declared on
        // every event before the form reads keys against it.
        let kind = dialog.replacement.control_kind();
        dialog.form.get_mut().declare(DialogControl::Field, kind);
        let result = dialog.form.get_mut().handle(&event);
        self.last_event_consumed.set(result.consumed);
        let interaction =
            match crate::wizards::route_path_completion(self, &mut dialog, result.action) {
                Ok(action) => {
                    self.mode = Mode::RepositoryOrigin(dialog);
                    return action;
                }
                Err(interaction) => interaction,
            };
        match interaction {
            Some(Interaction::Cancel) | Some(Interaction::Activate(DialogControl::Cancel)) => {
                self.cancel_modal();
            }
            Some(Interaction::Edit(DialogControl::Field, edit)) => {
                if PathField::apply(&mut dialog.replacement, edit) == EditOutcome::Changed {
                    dialog.error = None;
                }
                self.mode = Mode::RepositoryOrigin(dialog);
            }
            Some(Interaction::Activate(DialogControl::Field | DialogControl::Primary)) => {
                if dialog.replacement.trim().is_empty() {
                    dialog.error = Some("Enter the repository's new origin.".into());
                    dialog.form.get_mut().focus(DialogControl::Field);
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
    }

    /// Show the recovery choices after a checkpointed close could not finish.
    pub fn show_close_failure(&mut self, session_id: String, error: impl Into<String>) {
        self.mode = Mode::Confirm(ConfirmDialog::new(Confirmation::CloseFailed {
            can_discard: self
                .state
                .sessions
                .get(&session_id)
                .is_some_and(|s| s.checkpoint.is_some()),
            session_id,
            error: error.into(),
        }));
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
    }

    pub fn update_import_progress(&mut self, step: usize, total: Option<usize>, message: String) {
        let Some(progress) = import_progress_mut(&mut self.mode) else {
            return;
        };
        progress.step = step;
        progress.total = total;
        progress.message = message;
        progress.last_updated = Instant::now();
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
        self.last_event_consumed.set(result.consumed);
        let interaction = result.action;
        match interaction {
            Some(Interaction::Cancel) | Some(Interaction::Activate(DialogControl::Cancel)) => {
                self.mode = Mode::Confirm(ConfirmDialog::new(Confirmation::Dismiss {
                    mode: Box::new(Mode::Importing(progress)),
                    intent: DismissalIntent::CancelImport,
                }));
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
            session_name: session.display_title().to_owned(),
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
    }

    pub(crate) fn begin_notice_log(&mut self) {
        self.mode = Mode::NoticeLog(NoticeLogDialog {
            scroll: 0,
            max_scroll: std::cell::Cell::new(0),
            form: RefCell::new(Dialog::default()),
        });
    }

    pub(crate) fn handle_notice_log_event(
        &mut self,
        event: Event,
        mut dialog: NoticeLogDialog,
    ) -> DashboardAction {
        let last = dialog.max_scroll.get();
        if let Event::Key(key) = &event
            && key.kind != KeyEventKind::Release
        {
            let scrolled = match key.code {
                KeyCode::Down | KeyCode::Char('j') => Some(dialog.scroll.saturating_add(1)),
                KeyCode::Up | KeyCode::Char('k') => Some(dialog.scroll.saturating_sub(1)),
                KeyCode::PageDown => Some(dialog.scroll.saturating_add(10)),
                KeyCode::PageUp => Some(dialog.scroll.saturating_sub(10)),
                KeyCode::Home => Some(0),
                KeyCode::End => Some(last),
                _ => None,
            };
            if let Some(scroll) = scrolled {
                dialog.scroll = scroll.min(last);
                self.record_event_handled();
                self.mode = Mode::NoticeLog(dialog);
                return DashboardAction::None;
            }
        }
        let result = dialog.form.get_mut().handle(&event);
        self.last_event_consumed.set(result.consumed);
        match result.action {
            Some(Interaction::Cancel)
            | Some(Interaction::Activate(DialogControl::NoticeLogClose)) => {
                self.cancel_modal();
            }
            _ => self.mode = Mode::NoticeLog(dialog),
        }
        DashboardAction::None
    }

    /// Opens the changed-files overlay for the selected session and asks the
    /// host to read its checkout, so the list is fresh even when a row was
    /// probed a while ago.
    pub(crate) fn begin_changed_files(&mut self) -> DashboardAction {
        let Some(session_id) = self.selected_session().map(|session| session.id.clone()) else {
            return DashboardAction::None;
        };
        self.mode = Mode::ChangedFiles(ChangedFilesDialog {
            session_id: session_id.clone(),
            scroll: 0,
            form: RefCell::new(Dialog::default()),
        });
        self.request_git_probe(&session_id)
    }

    pub(crate) fn handle_changed_files_event(
        &mut self,
        event: Event,
        mut dialog: ChangedFilesDialog,
    ) -> DashboardAction {
        let files = self
            .git_status
            .get(&dialog.session_id)
            .and_then(|status| status.as_ref().ok())
            .map_or(0, |status| status.changed.len());
        if let Event::Key(key) = &event
            && key.kind != KeyEventKind::Release
        {
            let last = files.saturating_sub(1);
            let scrolled = match key.code {
                KeyCode::Down | KeyCode::Char('j') => Some(dialog.scroll.saturating_add(1)),
                KeyCode::Up | KeyCode::Char('k') => Some(dialog.scroll.saturating_sub(1)),
                KeyCode::PageDown => Some(dialog.scroll.saturating_add(10)),
                KeyCode::PageUp => Some(dialog.scroll.saturating_sub(10)),
                KeyCode::Home => Some(0),
                KeyCode::End => Some(last),
                KeyCode::Char('r') => {
                    let session_id = dialog.session_id.clone();
                    self.mode = Mode::ChangedFiles(dialog);
                    return self.request_git_probe(&session_id);
                }
                _ => None,
            };
            if let Some(scroll) = scrolled {
                dialog.scroll = scroll.min(last);
                self.record_event_handled();
                self.mode = Mode::ChangedFiles(dialog);
                return DashboardAction::None;
            }
        }
        let result = dialog.form.get_mut().handle(&event);
        self.last_event_consumed.set(result.consumed);
        match result.action {
            Some(Interaction::Cancel)
            | Some(Interaction::Activate(DialogControl::ChangedFilesClose)) => {
                self.cancel_modal();
            }
            Some(Interaction::Activate(DialogControl::ChangedFilesRefresh)) => {
                let session_id = dialog.session_id.clone();
                self.mode = Mode::ChangedFiles(dialog);
                return self.request_git_probe(&session_id);
            }
            _ => self.mode = Mode::ChangedFiles(dialog),
        }
        DashboardAction::None
    }

    pub(crate) fn handle_rename_event(
        &mut self,
        event: Event,
        mut editor: RenameEditor,
    ) -> DashboardAction {
        let outcome = self.handle_text_prompt_event(
            editor.form.get_mut(),
            &mut editor.title,
            &event,
            "Session name cannot be empty.",
        );
        match outcome {
            TextPromptOutcome::Cancel => self.cancel_modal(),
            TextPromptOutcome::Edited => self.mode = Mode::Rename(editor),
            TextPromptOutcome::Rejected => {
                // Put the caret back where the missing name has to be typed.
                editor.form.get_mut().focus(DialogControl::Field);
                self.mode = Mode::Rename(editor);
            }
            TextPromptOutcome::Submit => {
                self.cancel_modal();
                return DashboardAction::RenameSession {
                    session_id: editor.session_id,
                    title: editor.title.into_value(),
                };
            }
        }
        DashboardAction::None
    }

    pub(crate) fn handle_import_bundle_event(
        &mut self,
        event: Event,
        mut confirmation: ImportBundleConfirmation,
    ) -> DashboardAction {
        let result = confirmation.form.get_mut().handle(&event);
        self.last_event_consumed.set(result.consumed);
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
        // Every button has a letter, derived from its label by
        // `confirmation_accelerators`, so a confirmation never needs Tab.
        if let Event::Key(key) = &event
            && key.kind != crossterm::event::KeyEventKind::Release
            && !key.modifiers.intersects(
                crossterm::event::KeyModifiers::CONTROL | crossterm::event::KeyModifiers::ALT,
            )
            && let crossterm::event::KeyCode::Char(letter) = key.code
        {
            let buttons = crate::dialogs::render::confirmation_buttons(&dialog.confirmation);
            let accelerators = crate::dialogs::render::confirmation_accelerators(buttons);
            if let Some(index) = accelerators
                .iter()
                .position(|accelerator| *accelerator == letter.to_ascii_lowercase())
            {
                return self.activate_confirmation_button(dialog.confirmation, index);
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
            return DashboardAction::None;
        }
        self.handle_confirmation_controls(dialog, event)
    }

    fn handle_confirmation_controls(
        &mut self,
        mut dialog: ConfirmDialog,
        event: Event,
    ) -> DashboardAction {
        // The field's kind carries its popup state, so it is re-declared on
        // every event before the form reads keys against it.
        let result = dialog.form.get_mut().handle(&event);
        self.last_event_consumed.set(result.consumed);
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
            // Button 1 destroys the session and leaves its branch alone;
            // button 2 is the explicit opt-in to delete the branch too.
            (Confirmation::ForceDestroy { session_id }, index @ (1 | 2)) => {
                self.cancel_modal();
                DashboardAction::ForceDestroy {
                    session_id,
                    delete_branch: index == 2,
                }
            }
            (Confirmation::DestroyStopped { session_id, reopen }, index @ (1 | 2)) => {
                self.restore_after_confirmation(reopen);
                DashboardAction::DestroyStopped {
                    session_id,
                    delete_branch: index == 2,
                }
            }
            (
                Confirmation::CloseFailed {
                    session_id,
                    can_discard: true,
                    ..
                },
                1,
            ) => {
                if let Some(checkpoint) = self
                    .state
                    .sessions
                    .get(&session_id)
                    .and_then(|s| s.checkpoint.clone())
                {
                    self.mode =
                        Mode::Confirm(ConfirmDialog::new(Confirmation::DiscardSinceCheckpoint {
                            session_id,
                            checkpoint,
                        }));
                } else {
                    self.set_notice("No recovery copy is available. Retry suspension.");
                }
                DashboardAction::None
            }
            (
                Confirmation::DiscardSinceCheckpoint {
                    session_id,
                    checkpoint,
                },
                1,
            ) => {
                if self
                    .state
                    .sessions
                    .get(&session_id)
                    .and_then(|s| s.checkpoint.as_ref())
                    != Some(&checkpoint)
                {
                    self.cancel_modal();
                    self.set_notice(
                        "The recovery copy changed. Review it before discarding changes.",
                    );
                    return DashboardAction::None;
                }
                self.cancel_modal();
                DashboardAction::DiscardSinceCheckpoint {
                    session_id,
                    checkpoint,
                }
            }
            (Confirmation::CloseFailed { session_id, .. }, 2)
            | (
                Confirmation::CloseFailed {
                    session_id,
                    can_discard: false,
                    ..
                },
                1,
            ) => {
                self.cancel_modal();
                DashboardAction::Suspend { session_id }
            }
            (Confirmation::SuspendSession { session_id, .. }, 1) => {
                self.cancel_modal();
                DashboardAction::Suspend { session_id }
            }
            (
                Confirmation::InterruptWork {
                    session_id,
                    restart,
                },
                1,
            ) => {
                self.cancel_modal();
                if restart {
                    DashboardAction::RestartSession { session_id }
                } else {
                    DashboardAction::Suspend { session_id }
                }
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

impl crate::wizards::CompletesPaths for RepositoryOriginDialog {
    /// The replacement origin may be a URL, so it completes only while it
    /// reads as a path on the controller.
    fn focused_path_input(
        &mut self,
        _dashboard: &DashboardState,
    ) -> Option<(
        &mut PathInput,
        mj_core::path_completion::CompletionHost,
        mj_core::path_completion::CompletionKind,
    )> {
        if !self.form.borrow().is_focused(DialogControl::Field)
            || !mj_core::path_completion::looks_like_path(self.replacement.value())
        {
            return None;
        }
        Some((
            &mut self.replacement,
            mj_core::path_completion::CompletionHost::Local,
            mj_core::path_completion::CompletionKind::Directories,
        ))
    }

    fn dismiss_unfocused_completions(&mut self) {
        if !self.form.borrow().is_focused(DialogControl::Field) {
            self.replacement.dismiss_completion();
        }
    }
}

#[cfg(test)]
mod tests;
