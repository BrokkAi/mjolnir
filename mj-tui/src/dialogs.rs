//! Modal dialogs: session import, confirmations, and the rename editor.

mod container;
#[cfg(test)]
use container::ContainerEditFocus;
pub(crate) use container::{ContainerEditor, render_container_editor};

use std::cell::RefCell;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use qrcode::QrCode;
use qrcode::types::{Color as QrColor, EcLevel};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use std::path::PathBuf;

use hel::hel_config::{HarnessKind, mount_history_host};
use hel::hel_targets::{AdditionalMount, default_mount_destination, validate_additional_mounts};
use mj_chat::components::{
    Button, ButtonRow, Checkbox, ChoiceList, ControlKind, Form, Interaction, Outcome, TextField,
};
use mj_chat::hel_selection::FrameSurfaces;
use mj_chat::hel_text_input::TextInput;

use crate::widgets::{
    centered_modal, centered_modal_fixed, modal_area, popup_height, truncate_text,
};
use crate::wizards::read_only_marker;
use crate::{DashboardAction, DashboardState, Mode, WebViewerAccess};

pub(crate) const FORCE_STOP_CONFIRMATION: &str = "STOP";

const IMPORT_STALL_WARNING_AFTER: Duration = Duration::from_secs(10);

/// Stable control identities used by the standard dashboard dialogs.
///
/// Each dialog owns its own [`Form`], so the shared identities can be reused
/// across modes while retaining focus and pointer state during redraws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DialogControl {
    Field,
    TypedField,
    Cancel,
    Save,
    Primary,
    TargetList,
    TargetRename,
    TargetTest,
    TargetClose,
    ConfirmButton(usize),
    ImportIgnore,
    ImportCancel,
    ImportContinue,
    WebClose,
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
    pub(crate) form: RefCell<Form<DialogControl>>,
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
    pub(crate) kind: ConfigEntryKind,
    pub(crate) old_id: String,
    pub(crate) value: TextInput,
    pub(crate) form: RefCell<Form<DialogControl>>,
    pub(crate) return_to_targets: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TargetActionsDialog {
    pub(crate) target_ids: Vec<String>,
    pub(crate) target_index: usize,
    pub(crate) form: RefCell<Form<DialogControl>>,
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
    pub(crate) form: RefCell<Form<DialogControl>>,
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
            form: dialog_form(&[DialogControl::WebClose], DialogControl::WebClose),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepositoryOriginDialog {
    pub(crate) session_id: String,
    pub(crate) repository_id: String,
    pub(crate) missing_commit: String,
    pub(crate) archived_origin: String,
    pub(crate) configured_origin: String,
    pub(crate) replacement: TextInput,
    pub(crate) error: Option<String>,
    pub(crate) form: RefCell<Form<DialogControl>>,
    pub(crate) launch: Box<DashboardAction>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Confirmation {
    DirtyLocal {
        action: DashboardAction,
        repositories: Vec<String>,
    },
    Close {
        session_id: String,
        /// Whether the latest relay projection has a turn in flight. The
        /// controller checks authoritatively at close time; this only chooses
        /// the warning the person sees.
        active_turn: bool,
        /// Whether a second opinion is open on this session. Stopping tears
        /// the target down, and the reviewer's conversation goes with it.
        reviewer_conversation: bool,
    },
    CloseFailed {
        session_id: String,
        error: String,
    },
    ForceStop {
        session_id: String,
        typed: TextInput,
    },
    ForceDestroy {
        session_id: String,
        /// The short id the user must type: this confirmation also destroys
        /// the recovery archive, so it names the session being destroyed
        /// instead of a fixed word.
        expected: String,
        typed: TextInput,
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
}

/// A confirmation dialog and its persistent standard-control state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfirmDialog {
    pub(crate) confirmation: Confirmation,
    pub(crate) form: RefCell<Form<DialogControl>>,
}

impl ConfirmDialog {
    pub(crate) fn new(confirmation: Confirmation) -> Self {
        Self {
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
    pub(crate) form: RefCell<Form<DialogControl>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImportBundleConfirmation {
    dirty_git_roots: Vec<String>,
    omitted_non_git_dirs: Vec<String>,
    scratch_git_roots: Vec<String>,
    has_untracked_files: bool,
    ignore_untracked: bool,
    pub(crate) form: RefCell<Form<DialogControl>>,
}

fn dialog_form(controls: &[DialogControl], initial: DialogControl) -> RefCell<Form<DialogControl>> {
    let mut form = Form::new();
    for id in controls {
        let kind = match id {
            DialogControl::Field | DialogControl::TypedField => ControlKind::TextField,
            DialogControl::TargetList => ControlKind::ChoiceList {
                len: 0,
                selected: 0,
            },
            DialogControl::ImportIgnore => ControlKind::Checkbox,
            _ => ControlKind::Button,
        };
        form.declare(*id, kind);
    }
    form.end_frame(initial);
    RefCell::new(form)
}

fn confirmation_form(confirmation: &Confirmation) -> RefCell<Form<DialogControl>> {
    let buttons = confirmation_buttons(confirmation);
    let mut form = Form::new();
    if confirmation_has_typed_field(confirmation) {
        form.declare(DialogControl::TypedField, ControlKind::TextField);
        form.declare(DialogControl::ConfirmButton(0), ControlKind::Button);
        form.declare_with_enabled(DialogControl::ConfirmButton(1), ControlKind::Button, false);
        form.end_frame(DialogControl::TypedField);
    } else {
        for index in 0..buttons.len() {
            form.declare(DialogControl::ConfirmButton(index), ControlKind::Button);
        }
        form.end_frame(DialogControl::ConfirmButton(primary_button(buttons)));
    }
    RefCell::new(form)
}

fn typed_confirmation_valid(confirmation: &Confirmation) -> bool {
    match confirmation {
        Confirmation::ForceStop { typed, .. } => typed == FORCE_STOP_CONFIRMATION,
        Confirmation::ForceDestroy {
            expected, typed, ..
        } => typed == expected.as_str(),
        _ => false,
    }
}

fn sync_typed_confirmation_form(dialog: &mut ConfirmDialog) {
    let enabled = typed_confirmation_valid(&dialog.confirmation);
    let form = dialog.form.get_mut();
    form.declare(DialogControl::TypedField, ControlKind::TextField);
    form.declare(DialogControl::ConfirmButton(0), ControlKind::Button);
    form.declare_with_enabled(
        DialogControl::ConfirmButton(1),
        ControlKind::Button,
        enabled,
    );
    form.end_frame(DialogControl::TypedField);
}

fn target_actions_form(
    target_count: usize,
    selected: usize,
    initial: DialogControl,
) -> RefCell<Form<DialogControl>> {
    let mut form = Form::new();
    form.declare(
        DialogControl::TargetList,
        ControlKind::ChoiceList {
            len: target_count,
            selected: selected.min(target_count.saturating_sub(1)),
        },
    );
    form.declare(DialogControl::TargetRename, ControlKind::Button);
    form.declare(DialogControl::TargetTest, ControlKind::Button);
    form.declare(DialogControl::TargetClose, ControlKind::Button);
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
    form.declare(DialogControl::TargetClose, ControlKind::Button);
    form.end_frame(DialogControl::TargetList);
}

fn clear_dialog_form_geometry(form: &mut Form<DialogControl>) {
    // Keep declarations available for keyboard input while the modal is
    // clipped, but discard hitboxes and any in-flight mouse gesture.
    form.cancel_pointer();
    form.reset_geometry();
}

fn confirmation_has_typed_field(confirmation: &Confirmation) -> bool {
    matches!(
        confirmation,
        Confirmation::ForceStop { .. } | Confirmation::ForceDestroy { .. }
    )
}

/// Button labels for a confirmation dialog, ordered Cancel first and the primary
/// action last. This is the single declaration used by both key handling and
/// rendering.
fn confirmation_buttons(confirmation: &Confirmation) -> &'static [&'static str] {
    match confirmation {
        Confirmation::DirtyLocal { .. } => &["Cancel", "Continue"],
        Confirmation::Close { .. } => &["Cancel", "Stop"],
        Confirmation::DestroyStopped { .. } => &["Cancel", "Destroy"],
        Confirmation::CloseFailed { .. } => &["Cancel", "Force stop", "Retry stop"],
        Confirmation::RecoverFailed {
            recoverable: true, ..
        } => &["Cancel", "Open transcript", "Recover"],
        Confirmation::RecoverFailed { .. } => &["Cancel", "Open transcript"],
        Confirmation::ForceStop { .. } => &["Cancel", "Force stop"],
        Confirmation::ForceDestroy { .. } => &["Cancel", "Force destroy"],
    }
}

/// Index of the primary (rightmost) button, which is focused when a dialog opens.
fn primary_button(labels: &[&str]) -> usize {
    labels.len().saturating_sub(1)
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
    let stalled_for = progress.last_updated.elapsed();
    let status = if stalled_for >= IMPORT_STALL_WARNING_AFTER {
        Line::styled(
            format!(
                "No progress for {}s; the filesystem may be stalled.",
                stalled_for.as_secs()
            ),
            Style::default().fg(Color::Yellow),
        )
    } else {
        Line::styled(
            "The dashboard remains responsive while the import runs.",
            Style::default().fg(Color::Gray),
        )
    };
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
    frame.render_widget(
        Block::default().borders(Borders::ALL).title(format!(
            " Importing session · progress {}/{total} ",
            progress.step
        )),
        popup,
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
            confirmation
                .dirty_git_roots
                .iter()
                .map(|root| Line::styled(root.clone(), Style::default().fg(Color::Yellow))),
        );
    }
    if !confirmation.omitted_non_git_dirs.is_empty() {
        if !lines.is_empty() {
            lines.push(Line::raw(""));
        }
        lines.push(Line::raw(
            "These edited directories are outside Git and cannot be included:",
        ));
        lines.extend(
            confirmation.omitted_non_git_dirs.iter().map(|directory| {
                Line::styled(directory.clone(), Style::default().fg(Color::Yellow))
            }),
        );
    }
    if !confirmation.scratch_git_roots.is_empty() {
        if !lines.is_empty() {
            lines.push(Line::raw(""));
        }
        lines.push(Line::raw(
            "These scratch repositories are under temporary directories and stay out of the workspace:",
        ));
        lines.extend(
            confirmation
                .scratch_git_roots
                .iter()
                .map(|root| Line::styled(root.clone(), Style::default().fg(Color::Yellow))),
        );
    }
    lines.push(Line::raw(""));
    if confirmation.has_untracked_files {
        lines.push(Line::raw("Space toggles the checkbox."));
    }
    let control_lines = usize::from(confirmation.has_untracked_files) + 2;
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
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .title(" Import safety warning "),
        popup,
    );
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
    let y = inner.y.saturating_add(body_height);
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
    ButtonRow::render(
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
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .title(" Rename session "),
        popup,
    );
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
    TextField::render(frame, field, &editor.title, &mut form, DialogControl::Field);
    ButtonRow::render(
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
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" Rename {} ID ", editor.kind.label())),
        popup,
    );
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
    TextField::render(frame, field, &editor.value, &mut form, DialogControl::Field);
    ButtonRow::render(
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
            Line::styled(format!("{id:<24} {kind}"), Style::default())
        })
        .collect::<Vec<_>>();
    let list_rows = if rows.is_empty() {
        vec![Line::styled(
            "No targets configured.",
            Style::default().fg(Color::DarkGray),
        )]
    } else {
        rows
    };
    let height = u16::try_from(list_rows.len())
        .unwrap_or(u16::MAX)
        .saturating_add(8)
        .max(12);
    let popup = centered_modal(frame, surfaces, 72, height, area);
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .title(" Target actions "),
        popup,
    );
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
            Paragraph::new(format!("Testing {target_id}… Alt-X cancels test"))
                .style(Style::default().fg(Color::Yellow)),
            Rect::new(inner.x, status_y, inner.width, 1),
        );
    } else if let Some((target_id, result)) = &dialog.result {
        frame.render_widget(
            Paragraph::new(match result {
                Ok(()) => format!("{target_id}: ready"),
                Err(error) => format!("{target_id}: {error}"),
            })
            .style(Style::default().fg(if result.is_ok() {
                Color::Green
            } else {
                Color::Yellow
            })),
            Rect::new(inner.x, status_y, inner.width, 1),
        );
    }
    let hint = Rect::new(inner.x, inner.bottom().saturating_sub(2), inner.width, 1);
    frame.render_widget(
        Paragraph::new("Up/Down selects target · Tab selects action · Esc closes")
            .style(Style::default().fg(Color::DarkGray)),
        hint,
    );
    let footer = Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1);
    let mut form = dialog.form.borrow_mut();
    form.begin_frame();
    ChoiceList::render(
        frame,
        list_area,
        &list_rows,
        dialog.target_index,
        &mut form,
        DialogControl::TargetList,
    );
    ButtonRow::render(
        frame,
        footer,
        &[
            (DialogControl::TargetRename, "Rename", true),
            (DialogControl::TargetTest, "Test", dialog.testing.is_none()),
            (DialogControl::TargetClose, "Close", true),
        ],
        &mut form,
    );
    form.end_frame(DialogControl::TargetList);
}

fn target_kind_label(target: &hel::hel_config::TargetTemplate) -> &'static str {
    match target {
        hel::hel_config::TargetTemplate::LocalBare => "local bare",
        hel::hel_config::TargetTemplate::LocalPodman { .. } => "local Podman",
        hel::hel_config::TargetTemplate::LocalDocker { .. } => "local Docker",
        hel::hel_config::TargetTemplate::AppleContainer { .. } => "Apple container",
        hel::hel_config::TargetTemplate::AwsEc2 { .. } => "AWS EC2",
        hel::hel_config::TargetTemplate::SshBare { .. } => "SSH bare",
        hel::hel_config::TargetTemplate::SshPodman { .. } => "SSH Podman",
        hel::hel_config::TargetTemplate::SshDocker { .. } => "SSH Docker",
    }
}

pub(crate) fn render_web_dialog(
    frame: &mut Frame,
    area: Rect,
    dialog: &WebDialog,
    surfaces: &mut FrameSurfaces,
) {
    const FOOTER: &str = "Close";
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
    if dialog.loading {
        lines.push(Line::styled(
            "Loading web viewer access…",
            Style::default().fg(Color::Yellow),
        ));
    } else if let Some(message) = &dialog.message {
        inner_width = inner_width.max(message.chars().count());
        lines.push(Line::styled(
            message.clone(),
            Style::default().fg(Color::Yellow),
        ));
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
                    Style::default().fg(Color::Yellow),
                )
                .centered(),
            );
            lines.push(Line::raw(""));
        }
        if let Some(url) = &dialog.viewer_url {
            // The QR encodes this URL; the text is the fallback for hand entry,
            // so it wraps within the box instead of widening it.
            lines.push(Line::from(vec![
                Span::styled("Web: ", Style::default().fg(Color::DarkGray)),
                Span::styled(url.clone(), Style::default().fg(Color::Cyan)),
            ]));
        }
        if let Some(code) = &dialog.viewer_code {
            lines.push(Line::from(vec![
                Span::styled("Viewer code: ", Style::default().fg(Color::DarkGray)),
                Span::styled(code.clone(), Style::default().fg(Color::Cyan)),
            ]));
        }
        if let Some(reason) = &dialog.fallback_reason {
            lines.push(Line::styled(
                format!("Local fallback: {reason}"),
                Style::default().fg(Color::Yellow),
            ));
        }
    }
    lines.push(Line::raw(""));

    let inner_width = inner_width.min(max_inner).max(1);
    let box_width = u16::try_from(inner_width + 2).unwrap_or(u16::MAX);
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let wrapped =
        u16::try_from(paragraph.line_count(box_width.saturating_sub(2))).unwrap_or(u16::MAX);
    let box_height = wrapped.saturating_add(3).min(inner_area.height);
    let popup = centered_modal_fixed(frame, surfaces, box_width, box_height, area);
    frame.render_widget(
        Block::default().borders(Borders::ALL).title(" Web viewer "),
        popup,
    );
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
        inner.height.saturating_sub(1),
    );
    frame.render_widget(paragraph, body);
    let footer = Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1);
    let mut form = dialog.form.borrow_mut();
    form.begin_frame();
    Button::render(
        frame,
        footer,
        FOOTER,
        true,
        &mut form,
        DialogControl::WebClose,
    );
    form.end_frame(DialogControl::WebClose);
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
            Style::default().fg(Color::Yellow),
        ));
    }
    let body_paragraph = Paragraph::new(lines.clone()).wrap(Wrap { trim: false });
    let popup_height = popup_height(&body_paragraph, 76, 14, area);
    let popup = centered_modal(frame, surfaces, 76, popup_height, area);
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .title(" Repository history is missing "),
        popup,
    );
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
            .style(Style::default().fg(Color::DarkGray)),
        Rect::new(inner.x, hint_y, inner.width, 1),
    );
    let footer = Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1);
    let mut form = dialog.form.borrow_mut();
    form.begin_frame();
    TextField::render(
        frame,
        field,
        &dialog.replacement,
        &mut form,
        DialogControl::Field,
    );
    ButtonRow::render(
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
        Confirmation::DirtyLocal { repositories, .. } => {
            let mut lines = vec![
                Line::raw("The initial worker will include these uncommitted changes:"),
                Line::raw(""),
            ];
            lines.extend(repositories.iter().map(|repository| {
                Line::styled(repository.clone(), Style::default().fg(Color::Yellow))
            }));
            lines.extend([
                Line::raw(""),
                Line::raw("Pushes back to origin are rejected until the local checkout is clean."),
            ]);
            (" Local repository has uncommitted changes ", lines)
        }
        Confirmation::Close {
            session_id,
            active_turn,
            reviewer_conversation,
        } => {
            let mut lines = vec![Line::raw(format!("Session: {session_id}")), Line::raw("")];
            if *active_turn {
                lines.extend([
                    Line::styled(
                        "Mjolnir will interrupt the current turn.",
                        Style::default().fg(Color::Yellow),
                    ),
                    Line::raw("It will then save a recovery copy and destroy the target."),
                ]);
            } else {
                lines.push(Line::raw(
                    "Mjolnir will verify a recovery copy before destroying the target.",
                ));
            }
            if *reviewer_conversation {
                // The reviewer's native session lives on the target, and a v1
                // checkpoint is single session, so resuming cannot bring it
                // back. Saying so before the stop is the only warning there is.
                lines.push(Line::raw(""));
                lines.push(Line::styled(
                    "The second opinion in progress cannot be continued after resume.",
                    Style::default().fg(Color::Yellow),
                ));
                lines.push(Line::raw(
                    "Its review is kept for reference; a later one starts a new conversation.",
                ));
            }
            if *active_turn {
                (" Stop active session? ", lines)
            } else {
                (" Stop session? ", lines)
            }
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
                    Style::default().fg(Color::Yellow),
                ),
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
                    Style::default().fg(Color::Yellow),
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
        Confirmation::ForceStop { session_id, .. } => (
            " FORCE STOP · RECENT WORK MAY BE LOST ",
            vec![
                Line::raw(format!("Session: {session_id}")),
                Line::raw(""),
                Line::raw("The current target will be removed without a new checkpoint."),
                Line::raw("You can resume from the latest verified recovery archive."),
                Line::raw(format!("Type {FORCE_STOP_CONFIRMATION}, then press Enter:")),
            ],
        ),
        Confirmation::ForceDestroy {
            session_id,
            expected,
            ..
        } => (
            " FORCE DESTROY · THE SESSION AND ITS RECOVERY ARCHIVE WILL BE LOST ",
            vec![
                Line::raw(format!("Session: {session_id}")),
                Line::raw(""),
                Line::raw("The target, worktree, recovery archive, and record are removed."),
                Line::raw("Nothing from this session can be resumed or read afterwards."),
                Line::raw(format!(
                    "Type {expected} (this session's short id), then press Enter:"
                )),
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
        Confirmation::DirtyLocal { .. } => 11,
        Confirmation::CloseFailed { .. } => 12,
        Confirmation::Close {
            reviewer_conversation: true,
            ..
        } => 13,
        Confirmation::Close { .. } | Confirmation::DestroyStopped { .. } => 10,
        Confirmation::RecoverFailed { .. } => 12,
        Confirmation::ForceStop { .. } => 10,
        Confirmation::ForceDestroy { .. } => 11,
    };
    let (title, mut lines) = confirmation_body(confirmation);
    let buttons = confirmation_buttons(confirmation);
    let has_typed_field = confirmation_has_typed_field(confirmation);
    lines.push(Line::raw(""));
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let extra = if has_typed_field { 2 } else { 1 };
    let height = popup_height(&paragraph, 72, nominal.saturating_add(extra), area);
    let popup = centered_modal(frame, surfaces, 72, height, area);
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Red))
            .title(title),
        popup,
    );
    let inner = popup.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 1,
    });
    if inner.height == 0 {
        clear_dialog_form_geometry(&mut dialog.form.borrow_mut());
        return;
    }
    let controls_height = if has_typed_field { 2 } else { 1 };
    let body = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner.height.saturating_sub(controls_height),
    );
    frame.render_widget(paragraph, body);
    let mut form = dialog.form.borrow_mut();
    form.begin_frame();
    if has_typed_field {
        let typed = match confirmation {
            Confirmation::ForceStop { typed, .. } | Confirmation::ForceDestroy { typed, .. } => {
                typed
            }
            _ => unreachable!("buttonless confirmation must have a typed field"),
        };
        let field = Rect::new(inner.x, inner.bottom().saturating_sub(2), inner.width, 1);
        TextField::render(frame, field, typed, &mut form, DialogControl::TypedField);
    }
    let footer = Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1);
    ButtonRow::render(
        frame,
        footer,
        &buttons
            .iter()
            .enumerate()
            .map(|(index, label)| {
                (
                    DialogControl::ConfirmButton(index),
                    *label,
                    index == 0 || !has_typed_field || typed_confirmation_valid(confirmation),
                )
            })
            .collect::<Vec<_>>(),
        &mut form,
    );
    form.end_frame(if has_typed_field {
        DialogControl::TypedField
    } else {
        DialogControl::ConfirmButton(primary_button(buttons))
    });
}

impl DashboardState {
    pub fn apply_web_access(&mut self, access: WebViewerAccess) {
        let dialog = match access {
            WebViewerAccess::Ready {
                viewer_url,
                viewer_code,
                qr_login_url,
                fallback_reason,
            } => {
                let (qr, message) = match qr_login_url {
                    Some(url) => match render_qr(&url) {
                        Ok(qr) => (Some(qr), None),
                        Err(error) => (None, Some(error)),
                    },
                    None => (None, None),
                };
                WebDialog {
                    loading: false,
                    viewer_url: Some(viewer_url),
                    viewer_code: Some(viewer_code),
                    fallback_reason,
                    message,
                    qr,
                    form: dialog_form(&[DialogControl::WebClose], DialogControl::WebClose),
                }
            }
            WebViewerAccess::Unavailable(message) => WebDialog {
                loading: false,
                viewer_url: None,
                viewer_code: None,
                fallback_reason: None,
                message: Some(message),
                qr: None,
                form: dialog_form(&[DialogControl::WebClose], DialogControl::WebClose),
            },
        };
        if matches!(self.mode, Mode::Web(_)) {
            self.mode = Mode::Web(dialog);
        }
    }

    pub(crate) fn handle_web_event(
        &mut self,
        event: Event,
        mut dialog: WebDialog,
    ) -> DashboardAction {
        let interaction = dialog.form.get_mut().handle(&event).action;
        match interaction {
            Some(Interaction::Cancel) | Some(Interaction::Activate(DialogControl::WebClose)) => {
                self.cancel_modal();
            }
            _ => self.mode = Mode::Web(dialog),
        }
        DashboardAction::None
    }

    pub(crate) fn begin_profile_rename(&mut self) {
        let Some(old_id) = self.config.profiles.keys().nth(self.quota_index).cloned() else {
            self.notices.set("No profile is selected.");
            return;
        };
        self.mode = Mode::ConfigId(ConfigIdEditor {
            kind: ConfigEntryKind::Profile,
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
            return_to_targets: false,
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
            dialog.form.get_mut().focus(DialogControl::TargetList);
        }
        let interaction = dialog.form.get_mut().handle(&event).action;
        match interaction {
            Some(Interaction::Cancel) => {
                self.cancel_modal();
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
                    DialogControl::TargetRename => {
                        self.mode = Mode::ConfigId(ConfigIdEditor {
                            kind: ConfigEntryKind::Target,
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
                            return_to_targets: true,
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
                    DialogControl::TargetClose => {
                        self.cancel_modal();
                        return DashboardAction::None;
                    }
                    _ => {}
                }
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
        let interaction = editor.form.get_mut().handle(&event).action;
        match interaction {
            Some(Interaction::Cancel) | Some(Interaction::Activate(DialogControl::Cancel)) => {
                if editor.return_to_targets {
                    self.begin_target_actions();
                } else {
                    self.cancel_modal();
                }
            }
            Some(Interaction::Edit(DialogControl::Field, edit)) => {
                TextField::apply(&mut editor.value, edit);
                self.mode = Mode::ConfigId(editor);
            }
            Some(Interaction::Activate(DialogControl::Field | DialogControl::Save)) => {
                if editor.value.trim().is_empty() {
                    self.notices.set("Configuration ID cannot be empty.");
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
            replacement: TextInput::new(),
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
        let interaction = dialog.form.get_mut().handle(&event).action;
        match interaction {
            Some(Interaction::Cancel) | Some(Interaction::Activate(DialogControl::Cancel)) => {
                self.cancel_modal();
            }
            Some(Interaction::Edit(DialogControl::Field, edit)) => {
                if TextField::apply(&mut dialog.replacement, edit) == Outcome::Changed {
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

    /// Show the recovery choices after a checkpointed close could not finish.
    pub fn show_close_failure(&mut self, session_id: String, error: impl Into<String>) {
        self.mode = Mode::Confirm(ConfirmDialog::new(Confirmation::CloseFailed {
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
        let Mode::Importing(progress) = &mut self.mode else {
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
    ) {
        let form = if has_untracked_files {
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
        self.mode = Mode::ConfirmImportBundle(ImportBundleConfirmation {
            dirty_git_roots,
            omitted_non_git_dirs,
            scratch_git_roots,
            has_untracked_files,
            ignore_untracked: has_untracked_files,
            form,
        });
    }

    pub fn show_dirty_local_confirmation(
        &mut self,
        action: DashboardAction,
        repositories: Vec<String>,
    ) {
        self.mode = Mode::Confirm(ConfirmDialog::new(Confirmation::DirtyLocal {
            action,
            repositories,
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
        let interaction = progress.form.get_mut().handle(&event).action;
        match interaction {
            Some(Interaction::Cancel) | Some(Interaction::Activate(DialogControl::Cancel)) => {
                self.mode = Mode::Importing(progress);
                DashboardAction::CancelImport
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
    }

    pub(crate) fn handle_rename_event(
        &mut self,
        event: Event,
        mut editor: RenameEditor,
    ) -> DashboardAction {
        let interaction = editor.form.get_mut().handle(&event).action;
        match interaction {
            Some(Interaction::Cancel) | Some(Interaction::Activate(DialogControl::Cancel)) => {
                self.cancel_modal();
            }
            Some(Interaction::Edit(DialogControl::Field, edit)) => {
                TextField::apply(&mut editor.title, edit);
                self.mode = Mode::Rename(editor);
            }
            Some(Interaction::Activate(DialogControl::Field | DialogControl::Save)) => {
                if editor.title.trim().is_empty() {
                    self.notices.set("Session name cannot be empty.");
                    editor.form.get_mut().focus(DialogControl::Field);
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
        let interaction = confirmation.form.get_mut().handle(&event).action;
        match interaction {
            Some(Interaction::Cancel)
            | Some(Interaction::Activate(DialogControl::ImportCancel)) => {
                return DashboardAction::ConfirmImportBundle {
                    accepted: false,
                    include_untracked: false,
                };
            }
            Some(Interaction::Toggle(DialogControl::ImportIgnore)) => {
                confirmation.ignore_untracked = !confirmation.ignore_untracked;
                self.mode = Mode::ConfirmImportBundle(confirmation);
            }
            Some(Interaction::Activate(DialogControl::ImportContinue)) => {
                return DashboardAction::ConfirmImportBundle {
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
        let interaction = dialog.form.get_mut().handle(&event).action;
        match interaction {
            Some(Interaction::Cancel) => {
                if let Confirmation::DestroyStopped { reopen, .. } = dialog.confirmation {
                    self.restore_after_confirmation(reopen);
                } else {
                    self.cancel_modal();
                }
            }
            Some(Interaction::Edit(DialogControl::TypedField, edit)) => {
                match &mut dialog.confirmation {
                    Confirmation::ForceStop { typed, .. }
                    | Confirmation::ForceDestroy { typed, .. } => {
                        TextField::apply(typed, edit);
                    }
                    _ => {}
                }
                sync_typed_confirmation_form(&mut dialog);
                self.mode = Mode::Confirm(dialog);
            }
            Some(Interaction::Activate(DialogControl::TypedField)) => {
                let valid = typed_confirmation_valid(&dialog.confirmation);
                if valid {
                    match dialog.confirmation {
                        Confirmation::ForceStop { session_id, .. } => {
                            self.cancel_modal();
                            return DashboardAction::ForceStop { session_id };
                        }
                        Confirmation::ForceDestroy { session_id, .. } => {
                            self.cancel_modal();
                            return DashboardAction::ForceDestroy { session_id };
                        }
                        _ => {}
                    }
                } else {
                    self.mode = Mode::Confirm(dialog);
                }
            }
            Some(Interaction::Activate(DialogControl::ConfirmButton(1)))
                if typed_confirmation_valid(&dialog.confirmation) =>
            {
                match dialog.confirmation {
                    Confirmation::ForceStop { session_id, .. } => {
                        self.cancel_modal();
                        return DashboardAction::ForceStop { session_id };
                    }
                    Confirmation::ForceDestroy { session_id, .. } => {
                        self.cancel_modal();
                        return DashboardAction::ForceDestroy { session_id };
                    }
                    _ => self.mode = Mode::Confirm(dialog),
                }
            }
            Some(Interaction::Activate(DialogControl::ConfirmButton(1)))
                if confirmation_has_typed_field(&dialog.confirmation) =>
            {
                // The form normally suppresses activation for a disabled
                // primary button. Keep the state machine safe if an event is
                // supplied directly before the next redraw.
                self.mode = Mode::Confirm(dialog);
            }
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
            (Confirmation::DirtyLocal { mut action, .. }, 1) => {
                if let DashboardAction::CreateSession {
                    allow_dirty_local, ..
                } = &mut action
                {
                    *allow_dirty_local = true;
                }
                self.cancel_modal();
                action
            }
            (Confirmation::Close { session_id, .. }, 1) => {
                self.cancel_modal();
                DashboardAction::Close { session_id }
            }
            (Confirmation::DestroyStopped { session_id, reopen }, 1) => {
                self.restore_after_confirmation(reopen);
                DashboardAction::DestroyStopped { session_id }
            }
            (Confirmation::CloseFailed { session_id, .. }, 1) => {
                self.mode = Mode::Confirm(ConfirmDialog::new(Confirmation::ForceStop {
                    session_id,
                    typed: TextInput::new()
                        .with_max_chars(FORCE_STOP_CONFIRMATION.len())
                        .with_filter(
                            mj_chat::hel_text_input::InputFilter::AsciiAlphabeticUppercase,
                        ),
                }));
                DashboardAction::None
            }
            (Confirmation::CloseFailed { session_id, .. }, 2) => {
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
mod tests {
    use crossterm::event::KeyEvent;
    use std::time::Instant;

    use crossterm::event::{KeyCode, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Position;
    use ratatui::style::Color;

    use hel::hel_state::SessionState;

    use super::*;
    use crate::test_support::*;

    use crate::render::render;
    use crate::{DashboardAction, DashboardState, Mode};

    #[test]
    fn web_qr_has_a_four_module_quiet_zone() {
        let qr = render_qr("https://example.test/auth/login?token=secret").unwrap();
        let lines = qr.lines().collect::<Vec<_>>();
        assert!(lines.len() > 4);
        assert!(lines[0].chars().all(|character| character == ' '));
        assert!(lines[1].chars().all(|character| character == ' '));
        assert!(lines.iter().all(|line| line.starts_with("    ")));
        assert!(lines.iter().all(|line| line.ends_with("    ")));
    }

    fn draw_web_dialog(dialog: &WebDialog, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal
            .draw(|frame| {
                let mut surfaces = FrameSurfaces::new();
                render_web_dialog(frame, frame.area(), dialog, &mut surfaces);
            })
            .expect("draw web dialog");
        buffer_lines(terminal.backend().buffer())
    }

    #[test]
    fn web_dialog_without_a_qr_keeps_access_details_and_close_readable() {
        let mut dialog = WebDialog::loading();
        dialog.loading = false;
        dialog.viewer_url = Some("http://127.0.0.1:37650".to_owned());
        dialog.viewer_code = Some("022160".to_owned());
        dialog.fallback_reason = Some("automatic Tailscale detection is disabled".to_owned());
        let rendered = draw_web_dialog(&dialog, 140, 40).join("\n");
        assert!(rendered.contains("Web viewer"));
        assert!(rendered.contains("http://127.0.0.1:37650"));
        assert!(rendered.contains("Viewer code: 022160"));
        assert!(rendered.contains("[ Close ]"));
    }

    #[test]
    fn web_dialog_wraps_a_long_url_without_truncating_it() {
        // A URL wider than the QR must wrap within the box, not get cut off.
        let url = "https://a-very-long-machine-name.some-tailnet.ts.net:37650/viewer";
        let dialog = WebDialog {
            loading: false,
            viewer_url: Some(url.to_owned()),
            viewer_code: Some("022160".to_owned()),
            fallback_reason: None,
            message: None,
            qr: Some(render_qr(url).unwrap()),
            form: dialog_form(&[DialogControl::WebClose], DialogControl::WebClose),
        };

        let rendered = draw_web_dialog(&dialog, 60, 40);

        // Every box row fits the terminal, so the dialog never overflows.
        assert!(rendered.iter().all(|line| line.chars().count() <= 60));
        // The URL survives in full once the border padding is stripped away.
        // Drop whitespace and the box border so the URL's wrapped halves sit
        // adjacent, then confirm none of its characters were lost.
        let flat = rendered
            .join("")
            .chars()
            .filter(|character| !character.is_whitespace() && *character != '│')
            .collect::<String>();
        assert!(
            flat.contains(url),
            "the full URL should appear (wrapped) in the dialog"
        );
        assert!(
            rendered
                .iter()
                .any(|line| line.contains("Viewer code: 022160"))
        );
    }

    fn dashboard_with_container_session() -> DashboardState {
        let mut session = running_session();
        session.additional_mounts = vec![AdditionalMount {
            source: PathBuf::from("/srv/data"),
            destination: PathBuf::from("/mnt/data"),
            read_only: false,
        }];
        let mut dashboard = dashboard_with_session(session);
        dashboard
            .state
            .mount_history
            .insert("local".into(), vec![PathBuf::from("/srv/models")]);
        dashboard
    }

    fn container_editor(dashboard: &DashboardState) -> &ContainerEditor {
        let Mode::EditContainer(editor) = &dashboard.mode else {
            panic!("expected the container editor");
        };
        editor
    }

    /// Reaches a session command the way the user does now: `F2`, type
    /// enough of the name to pick it out, Enter. The session edit dialog
    /// these fixtures used to press `e` for no longer exists.
    fn through_the_palette(dashboard: &mut DashboardState, query: &str) {
        dashboard.handle_key(key(KeyCode::F(2)));
        assert!(
            matches!(dashboard.mode, Mode::Palette(_)),
            "F2 opens the palette"
        );
        for character in query.chars() {
            dashboard.handle_key(key(KeyCode::Char(character)));
        }
        dashboard.handle_key(key(KeyCode::Enter));
    }

    fn open_container_editor(dashboard: &mut DashboardState) {
        through_the_palette(dashboard, "container");
        assert!(matches!(dashboard.mode, Mode::EditContainer(_)));
    }

    fn open_rename_editor(dashboard: &mut DashboardState) {
        through_the_palette(dashboard, "rename");
        assert!(matches!(dashboard.mode, Mode::Rename(_)));
    }

    fn open_stop_dialog(dashboard: &mut DashboardState) {
        through_the_palette(dashboard, "stop");
        assert!(matches!(
            dashboard.mode,
            Mode::Confirm(ConfirmDialog {
                confirmation: Confirmation::Close { .. },
                ..
            })
        ));
    }

    #[test]
    fn ctrl_e_opens_the_container_editor_only_once_setup_is_done() {
        let mut empty = DashboardState::new(
            hel::hel_config::HelConfig {
                version: hel::hel_config::CONFIG_VERSION,
                newer_config_version: None,
                phone: Default::default(),
                review: Default::default(),
                profiles: Default::default(),
                bundles: Default::default(),
                targets: Default::default(),
            },
            hel::hel_state::HelState::default(),
            Default::default(),
        );
        assert_eq!(
            empty.handle_key(key(KeyCode::Char('e'))),
            DashboardAction::OpenConfig
        );
        assert!(matches!(empty.mode, Mode::Dashboard));

        let mut dashboard = dashboard_with_container_session();
        open_container_editor(&mut dashboard);
        let editor = container_editor(&dashboard);
        assert_eq!(editor.session_id, "session-1");
        assert_eq!(editor.mounts.len(), 1);
        assert_eq!(editor.suggestions, vec![PathBuf::from("/srv/models")]);
    }

    #[test]
    fn container_editor_saves_edited_size_mounts_and_remembered_sources() {
        let mut dashboard = dashboard_with_container_session();
        open_container_editor(&mut dashboard);
        for character in "4".chars() {
            dashboard.handle_key(key(KeyCode::Char(character)));
        }
        dashboard.handle_key(key(KeyCode::Tab));
        for character in "6g".chars() {
            dashboard.handle_key(key(KeyCode::Char(character)));
        }
        assert_eq!(
            container_editor(&dashboard).focused(),
            ContainerEditFocus::Memory
        );

        // Take the remembered directory as the next mount.
        while container_editor(&dashboard).focused() != ContainerEditFocus::Suggestions {
            dashboard.handle_key(key(KeyCode::Tab));
        }
        dashboard.handle_key(key(KeyCode::Enter));
        assert_eq!(container_editor(&dashboard).source, "/srv/models");
        dashboard.handle_key(key(KeyCode::Enter));
        assert_eq!(
            container_editor(&dashboard).mounts,
            vec![
                AdditionalMount {
                    source: PathBuf::from("/srv/data"),
                    destination: PathBuf::from("/mnt/data"),
                    read_only: false,
                },
                AdditionalMount {
                    source: PathBuf::from("/srv/models"),
                    destination: PathBuf::from("/mnt/models"),
                    read_only: false,
                },
            ]
        );

        // Forget the remembered directory, then drop the original mount.
        while container_editor(&dashboard).focused() != ContainerEditFocus::Suggestions {
            dashboard.handle_key(key(KeyCode::Tab));
        }
        dashboard.handle_key(key(KeyCode::Char('d')));
        assert!(container_editor(&dashboard).suggestions.is_empty());
        while container_editor(&dashboard).focused() != ContainerEditFocus::Mounts {
            dashboard.handle_key(key(KeyCode::Tab));
        }
        dashboard.handle_key(key(KeyCode::Up));
        assert_eq!(container_editor(&dashboard).mount_index, 0);
        dashboard.handle_key(key(KeyCode::Char('d')));

        while container_editor(&dashboard).focused() != ContainerEditFocus::Save {
            dashboard.handle_key(key(KeyCode::Tab));
        }
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::SaveContainerSettings {
                session_id: "session-1".into(),
                cpus: Some("4".into()),
                memory: Some("6g".into()),
                additional_mounts: vec![AdditionalMount {
                    source: PathBuf::from("/srv/models"),
                    destination: PathBuf::from("/mnt/models"),
                    read_only: false,
                }],
                mount_history: Vec::new(),
            }
        );
        assert!(matches!(dashboard.mode, Mode::Dashboard));
    }

    #[test]
    fn container_editor_marks_new_and_existing_mounts_read_only() {
        let mut dashboard = dashboard_with_container_session();
        open_container_editor(&mut dashboard);

        // Space on the checkbox attaches the next directory read-only.
        while container_editor(&dashboard).focused() != ContainerEditFocus::Source {
            dashboard.handle_key(key(KeyCode::Tab));
        }
        for character in "/nfs/share".chars() {
            dashboard.handle_key(key(KeyCode::Char(character)));
        }
        dashboard.handle_key(key(KeyCode::Tab));
        dashboard.handle_key(key(KeyCode::Tab));
        assert_eq!(
            container_editor(&dashboard).focused(),
            ContainerEditFocus::ReadOnly
        );
        dashboard.handle_key(key(KeyCode::Char(' ')));
        assert!(container_editor(&dashboard).read_only);
        while container_editor(&dashboard).focused() != ContainerEditFocus::Source {
            dashboard.handle_key(key(KeyCode::Tab));
        }
        dashboard.handle_key(key(KeyCode::Enter));

        // Space on a listed row toggles that row, and the flag is saved.
        while container_editor(&dashboard).focused() != ContainerEditFocus::Mounts {
            dashboard.handle_key(key(KeyCode::Tab));
        }
        dashboard.handle_key(key(KeyCode::Up));
        assert_eq!(container_editor(&dashboard).mount_index, 0);
        dashboard.handle_key(key(KeyCode::Char(' ')));

        while container_editor(&dashboard).focused() != ContainerEditFocus::Save {
            dashboard.handle_key(key(KeyCode::Tab));
        }
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::SaveContainerSettings {
                session_id: "session-1".into(),
                cpus: None,
                memory: None,
                additional_mounts: vec![
                    AdditionalMount {
                        source: PathBuf::from("/srv/data"),
                        destination: PathBuf::from("/mnt/data"),
                        read_only: true,
                    },
                    AdditionalMount {
                        source: PathBuf::from("/nfs/share"),
                        destination: PathBuf::from("/mnt/share"),
                        read_only: true,
                    },
                ],
                mount_history: vec![PathBuf::from("/srv/models")],
            }
        );
    }

    #[test]
    fn container_editor_says_when_the_change_takes_effect() {
        let mut dashboard = dashboard_with_container_session();
        open_container_editor(&mut dashboard);
        let mut terminal = Terminal::new(TestBackend::new(100, 40)).expect("terminal");
        terminal
            .draw(|frame| crate::render::render(frame, &mut dashboard))
            .expect("draw editor");
        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(rendered.contains("Applies when the container is next recreated"));
        assert!(rendered.contains("/srv/data"));
    }

    #[test]
    fn rename_uses_acp_title_as_the_initial_value() {
        let mut dashboard = dashboard_with_session(running_session());
        open_rename_editor(&mut dashboard);
        let Mode::Rename(editor) = &dashboard.mode else {
            panic!("expected rename editor");
        };
        assert_eq!(editor.form.borrow().focused(), Some(DialogControl::Field));
        for character in " v2".chars() {
            dashboard.handle_key(key(KeyCode::Char(character)));
        }
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::RenameSession {
                session_id: "session-1".into(),
                title: "ACP pretty name v2".into(),
            }
        );
        assert!(matches!(dashboard.mode, Mode::Dashboard));
    }

    #[test]
    fn focused_text_fields_own_readline_keys_and_control_c_cancels() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.begin_rename();
        for character in " alpha beta".chars() {
            dashboard.handle_key(key(KeyCode::Char(character)));
        }

        let control_key =
            |character| KeyEvent::new(KeyCode::Char(character), KeyModifiers::CONTROL);
        assert_eq!(
            dashboard.handle_key(control_key('w')),
            DashboardAction::None
        );
        let Mode::Rename(editor) = &dashboard.mode else {
            panic!("expected rename editor");
        };
        assert!(editor.title.ends_with("alpha "));

        assert_eq!(
            dashboard.handle_key(control_key('c')),
            DashboardAction::None
        );
        assert!(matches!(dashboard.mode, Mode::Dashboard));
    }

    fn dashboard_with_rename_editor() -> DashboardState {
        let mut dashboard = dashboard_with_session(running_session());
        open_rename_editor(&mut dashboard);
        dashboard
    }

    fn rename_focus(dashboard: &DashboardState) -> DialogControl {
        let Mode::Rename(editor) = &dashboard.mode else {
            panic!("expected rename editor");
        };
        match editor.form.borrow().focused() {
            Some(DialogControl::Field) => DialogControl::Field,
            Some(DialogControl::Cancel) => DialogControl::Cancel,
            Some(DialogControl::Save) => DialogControl::Save,
            focused => panic!("unexpected rename focus: {focused:?}"),
        }
    }

    #[test]
    fn rename_editor_cycles_focus_from_the_field_through_both_buttons() {
        let mut dashboard = dashboard_with_rename_editor();
        for expected in [
            DialogControl::Cancel,
            DialogControl::Save,
            DialogControl::Field,
            DialogControl::Cancel,
        ] {
            assert_eq!(
                dashboard.handle_key(key(KeyCode::Tab)),
                DashboardAction::None
            );
            assert_eq!(rename_focus(&dashboard), expected);
        }

        let mut dashboard = dashboard_with_rename_editor();
        for expected in [
            DialogControl::Save,
            DialogControl::Cancel,
            DialogControl::Field,
        ] {
            assert_eq!(
                dashboard.handle_key(key(KeyCode::BackTab)),
                DashboardAction::None
            );
            assert_eq!(rename_focus(&dashboard), expected);
        }
    }

    #[test]
    fn rename_editor_arrows_move_between_buttons_but_never_edit_the_field() {
        let mut dashboard = dashboard_with_rename_editor();
        // The field has no cursor, so arrows there change nothing.
        for arrow in [KeyCode::Left, KeyCode::Right] {
            assert_eq!(dashboard.handle_key(key(arrow)), DashboardAction::None);
            assert_eq!(rename_focus(&dashboard), DialogControl::Field);
        }

        dashboard.handle_key(key(KeyCode::Tab));
        assert_eq!(rename_focus(&dashboard), DialogControl::Cancel);
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Right)),
            DashboardAction::None
        );
        assert_eq!(rename_focus(&dashboard), DialogControl::Save);
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Left)),
            DashboardAction::None
        );
        assert_eq!(rename_focus(&dashboard), DialogControl::Cancel);
    }

    #[test]
    fn rename_editor_buttons_ignore_typing_and_backspace() {
        let mut dashboard = dashboard_with_rename_editor();
        dashboard.handle_key(key(KeyCode::Tab));
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Char('x'))),
            DashboardAction::None
        );
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Backspace)),
            DashboardAction::None
        );
        let Mode::Rename(editor) = &dashboard.mode else {
            panic!("expected rename editor");
        };
        assert_eq!(editor.title, "ACP pretty name");
        assert_eq!(editor.form.borrow().focused(), Some(DialogControl::Cancel));
    }

    #[test]
    fn rename_editor_cancel_button_closes_without_renaming() {
        let mut dashboard = dashboard_with_rename_editor();
        dashboard.handle_key(key(KeyCode::Tab));
        assert_eq!(rename_focus(&dashboard), DialogControl::Cancel);
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::None
        );
        assert!(matches!(dashboard.mode, Mode::Dashboard));
    }

    #[test]
    fn rename_editor_save_button_renames_like_the_field() {
        let mut dashboard = dashboard_with_rename_editor();
        dashboard.handle_key(key(KeyCode::Char('!')));
        dashboard.handle_key(key(KeyCode::Tab));
        dashboard.handle_key(key(KeyCode::Tab));
        assert_eq!(rename_focus(&dashboard), DialogControl::Save);
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::RenameSession {
                session_id: "session-1".into(),
                title: "ACP pretty name!".into(),
            }
        );
    }

    #[test]
    fn rename_editor_rejects_an_empty_title_from_the_field_and_the_save_button() {
        for focus_moves in [0, 2] {
            let mut dashboard = dashboard_with_rename_editor();
            let Mode::Rename(editor) = &mut dashboard.mode else {
                panic!("expected rename editor");
            };
            editor.title.clear();
            for _ in 0..focus_moves {
                dashboard.handle_key(key(KeyCode::Tab));
            }
            assert_eq!(
                dashboard.handle_key(key(KeyCode::Enter)),
                DashboardAction::None,
                "{focus_moves} focus moves"
            );
            assert_eq!(
                dashboard.notice().as_deref(),
                Some("Session name cannot be empty."),
                "{focus_moves} focus moves"
            );
            assert!(matches!(dashboard.mode, Mode::Rename(_)), "{focus_moves}");
        }
    }

    #[test]
    fn rename_editor_escape_cancels_from_any_focus() {
        for focus_moves in 0..3 {
            let mut dashboard = dashboard_with_rename_editor();
            for _ in 0..focus_moves {
                dashboard.handle_key(key(KeyCode::Tab));
            }
            assert_eq!(
                dashboard.handle_key(key(KeyCode::Esc)),
                DashboardAction::None,
                "{focus_moves} focus moves"
            );
            assert!(matches!(dashboard.mode, Mode::Dashboard), "{focus_moves}");
        }
    }

    #[test]
    fn rename_editor_highlights_save_until_cancel_takes_focus() {
        let mut dashboard = dashboard_with_rename_editor();
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).expect("terminal");
        let mut button_styles = |dashboard: &mut DashboardState| {
            terminal
                .draw(|frame| render(frame, dashboard))
                .expect("draw rename editor");
            let buffer = terminal.backend().buffer();
            let lines = buffer_lines(buffer);
            let row = lines
                .iter()
                .position(|line| line.contains(" Cancel ") && line.contains(" Save "))
                .expect("button row");
            let y = buffer.area.y + row as u16;
            assert!(!lines.iter().any(|line| line.contains("Enter save")));
            (
                buffer[(buffer.area.x + cell_column(&lines[row], "Cancel"), y)].bg,
                buffer[(buffer.area.x + cell_column(&lines[row], "Save"), y)].bg,
            )
        };

        // The shared button row keeps both buttons in their normal style while
        // the field has focus. Tab then moves the cyan focus style between the
        // footer buttons.
        assert_eq!(button_styles(&mut dashboard), (Color::Reset, Color::Reset));

        dashboard.handle_key(key(KeyCode::Tab));
        assert_eq!(button_styles(&mut dashboard), (Color::Cyan, Color::Reset));

        dashboard.handle_key(key(KeyCode::Tab));
        assert_eq!(button_styles(&mut dashboard), (Color::Reset, Color::Cyan));
    }

    #[test]
    fn import_progress_renders_a_focused_cancel_button_that_enter_presses() {
        let mut dashboard = dashboard_with_session(stopped_session());
        dashboard.show_import_progress("Chosen session".into());
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).expect("terminal");
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw import progress");
        let buffer = terminal.backend().buffer();
        let lines = buffer_lines(buffer);
        let row = lines
            .iter()
            .position(|line| line.contains(" Cancel "))
            .expect("button row");
        let y = buffer.area.y + row as u16;
        let cancel_x = buffer.area.x + cell_column(&lines[row], "Cancel");
        assert_eq!(buffer[(cancel_x, y)].bg, Color::Cyan);
        assert_eq!(buffer[(cancel_x - 1, y)].bg, Color::Cyan);
        assert!(!lines.iter().any(|line| line.contains("Esc cancels this")));
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::CancelImport
        );
    }

    #[test]
    fn import_safety_defaults_to_ignoring_untracked_files_and_can_include_them() {
        let mut dashboard = dashboard_with_session(stopped_session());
        dashboard.show_import_bundle_confirmation(
            vec!["/work/repo — 1 tracked change · 222561 untracked paths".into()],
            Vec::new(),
            Vec::new(),
            true,
        );
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("terminal");
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw safety warning");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("[x] Ignore untracked files"));
        assert!(rendered.contains(" Cancel "));
        assert!(rendered.contains(" Continue "));
        assert!(rendered.contains("Space toggles the checkbox."));
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::ConfirmImportBundle {
                accepted: true,
                include_untracked: false,
            }
        );

        dashboard.show_import_bundle_confirmation(
            vec!["/work/repo — 222561 untracked paths".into()],
            Vec::new(),
            Vec::new(),
            true,
        );
        dashboard.handle_key(key(KeyCode::Tab));
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Char(' '))),
            DashboardAction::None
        );
        dashboard.handle_key(key(KeyCode::BackTab));
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::ConfirmImportBundle {
                accepted: true,
                include_untracked: true,
            }
        );
    }

    #[test]
    fn import_safety_lists_scratch_repositories_left_out_of_the_workspace() {
        let mut dashboard = dashboard_with_session(stopped_session());
        dashboard.show_import_bundle_confirmation(
            Vec::new(),
            Vec::new(),
            vec!["/tmp/claude-1000/scratch".into()],
            false,
        );
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("terminal");
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw safety warning");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(rendered.contains("temporary directories"), "{rendered}");
        assert!(rendered.contains("/tmp/claude-1000/scratch"), "{rendered}");
    }

    #[test]
    fn import_safety_buttons_toggle_the_checkbox_and_cancel_from_the_cancel_button() {
        let mut dashboard = dashboard_with_session(stopped_session());
        dashboard.show_import_bundle_confirmation(
            vec!["/work/repo — 1 tracked change · 3 untracked paths".into()],
            Vec::new(),
            Vec::new(),
            true,
        );

        // Focus starts on Continue; the checkbox is the next control in the
        // shared form, and moving on to Cancel does not disturb its state.
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Tab)),
            DashboardAction::None
        );
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Char(' '))),
            DashboardAction::None
        );
        let Mode::ConfirmImportBundle(confirmation) = &dashboard.mode else {
            panic!("expected import safety confirmation");
        };
        assert!(!confirmation.ignore_untracked);
        assert_eq!(
            confirmation.form.borrow().focused(),
            Some(DialogControl::ImportIgnore)
        );
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Tab)),
            DashboardAction::None
        );
        let Mode::ConfirmImportBundle(confirmation) = &dashboard.mode else {
            panic!("expected import safety confirmation");
        };
        assert_eq!(
            confirmation.form.borrow().focused(),
            Some(DialogControl::ImportCancel)
        );
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::ConfirmImportBundle {
                accepted: false,
                include_untracked: false,
            }
        );

        dashboard.show_import_bundle_confirmation(Vec::new(), Vec::new(), Vec::new(), false);
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Char('y'))),
            DashboardAction::None
        );
        assert!(matches!(dashboard.mode, Mode::ConfirmImportBundle(_)));
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Esc)),
            DashboardAction::ConfirmImportBundle {
                accepted: false,
                include_untracked: false,
            }
        );
    }

    #[test]
    fn importing_session_renders_unknown_then_known_progress_and_ignores_navigation() {
        let mut dashboard = dashboard_with_session(stopped_session());
        dashboard.show_import_progress("Chosen session".into());
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Down)),
            DashboardAction::None
        );
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).expect("terminal");
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw import progress");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Importing session · progress 1/?"));

        dashboard.update_import_progress(2, Some(4), "Native session parsed.".into());
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw known import progress");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Importing session · progress 2/4"));
        assert!(rendered.contains("Native session parsed."));

        let Mode::Importing(progress) = &mut dashboard.mode else {
            panic!("expected import progress");
        };
        progress.last_updated = Instant::now() - IMPORT_STALL_WARNING_AFTER;
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw stalled import progress");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("filesystem may be stalled"));
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Esc)),
            DashboardAction::CancelImport
        );
    }

    #[test]
    fn failed_archive_dialog_offers_retry_or_explicit_force_stop() {
        let mut session = stopped_session();
        session.state = SessionState::Running;
        let mut dashboard = dashboard_with_session(session);
        open_stop_dialog(&mut dashboard);
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::Close {
                session_id: "session-1".into()
            }
        );

        // "Retry stop" is the primary button, so it is focused when the dialog opens.
        dashboard.show_close_failure("session-1".into(), "archive unavailable");
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::Close {
                session_id: "session-1".into()
            }
        );

        dashboard.show_close_failure("session-1".into(), "archive unavailable");
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Char('x'))),
            DashboardAction::None
        );
        // "Force stop" sits between Cancel and Retry stop.
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Left)),
            DashboardAction::None
        );
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::None
        );
        assert!(matches!(
            dashboard.mode,
            Mode::Confirm(ConfirmDialog {
                confirmation: Confirmation::ForceStop { .. },
                ..
            })
        ));
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .unwrap();
        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(rendered.contains("FORCE STOP · RECENT WORK MAY BE LOST"));
        assert!(rendered.contains("resume from the latest verified recovery archive"));
        for character in FORCE_STOP_CONFIRMATION.chars() {
            assert_eq!(
                dashboard.handle_key(key(KeyCode::Char(character))),
                DashboardAction::None
            );
        }
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::ForceStop {
                session_id: "session-1".into()
            }
        );
    }

    #[test]
    fn typed_confirmation_updates_submit_eligibility_before_redraw() {
        let mut dashboard = running_dashboard_with_stop_dialog();
        dashboard.show_close_failure("session-1".into(), "archive unavailable");
        // The middle button opens the typed confirmation.
        dashboard.handle_key(key(KeyCode::Tab));
        dashboard.handle_key(key(KeyCode::Right));
        dashboard.handle_key(key(KeyCode::Enter));
        for character in FORCE_STOP_CONFIRMATION.chars() {
            assert_eq!(
                dashboard.handle_key(key(KeyCode::Char(character))),
                DashboardAction::None
            );
        }
        let Mode::Confirm(dialog) = &dashboard.mode else {
            panic!("expected typed confirmation");
        };
        assert_eq!(
            dialog.form.borrow().focused(),
            Some(DialogControl::TypedField)
        );
        // Tab sees the newly enabled standard buttons without a render pass.
        dashboard.handle_key(key(KeyCode::Tab));
        let Mode::Confirm(dialog) = &dashboard.mode else {
            panic!("expected typed confirmation");
        };
        assert_eq!(
            dialog.form.borrow().focused(),
            Some(DialogControl::ConfirmButton(0))
        );
        dashboard.handle_key(key(KeyCode::Tab));
        let Mode::Confirm(dialog) = &dashboard.mode else {
            panic!("expected typed confirmation");
        };
        assert_eq!(
            dialog.form.borrow().focused(),
            Some(DialogControl::ConfirmButton(1))
        );
        dashboard.handle_key(key(KeyCode::BackTab));
        dashboard.handle_key(key(KeyCode::BackTab));
        dashboard.handle_key(key(KeyCode::Backspace));
        dashboard.handle_key(key(KeyCode::Tab));
        assert_eq!(
            match &dashboard.mode {
                Mode::Confirm(dialog) => dialog.form.borrow().focused(),
                _ => None,
            },
            Some(DialogControl::ConfirmButton(0)),
            "the enabled cancel button remains reachable after invalidation"
        );
        dashboard.handle_key(key(KeyCode::Tab));
        let Mode::Confirm(dialog) = &dashboard.mode else {
            panic!("expected typed confirmation");
        };
        assert_eq!(
            dialog.form.borrow().focused(),
            Some(DialogControl::TypedField),
            "invalidating the text disables the submit control immediately"
        );
        dashboard.handle_key(key(KeyCode::Char('P')));
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::ForceStop {
                session_id: "session-1".into()
            }
        );
    }

    #[test]
    fn close_failure_cancel_button_closes_the_dialog_without_acting() {
        let mut session = stopped_session();
        session.state = SessionState::Running;
        let mut dashboard = dashboard_with_session(session);
        dashboard.show_close_failure("session-1".into(), "archive unavailable");

        // Tab from the rightmost button (Retry stop) wraps to Cancel.
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Tab)),
            DashboardAction::None
        );
        let Mode::Confirm(dialog) = &dashboard.mode else {
            panic!("expected close failure dialog");
        };
        assert_eq!(
            dialog.form.borrow().focused(),
            Some(DialogControl::ConfirmButton(0))
        );
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::None
        );
        assert!(matches!(dashboard.mode, Mode::Dashboard));
    }

    fn running_hex_session_dashboard() -> DashboardState {
        // Real session ids are lowercase hex; the force-destroy confirmation
        // gates on the id's first eight characters.
        let mut session = stopped_session();
        session.state = SessionState::Running;
        session.id = "0123456789abcdef0123456789abcdef".into();
        dashboard_with_session(session)
    }

    #[test]
    fn force_destroy_confirmation_requires_the_typed_short_id() {
        let mut dashboard = running_hex_session_dashboard();
        through_the_palette(&mut dashboard, "force destroy");
        assert!(matches!(
            dashboard.mode,
            Mode::Confirm(ConfirmDialog {
                confirmation: Confirmation::ForceDestroy { .. },
                ..
            })
        ));
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .unwrap();
        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(
            rendered.contains("FORCE DESTROY · THE SESSION AND ITS RECOVERY ARCHIVE WILL BE LOST")
        );
        assert!(rendered.contains("Type 01234567 (this session's short id), then press Enter:"));

        for character in "ffffffff".chars() {
            assert_eq!(
                dashboard.handle_key(key(KeyCode::Char(character))),
                DashboardAction::None
            );
        }
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::None,
            "a mismatched id must not destroy"
        );
        for _ in 0..8 {
            dashboard.handle_key(key(KeyCode::Backspace));
        }
        for character in "01234567".chars() {
            assert_eq!(
                dashboard.handle_key(key(KeyCode::Char(character))),
                DashboardAction::None
            );
        }
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::ForceDestroy {
                session_id: "0123456789abcdef0123456789abcdef".into()
            }
        );
    }

    #[test]
    fn force_destroy_paste_is_filtered_to_lowercase_hex_and_capped() {
        let mut dashboard = running_hex_session_dashboard();
        through_the_palette(&mut dashboard, "force destroy");
        dashboard.handle_paste("zz0123ABcD!");
        let Mode::Confirm(ConfirmDialog {
            confirmation: Confirmation::ForceDestroy { typed, .. },
            ..
        }) = &dashboard.mode
        else {
            panic!("expected force destroy dialog");
        };
        assert_eq!(typed.value(), "0123abcd");
    }

    fn running_dashboard_with_stop_dialog() -> DashboardState {
        let mut session = stopped_session();
        session.state = SessionState::Running;
        let mut dashboard = dashboard_with_session(session);
        open_stop_dialog(&mut dashboard);
        dashboard
    }

    #[test]
    fn stop_confirmation_focuses_the_primary_button_so_enter_stops() {
        let mut dashboard = running_dashboard_with_stop_dialog();
        let Mode::Confirm(dialog) = &dashboard.mode else {
            panic!("expected stop confirmation");
        };
        assert_eq!(
            confirmation_buttons(&dialog.confirmation),
            &["Cancel", "Stop"]
        );
        assert_eq!(
            dialog.form.borrow().focused(),
            Some(DialogControl::ConfirmButton(1))
        );
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::Close {
                session_id: "session-1".into()
            }
        );
        assert!(matches!(dashboard.mode, Mode::Dashboard));
    }

    #[test]
    fn stop_confirmation_cycles_focus_and_cancels_from_the_cancel_button() {
        for cycle_keys in [
            vec![KeyCode::Tab],
            vec![KeyCode::Right],
            vec![KeyCode::Left],
            vec![KeyCode::BackTab],
        ] {
            let mut dashboard = running_dashboard_with_stop_dialog();
            for cycle_key in &cycle_keys {
                assert_eq!(dashboard.handle_key(key(*cycle_key)), DashboardAction::None);
            }
            let Mode::Confirm(dialog) = &dashboard.mode else {
                panic!("expected stop confirmation to stay open for {cycle_keys:?}");
            };
            assert_eq!(
                dialog.form.borrow().focused(),
                Some(DialogControl::ConfirmButton(0)),
                "{cycle_keys:?}"
            );
            assert_eq!(
                dashboard.handle_key(key(KeyCode::Enter)),
                DashboardAction::None,
                "{cycle_keys:?}"
            );
            assert!(matches!(dashboard.mode, Mode::Dashboard), "{cycle_keys:?}");
        }
    }

    #[test]
    fn stop_confirmation_wraps_focus_back_to_the_primary_button() {
        let mut dashboard = running_dashboard_with_stop_dialog();
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Tab)),
            DashboardAction::None
        );
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Tab)),
            DashboardAction::None
        );
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::Close {
                session_id: "session-1".into()
            }
        );
    }

    #[test]
    fn stop_confirmation_escape_cancels_from_any_button() {
        for presses in 0..2 {
            let mut dashboard = running_dashboard_with_stop_dialog();
            for _ in 0..presses {
                dashboard.handle_key(key(KeyCode::Tab));
            }
            assert_eq!(
                dashboard.handle_key(key(KeyCode::Esc)),
                DashboardAction::None,
                "after {presses} focus moves"
            );
            assert!(matches!(dashboard.mode, Mode::Dashboard), "{presses}");
        }
    }

    #[test]
    fn stop_confirmation_ignores_the_removed_letter_accelerators() {
        for accelerator in ['y', 'Y', 'n', 'N'] {
            let mut dashboard = running_dashboard_with_stop_dialog();
            assert_eq!(
                dashboard.handle_key(key(KeyCode::Char(accelerator))),
                DashboardAction::None,
                "{accelerator}"
            );
            assert!(
                matches!(
                    dashboard.mode,
                    Mode::Confirm(ConfirmDialog {
                        confirmation: Confirmation::Close { .. },
                        ..
                    })
                ),
                "{accelerator}"
            );
        }
    }

    #[test]
    fn stop_confirmation_renders_only_cancel_and_stop_with_stop_focused() {
        let mut dashboard = running_dashboard_with_stop_dialog();
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).expect("terminal");
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw stop confirmation");
        let buffer = terminal.backend().buffer();
        let lines = buffer_lines(buffer);
        let row = lines
            .iter()
            .position(|line| line.contains(" Cancel ") && line.contains(" Stop "))
            .expect("button row");
        let button_y = buffer.area.y + row as u16;
        let cancel_x = buffer.area.x + cell_column(&lines[row], "Cancel");
        let stop_x = buffer.area.x + cell_column(&lines[row], "Stop");
        assert_eq!(buffer[(stop_x, button_y)].bg, Color::Cyan);
        assert_eq!(buffer[(cancel_x, button_y)].bg, Color::Reset);
        // Each label keeps its one-cell padding inside the button background.
        assert_eq!(buffer[(cancel_x - 1, button_y)].bg, Color::Reset);
        assert_eq!(buffer[(stop_x - 1, button_y)].bg, Color::Cyan);
        assert!(!lines.iter().any(|line| line.contains("Press y/Enter")));
    }

    #[test]
    fn stopping_an_active_turn_names_and_explains_the_interruption() {
        let mut session = stopped_session();
        session.state = SessionState::Running;
        let mut dashboard = dashboard_with_session(session);
        dashboard
            .session_details
            .get_mut("session-1")
            .expect("session detail")
            .current_turn_started_at = Some(1_000);

        open_stop_dialog(&mut dashboard);
        let Mode::Confirm(dialog) = &dashboard.mode else {
            panic!("expected stop confirmation");
        };
        let Confirmation::Close { active_turn, .. } = &dialog.confirmation else {
            panic!("expected close confirmation");
        };
        assert!(*active_turn);
        let (title, lines) = confirmation_body(&dialog.confirmation);
        assert_eq!(title, " Stop active session? ");
        assert!(
            lines.iter().any(|line| line.spans.iter().any(|span| {
                span.content
                    .contains("Mjolnir will interrupt the current turn")
            })),
            "active stop did not explain its interruption: {lines:?}"
        );
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::Close {
                session_id: "session-1".into()
            }
        );
    }

    #[test]
    fn stopping_a_session_warns_about_a_review_it_would_end() {
        let quiet = confirmation_lines(&Confirmation::Close {
            session_id: "session-1".into(),
            active_turn: false,
            reviewer_conversation: false,
        });
        assert!(
            !quiet.iter().any(|line| line.contains("second opinion")),
            "a session with no review says nothing about one: {quiet:?}"
        );

        let warned = confirmation_lines(&Confirmation::Close {
            session_id: "session-1".into(),
            active_turn: false,
            reviewer_conversation: true,
        });
        assert!(
            warned
                .iter()
                .any(|line| line.contains("cannot be continued after resume")),
            "stopping must warn that the review ends with the target: {warned:?}"
        );
        // The choice is still the ordinary one: stop anyway, or cancel.
        assert_eq!(
            confirmation_buttons(&Confirmation::Close {
                session_id: "session-1".into(),
                active_turn: false,
                reviewer_conversation: true,
            }),
            &["Cancel", "Stop"]
        );
    }

    /// The rendered body of one confirmation, as plain strings.
    fn confirmation_lines(confirmation: &Confirmation) -> Vec<String> {
        confirmation_body(confirmation)
            .1
            .into_iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn button_confirmations_keep_their_button_row_visible() {
        let confirmations = [
            Confirmation::Close {
                session_id: "session-1".into(),
                active_turn: false,
                reviewer_conversation: false,
            },
            // The warning adds rows, so the taller variant has to keep its
            // buttons on screen too.
            Confirmation::Close {
                session_id: "session-1".into(),
                active_turn: true,
                reviewer_conversation: true,
            },
            Confirmation::DestroyStopped {
                session_id: "session-1".into(),
                reopen: None,
            },
            Confirmation::CloseFailed {
                session_id: "session-1".into(),
                error: "archive unavailable".into(),
            },
            Confirmation::DirtyLocal {
                action: DashboardAction::None,
                repositories: vec!["/work/repo".into(), "/work/other".into()],
            },
            Confirmation::ForceStop {
                session_id: "session-1".into(),
                typed: TextInput::new(),
            },
            Confirmation::ForceDestroy {
                session_id: "session-1".into(),
                expected: "session-1".into(),
                typed: TextInput::new(),
            },
        ];
        for confirmation in confirmations {
            for (width, height) in [(120, 30), (100, 24), (72, 22)] {
                let mut dashboard = dashboard_with_session(stopped_session());
                dashboard.mode = Mode::Confirm(ConfirmDialog::new(confirmation.clone()));
                let mut terminal =
                    Terminal::new(TestBackend::new(width, height)).expect("terminal");
                terminal
                    .draw(|frame| render(frame, &mut dashboard))
                    .expect("draw confirmation");
                let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
                for label in confirmation_buttons(&confirmation) {
                    assert!(
                        rendered.contains(&format!(" {label} ")),
                        "{confirmation:?} at {width}x{height} hides {label}"
                    );
                }
            }
        }
    }

    #[test]
    fn destroy_stopped_confirmation_destroys_from_its_primary_button() {
        let mut dashboard = dashboard_with_session(stopped_session());
        dashboard.show_resume_dialog(1, Vec::new());
        dashboard.handle_key(key(KeyCode::Delete));
        let Mode::Confirm(dialog) = &dashboard.mode else {
            panic!("expected destroy confirmation");
        };
        assert_eq!(
            confirmation_buttons(&dialog.confirmation),
            &["Cancel", "Destroy"]
        );
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::DestroyStopped {
                session_id: "session-1".into()
            }
        );
        // Destroying from the dialog leaves the user in the dialog.
        assert!(matches!(dashboard.mode, Mode::ResumeDialog(_)));
    }

    #[test]
    fn dirty_local_confirmation_continues_or_cancels_from_its_buttons() {
        let create = |allow_dirty_local| DashboardAction::CreateSession {
            profile_id: "codex-1".into(),
            bundle_id: "hel".into(),
            project_directory: None,
            target_template_id: "podman".into(),
            additional_mounts: Vec::new(),
            allow_dirty_local,
            resource_allocation: None,
        };

        let mut dashboard = dashboard_with_session(stopped_session());
        dashboard.show_dirty_local_confirmation(create(false), vec!["project".into()]);
        assert_eq!(dashboard.handle_key(key(KeyCode::Enter)), create(true));
        assert!(matches!(dashboard.mode, Mode::Dashboard));

        let mut dashboard = dashboard_with_session(stopped_session());
        dashboard.show_dirty_local_confirmation(create(false), vec!["project".into()]);
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Char('y'))),
            DashboardAction::None
        );
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Tab)),
            DashboardAction::None
        );
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::None
        );
        assert!(matches!(dashboard.mode, Mode::Dashboard));
    }

    #[test]
    fn missing_checkpoint_history_dialog_makes_the_source_field_visible() {
        let mut dashboard = dashboard_with_session(stopped_session());
        dashboard.show_repository_origin_dialog(
            "session-1".into(),
            "bifrost".into(),
            "b41dc78".into(),
            "https://github.com/BrokkAi/bifrost.git".into(),
            "BrokkAi/bifrost-dev".into(),
            DashboardAction::None,
        );
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).expect("terminal");
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw repository origin dialog");

        let cursor_position = terminal.get_cursor_position().expect("source cursor");
        let buffer = terminal.backend().buffer();
        let lines = buffer_lines(buffer);
        let source_row = lines
            .iter()
            .position(|line| line.contains("Source:"))
            .expect("focused source field");
        let source_y = buffer.area.y + source_row as u16;
        let source_x = buffer.area.x + cell_column(&lines[source_row], "Source:");
        let field_x = source_x + 8;
        assert_eq!(buffer[(field_x, source_y)].bg, Color::Cyan);
        assert_eq!(
            cursor_position,
            Position {
                x: field_x,
                y: source_y,
            }
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("Type or paste into Source"))
        );

        let button_row = lines
            .iter()
            .position(|line| line.contains(" Cancel ") && line.contains(" Check origin "))
            .expect("button row");
        let button_y = buffer.area.y + button_row as u16;
        let cancel_x = buffer.area.x + cell_column(&lines[button_row], "Cancel");
        let check_x = buffer.area.x + cell_column(&lines[button_row], "Check origin");
        assert_eq!(buffer[(cancel_x, button_y)].bg, Color::Reset);
        assert_eq!(buffer[(check_x, button_y)].bg, Color::Reset);

        dashboard.handle_key(key(KeyCode::Tab));
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw repository origin dialog with cancel focused");
        let buffer = terminal.backend().buffer();
        let lines = buffer_lines(buffer);
        let button_row = lines
            .iter()
            .position(|line| line.contains(" Cancel ") && line.contains(" Check origin "))
            .expect("button row");
        let button_y = buffer.area.y + button_row as u16;
        let cancel_x = buffer.area.x + cell_column(&lines[button_row], "Cancel");
        assert_eq!(buffer[(cancel_x, button_y)].bg, Color::Cyan);
        assert_eq!(buffer[(field_x, source_y)].bg, Color::Reset);
    }

    #[test]
    fn missing_checkpoint_history_dialog_accepts_a_replacement_origin() {
        let launch = DashboardAction::ResumeSession {
            session_id: "session-1".into(),
            profile_id: "codex-1".into(),
            target_template_id: "podman".into(),
            additional_mounts: Vec::new(),
            resource_allocation: None,
            discard_queue: false,
        };
        let mut dashboard = dashboard_with_session(stopped_session());
        dashboard.show_repository_origin_dialog(
            "session-1".into(),
            "bifrost".into(),
            "b41dc78".into(),
            "https://github.com/BrokkAi/bifrost.git".into(),
            "BrokkAi/bifrost".into(),
            launch.clone(),
        );
        dashboard.handle_paste("BrokkAi/bifrost-dev\n");

        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::ReplaceResumeRepositoryOrigin {
                session_id: "session-1".into(),
                repository_id: "bifrost".into(),
                replacement: "BrokkAi/bifrost-dev".into(),
                launch: Box::new(launch),
            }
        );
        let Mode::RepositoryOrigin(dialog) = &dashboard.mode else {
            panic!("expected repository origin dialog");
        };
        assert_eq!(dialog.missing_commit, "b41dc78");

        dashboard.apply_repository_origin_failure(
            "bifrost",
            "That origin does not contain checkpoint base b41dc78.".into(),
        );
        let Mode::RepositoryOrigin(dialog) = &dashboard.mode else {
            panic!("expected repository origin dialog");
        };
        assert_eq!(dialog.form.borrow().focused(), Some(DialogControl::Field));
        assert_eq!(
            dialog.error.as_deref(),
            Some("That origin does not contain checkpoint base b41dc78.")
        );
    }
}
