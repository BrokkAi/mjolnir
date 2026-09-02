//! Modal dialogs: session import, confirmations, and the rename editor.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use qrcode::QrCode;
use qrcode::types::{Color as QrColor, EcLevel};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use std::path::PathBuf;

use hel::hel_config::{HarnessKind, mount_history_host};
use hel::hel_selection::FrameSurfaces;
use hel::hel_targets::{AdditionalMount, default_mount_destination, validate_additional_mounts};
use hel::hel_text_input::TextInput;

use crate::widgets::{
    action_buttons, centered_modal, centered_modal_fixed, focused_buttons, modal_area,
    popup_height, truncate_text,
};
use crate::wizards::read_only_marker;
use crate::{
    ButtonKey, DashboardAction, DashboardState, Mode, WebViewerAccess, button_row_key,
    cycle_button_focus, cycle_control, move_index,
};

pub(crate) const FORCE_STOP_CONFIRMATION: &str = "STOP";

const IMPORT_STALL_WARNING_AFTER: Duration = Duration::from_secs(10);

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
    pub(crate) focus: RenameFocus,
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
    pub(crate) focus: RenameFocus,
    pub(crate) return_to_targets: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TargetActionsDialog {
    pub(crate) target_ids: Vec<String>,
    pub(crate) target_index: usize,
    pub(crate) focus: usize,
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
        }
    }
}

const TARGET_ACTION_BUTTONS: &[&str] = &["Rename", "Test", "Close"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepositoryOriginDialog {
    pub(crate) session_id: String,
    pub(crate) repository_id: String,
    pub(crate) missing_commit: String,
    pub(crate) archived_origin: String,
    pub(crate) configured_origin: String,
    pub(crate) replacement: TextInput,
    pub(crate) error: Option<String>,
    pub(crate) focus: RepositoryOriginFocus,
    pub(crate) launch: Box<DashboardAction>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RepositoryOriginFocus {
    Field,
    Cancel,
    Validate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RenameFocus {
    Field,
    Cancel,
    Save,
}

const RENAME_BUTTONS: &[&str] = &["Cancel", "Save"];

const RENAME_FOCUS_ORDER: [RenameFocus; 3] =
    [RenameFocus::Field, RenameFocus::Cancel, RenameFocus::Save];

impl RenameFocus {
    /// The button Enter would press. Typing in the field also submits, so Save
    /// stays highlighted there and exactly one button is ever highlighted.
    fn button_index(self) -> usize {
        match self {
            RenameFocus::Cancel => 0,
            RenameFocus::Field | RenameFocus::Save => 1,
        }
    }

    fn from_button_index(index: usize) -> Self {
        if index == 0 {
            RenameFocus::Cancel
        } else {
            RenameFocus::Save
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Confirmation {
    DirtyLocal {
        action: DashboardAction,
        repositories: Vec<String>,
    },
    Close {
        session_id: String,
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

/// A confirmation dialog plus the index of its focused button.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfirmDialog {
    pub(crate) confirmation: Confirmation,
    pub(crate) focus: usize,
}

impl ConfirmDialog {
    pub(crate) fn new(confirmation: Confirmation) -> Self {
        let focus = primary_button(confirmation_buttons(&confirmation));
        Self {
            confirmation,
            focus,
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImportBundleConfirmation {
    dirty_git_roots: Vec<String>,
    omitted_non_git_dirs: Vec<String>,
    scratch_git_roots: Vec<String>,
    has_untracked_files: bool,
    ignore_untracked: bool,
    pub(crate) focus: usize,
}

const IMPORT_BUNDLE_BUTTONS: &[&str] = &["Cancel", "Continue"];

const IMPORT_PROGRESS_BUTTONS: &[&str] = &["Cancel"];

/// Button labels for a confirmation dialog, ordered Cancel first and the primary
/// action last. Typed-confirmation dialogs have no buttons. This is the single
/// declaration used by both key handling and rendering.
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
        Confirmation::ForceStop { .. } => &[],
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
        focused_buttons(IMPORT_PROGRESS_BUTTONS, 0),
    ])
    .block(Block::default().borders(Borders::ALL).title(format!(
        " Importing session · progress {}/{total} ",
        progress.step
    )))
    // `trim: false` keeps the padding inside the leftmost button background.
    .wrap(Wrap { trim: false });
    let popup = centered_modal(surfaces, 76, popup_height(&paragraph, 76, 10, area), area);
    frame.render_widget(Clear, popup);
    frame.render_widget(paragraph, popup);
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
        if confirmation.has_untracked_files {
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                format!(
                    "{} Ignore untracked files",
                    if confirmation.ignore_untracked {
                        "[x]"
                    } else {
                        "[ ]"
                    }
                ),
                Style::default().fg(Color::Cyan),
            ));
        }
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
        lines.push(Line::raw(""));
    }
    lines.push(focused_buttons(IMPORT_BUNDLE_BUTTONS, confirmation.focus));
    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Import safety warning "),
        )
        // `trim: false` keeps the padding inside the leftmost button background.
        .wrap(Wrap { trim: false });
    let popup = centered_modal(surfaces, 76, popup_height(&paragraph, 76, 12, area), area);
    frame.render_widget(Clear, popup);
    frame.render_widget(paragraph, popup);
}

/// Editable per-session container provisioning inputs: the size overrides and
/// the attached host directories. Nothing here is written to config.toml.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ContainerEditor {
    pub(crate) session_id: String,
    pub(crate) cpus: TextInput,
    pub(crate) memory: TextInput,
    pub(crate) mounts: Vec<AdditionalMount>,
    /// Remembered mount sources for this session's host, offered as
    /// suggestions and editable so a stale directory can be forgotten.
    pub(crate) suggestions: Vec<PathBuf>,
    pub(crate) source: TextInput,
    pub(crate) destination: TextInput,
    /// Read-only setting for the directory being typed, carried into the list
    /// when it is attached.
    pub(crate) read_only: bool,
    pub(crate) focus: ContainerEditFocus,
    pub(crate) mount_index: usize,
    pub(crate) suggestion_index: usize,
    pub(crate) error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContainerEditFocus {
    Cpus,
    Memory,
    Source,
    Destination,
    ReadOnly,
    Mounts,
    Suggestions,
    Cancel,
    Save,
}

const CONTAINER_EDIT_BUTTONS: &[&str] = &["Cancel", "Save"];

/// One line, so the dialog never implies a live resize.
pub(crate) const CONTAINER_EDIT_SCOPE: &str = "Applies when the container is next recreated.";

impl ContainerEditor {
    /// Focus order, skipping the lists that have nothing to select.
    fn focus_order(&self) -> Vec<ContainerEditFocus> {
        let mut order = vec![
            ContainerEditFocus::Cpus,
            ContainerEditFocus::Memory,
            ContainerEditFocus::Source,
            ContainerEditFocus::Destination,
            ContainerEditFocus::ReadOnly,
        ];
        if !self.mounts.is_empty() {
            order.push(ContainerEditFocus::Mounts);
        }
        if !self.suggestions.is_empty() {
            order.push(ContainerEditFocus::Suggestions);
        }
        order.extend([ContainerEditFocus::Cancel, ContainerEditFocus::Save]);
        order
    }

    fn field_mut(&mut self) -> Option<&mut TextInput> {
        match self.focus {
            ContainerEditFocus::Cpus => Some(&mut self.cpus),
            ContainerEditFocus::Memory => Some(&mut self.memory),
            ContainerEditFocus::Source => Some(&mut self.source),
            ContainerEditFocus::Destination => Some(&mut self.destination),
            ContainerEditFocus::ReadOnly
            | ContainerEditFocus::Mounts
            | ContainerEditFocus::Suggestions
            | ContainerEditFocus::Cancel
            | ContainerEditFocus::Save => None,
        }
    }

    pub(crate) fn field(&self) -> Option<&TextInput> {
        match self.focus {
            ContainerEditFocus::Cpus => Some(&self.cpus),
            ContainerEditFocus::Memory => Some(&self.memory),
            ContainerEditFocus::Source => Some(&self.source),
            ContainerEditFocus::Destination => Some(&self.destination),
            _ => None,
        }
    }

    fn button_index(&self) -> usize {
        match self.focus {
            ContainerEditFocus::Cancel => 0,
            _ => 1,
        }
    }

    /// Add the typed mount, filling in a default destination. Returns the
    /// reason it was rejected, if it was.
    fn add_mount(&mut self) -> Option<String> {
        let source = PathBuf::from(self.source.trim());
        if source.as_os_str().is_empty() {
            return Some("Enter a host directory to attach.".into());
        }
        let destination = if self.destination.trim().is_empty() {
            default_mount_destination(&source, &self.mounts)
        } else {
            PathBuf::from(self.destination.trim())
        };
        let mount = AdditionalMount {
            source,
            destination,
            read_only: self.read_only,
        };
        let mut mounts = self.mounts.clone();
        mounts.push(mount);
        if let Err(error) = validate_additional_mounts(&mounts) {
            return Some(error.to_string());
        }
        self.mounts = mounts;
        self.source.clear();
        self.destination.clear();
        self.read_only = false;
        self.mount_index = self.mounts.len() - 1;
        None
    }

    /// Toggle read-only for the entry being typed, or for the selected row.
    fn toggle_read_only(&mut self) {
        match self.focus {
            ContainerEditFocus::ReadOnly => self.read_only = !self.read_only,
            ContainerEditFocus::Mounts => {
                if let Some(mount) = self.mounts.get_mut(self.mount_index) {
                    mount.read_only = !mount.read_only;
                }
            }
            _ => {}
        }
    }

    fn take_suggestion(&mut self) {
        let Some(source) = self.suggestions.get(self.suggestion_index) else {
            return;
        };
        self.source = source.to_string_lossy().into_owned().into();
        self.destination = default_mount_destination(source, &self.mounts)
            .to_string_lossy()
            .into_owned()
            .into();
        self.focus = ContainerEditFocus::Source;
    }

    fn remove_selected(&mut self) {
        match self.focus {
            ContainerEditFocus::Mounts if !self.mounts.is_empty() => {
                self.mounts.remove(self.mount_index);
                self.mount_index = self.mount_index.min(self.mounts.len().saturating_sub(1));
                if self.mounts.is_empty() {
                    self.focus = ContainerEditFocus::Source;
                }
            }
            ContainerEditFocus::Suggestions if !self.suggestions.is_empty() => {
                self.suggestions.remove(self.suggestion_index);
                self.suggestion_index = self
                    .suggestion_index
                    .min(self.suggestions.len().saturating_sub(1));
                if self.suggestions.is_empty() {
                    self.focus = ContainerEditFocus::Source;
                }
            }
            _ => {}
        }
    }

    fn save(&self) -> Result<DashboardAction, String> {
        validate_additional_mounts(&self.mounts).map_err(|error| error.to_string())?;
        let value = |text: &str| {
            let trimmed = text.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_owned())
        };
        Ok(DashboardAction::SaveContainerSettings {
            session_id: self.session_id.clone(),
            cpus: value(&self.cpus),
            memory: value(&self.memory),
            additional_mounts: self.mounts.clone(),
            mount_history: self.suggestions.clone(),
        })
    }
}

pub(crate) fn render_container_editor(
    frame: &mut Frame,
    area: Rect,
    editor: &ContainerEditor,
    surfaces: &mut FrameSurfaces,
) {
    let field = |label: &str, value: &TextInput, focused: bool| {
        let style = if focused {
            Style::default().fg(Color::Black).bg(Color::Cyan)
        } else {
            Style::default().fg(Color::Cyan)
        };
        Line::from(vec![
            ratatui::text::Span::raw(format!("{label}: ")),
            ratatui::text::Span::styled(
                format!(
                    "{} ",
                    if focused {
                        value.with_cursor_marker("▏")
                    } else {
                        value.to_string()
                    }
                ),
                style,
            ),
        ])
    };
    let mut lines = vec![
        Line::raw(format!("Session: {}", editor.session_id)),
        Line::styled(CONTAINER_EDIT_SCOPE, Style::default().fg(Color::DarkGray)),
        Line::raw(""),
        field(
            "CPUs",
            &editor.cpus,
            editor.focus == ContainerEditFocus::Cpus,
        ),
        field(
            "Memory",
            &editor.memory,
            editor.focus == ContainerEditFocus::Memory,
        ),
        Line::styled(
            "Empty keeps the target's value.",
            Style::default().fg(Color::DarkGray),
        ),
        Line::raw(""),
        Line::raw("Attached directories"),
    ];
    if editor.mounts.is_empty() {
        lines.push(Line::styled("  none", Style::default().fg(Color::DarkGray)));
    }
    for (index, mount) in editor.mounts.iter().enumerate() {
        let selected = editor.focus == ContainerEditFocus::Mounts && index == editor.mount_index;
        lines.push(Line::styled(
            format!(
                "{} {} -> {}{}",
                if selected { "›" } else { " " },
                mount.source.display(),
                mount.destination.display(),
                read_only_marker(mount.read_only)
            ),
            if selected {
                Style::default().fg(Color::Black).bg(Color::Cyan)
            } else {
                Style::default()
            },
        ));
    }
    lines.extend([
        Line::raw(""),
        field(
            "Attach host directory",
            &editor.source,
            editor.focus == ContainerEditFocus::Source,
        ),
        field(
            "Container destination",
            &editor.destination,
            editor.focus == ContainerEditFocus::Destination,
        ),
        field(
            "Read-only",
            &TextInput::from(if editor.read_only { "[x]" } else { "[ ]" }),
            editor.focus == ContainerEditFocus::ReadOnly,
        ),
    ]);
    if !editor.suggestions.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::raw("Remembered directories"));
        for (index, source) in editor.suggestions.iter().enumerate() {
            let selected =
                editor.focus == ContainerEditFocus::Suggestions && index == editor.suggestion_index;
            lines.push(Line::styled(
                format!("{} {}", if selected { "›" } else { " " }, source.display()),
                if selected {
                    Style::default().fg(Color::Black).bg(Color::Cyan)
                } else {
                    Style::default()
                },
            ));
        }
    }
    if let Some(error) = &editor.error {
        lines.push(Line::styled(
            error.clone(),
            Style::default().fg(Color::Yellow),
        ));
    }
    lines.extend([
        Line::raw(""),
        Line::styled(
            "Enter attaches or takes the selected row · Space toggles read-only · d forgets it · \
             Tab moves",
            Style::default().fg(Color::DarkGray),
        ),
        focused_buttons(CONTAINER_EDIT_BUTTONS, editor.button_index()),
    ]);
    let paragraph = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Edit container size and mounts "),
    );
    let popup = centered_modal(surfaces, 70, popup_height(&paragraph, 70, 18, area), area);
    frame.render_widget(Clear, popup);
    frame.render_widget(paragraph, popup);
}

pub(crate) fn render_rename_editor(
    frame: &mut Frame,
    area: Rect,
    editor: &RenameEditor,
    surfaces: &mut FrameSurfaces,
) {
    let paragraph = Paragraph::new(vec![
        Line::raw(format!("Session: {}", editor.session_id)),
        Line::raw(""),
        Line::styled(
            if editor.focus == RenameFocus::Field {
                editor.title.with_cursor_marker("▏")
            } else {
                editor.title.to_string()
            },
            Style::default().fg(Color::Cyan),
        ),
        Line::raw(""),
        focused_buttons(RENAME_BUTTONS, editor.focus.button_index()),
    ])
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Rename session "),
    );
    let popup = centered_modal(surfaces, 60, popup_height(&paragraph, 60, 8, area), area);
    frame.render_widget(Clear, popup);
    frame.render_widget(paragraph, popup);
}

pub(crate) fn render_config_id_editor(
    frame: &mut Frame,
    area: Rect,
    editor: &ConfigIdEditor,
    surfaces: &mut FrameSurfaces,
) {
    let paragraph = Paragraph::new(vec![
        Line::raw(format!(
            "Current {} ID: {}",
            editor.kind.label(),
            editor.old_id
        )),
        Line::raw(""),
        Line::styled(
            if editor.focus == RenameFocus::Field {
                editor.value.with_cursor_marker("▏")
            } else {
                editor.value.to_string()
            },
            Style::default().fg(Color::Cyan),
        ),
        Line::raw(""),
        focused_buttons(RENAME_BUTTONS, editor.focus.button_index()),
    ])
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" Rename {} ID ", editor.kind.label())),
    );
    let popup = centered_modal(surfaces, 60, popup_height(&paragraph, 60, 8, area), area);
    frame.render_widget(Clear, popup);
    frame.render_widget(paragraph, popup);
}

pub(crate) fn render_target_actions(
    frame: &mut Frame,
    area: Rect,
    dashboard: &DashboardState,
    dialog: &TargetActionsDialog,
    surfaces: &mut FrameSurfaces,
) {
    let mut lines = dialog
        .target_ids
        .iter()
        .enumerate()
        .map(|(index, id)| {
            let kind = dashboard
                .config
                .targets
                .get(id)
                .map(target_kind_label)
                .unwrap_or("missing");
            Line::styled(
                format!(
                    "{} {id:<24} {kind}",
                    if index == dialog.target_index {
                        '›'
                    } else {
                        ' '
                    }
                ),
                if index == dialog.target_index {
                    Style::default().bg(Color::DarkGray).fg(Color::White)
                } else {
                    Style::default()
                },
            )
        })
        .collect::<Vec<_>>();
    if lines.is_empty() {
        lines.push(Line::raw("No targets configured."));
    }
    lines.push(Line::raw(""));
    if let Some(target_id) = &dialog.testing {
        lines.push(Line::styled(
            format!("Testing {target_id}… Alt-X cancels test"),
            Style::default().fg(Color::Yellow),
        ));
    } else if let Some((target_id, result)) = &dialog.result {
        lines.push(Line::styled(
            match result {
                Ok(()) => format!("{target_id}: ready"),
                Err(error) => format!("{target_id}: {error}"),
            },
            Style::default().fg(if result.is_ok() {
                Color::Green
            } else {
                Color::Yellow
            }),
        ));
    }
    lines.push(Line::raw(""));
    lines.push(focused_buttons(TARGET_ACTION_BUTTONS, dialog.focus));
    lines.push(Line::styled(
        "Up/Down selects target · Tab selects action · Esc closes",
        Style::default().fg(Color::DarkGray),
    ));
    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Target actions "),
        )
        .wrap(Wrap { trim: false });
    let popup = centered_modal(surfaces, 72, popup_height(&paragraph, 72, 12, area), area);
    frame.render_widget(Clear, popup);
    frame.render_widget(paragraph, popup);
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
    }
}

pub(crate) fn render_web_dialog(
    frame: &mut Frame,
    area: Rect,
    dialog: &WebDialog,
    surfaces: &mut FrameSurfaces,
) {
    const FOOTER: &str = "Enter or Esc closes";
    // Text that names the natural body width. The box hugs the QR, and longer
    // URLs wrap beneath it rather than stretching the dialog across the screen.
    const MIN_INNER_WIDTH: usize = FOOTER.len();

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
    lines.push(Line::styled(FOOTER, Style::default().fg(Color::DarkGray)).centered());

    let inner_width = inner_width.min(max_inner).max(1);
    let box_width = u16::try_from(inner_width + 2).unwrap_or(u16::MAX);
    let paragraph = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(" Web viewer "))
        .wrap(Wrap { trim: false });
    let wrapped =
        u16::try_from(paragraph.line_count(box_width.saturating_sub(2))).unwrap_or(u16::MAX);
    let box_height = wrapped.saturating_add(2).min(inner_area.height);
    let popup = centered_modal_fixed(surfaces, box_width, box_height, area);
    frame.render_widget(Clear, popup);
    frame.render_widget(paragraph, popup);
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
    let field_focused = dialog.focus == RepositoryOriginFocus::Field;
    let field_style = if field_focused {
        Style::default().fg(Color::Black).bg(Color::Cyan)
    } else {
        Style::default().fg(Color::Cyan)
    };
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
        Line::from(vec![
            Span::raw("Source: "),
            Span::styled(
                format!(
                    " {} ",
                    if field_focused {
                        dialog.replacement.with_cursor_marker("▏")
                    } else {
                        dialog.replacement.to_string()
                    }
                ),
                field_style,
            ),
        ]),
    ];
    if let Some(error) = &dialog.error {
        lines.push(Line::styled(
            error.clone(),
            Style::default().fg(Color::Yellow),
        ));
    }
    lines.extend([
        Line::raw(""),
        Line::styled(
            "Type or paste into Source · Tab moves · Enter checks",
            Style::default().fg(Color::DarkGray),
        ),
        action_buttons(&[
            ("Cancel", dialog.focus == RepositoryOriginFocus::Cancel),
            (
                "Check origin",
                dialog.focus == RepositoryOriginFocus::Validate,
            ),
        ]),
    ]);
    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Repository history is missing "),
        )
        .wrap(Wrap { trim: false });
    let popup = centered_modal(surfaces, 76, popup_height(&paragraph, 76, 14, area), area);
    frame.render_widget(Clear, popup);
    frame.render_widget(paragraph, popup);
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
            reviewer_conversation,
        } => {
            let mut lines = vec![
                Line::raw(format!("Session: {session_id}")),
                Line::raw(""),
                Line::raw("Mjolnir will verify a recovery copy before destroying the target."),
            ];
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
            (" Stop session? ", lines)
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
        Confirmation::ForceStop { session_id, typed } => (
            " FORCE STOP · RECENT WORK MAY BE LOST ",
            vec![
                Line::raw(format!("Session: {session_id}")),
                Line::raw(""),
                Line::raw("The current target will be removed without a new checkpoint."),
                Line::raw("You can resume from the latest verified recovery archive."),
                Line::raw(format!("Type {FORCE_STOP_CONFIRMATION}, then press Enter:")),
                Line::styled(
                    typed.with_cursor_marker("▏"),
                    Style::default().fg(Color::Red),
                ),
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
    let nominal = match confirmation {
        Confirmation::DirtyLocal { .. } => 11,
        Confirmation::CloseFailed { .. } => 12,
        Confirmation::Close {
            reviewer_conversation: true,
            ..
        } => 13,
        Confirmation::Close { .. } | Confirmation::DestroyStopped { .. } => 10,
        Confirmation::RecoverFailed { .. } => 12,
        Confirmation::ForceStop { .. } => 10,
    };
    let (title, mut lines) = confirmation_body(confirmation);
    let buttons = confirmation_buttons(confirmation);
    if !buttons.is_empty() {
        lines.push(Line::raw(""));
        lines.push(focused_buttons(buttons, dialog.focus));
    }
    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Red))
                .title(title),
        )
        // `trim: false` keeps the padding inside the leftmost button background.
        .wrap(Wrap { trim: false });
    let popup = centered_modal(
        surfaces,
        72,
        popup_height(&paragraph, 72, nominal, area),
        area,
    );
    frame.render_widget(Clear, popup);
    frame.render_widget(paragraph, popup);
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
                }
            }
            WebViewerAccess::Unavailable(message) => WebDialog {
                loading: false,
                viewer_url: None,
                viewer_code: None,
                fallback_reason: None,
                message: Some(message),
                qr: None,
            },
        };
        if matches!(self.mode, Mode::Web(_)) {
            self.mode = Mode::Web(dialog);
        }
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
            focus: RenameFocus::Field,
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
        self.mode = Mode::TargetActions(TargetActionsDialog {
            target_ids,
            target_index,
            focus: 0,
            testing: None,
            result: None,
        });
    }

    pub(crate) fn handle_target_actions_key(
        &mut self,
        key: KeyEvent,
        mut dialog: TargetActionsDialog,
    ) -> DashboardAction {
        // Alt-X is the surface's one cancel chord. The controller's chord
        // pre-filter deliberately leaves it alone while a dialog is open, so
        // here it cancels the test this dialog is running.
        if dialog.testing.is_some()
            && key.modifiers.contains(KeyModifiers::ALT)
            && key.code == KeyCode::Char('x')
        {
            dialog.testing = None;
            dialog.result = Some(("Target test".into(), Err("cancelled".into())));
            self.mode = Mode::TargetActions(dialog);
            return DashboardAction::CancelTargetTest;
        }
        match key.code {
            KeyCode::Esc => {
                self.cancel_modal();
                return DashboardAction::None;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                move_index(&mut dialog.target_index, dialog.target_ids.len(), -1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                move_index(&mut dialog.target_index, dialog.target_ids.len(), 1);
            }
            KeyCode::Tab | KeyCode::Right => {
                dialog.focus = cycle_button_focus(dialog.focus, TARGET_ACTION_BUTTONS.len(), false);
            }
            KeyCode::BackTab | KeyCode::Left => {
                dialog.focus = cycle_button_focus(dialog.focus, TARGET_ACTION_BUTTONS.len(), true);
            }
            KeyCode::Enter => {
                let Some(target_id) = dialog.target_ids.get(dialog.target_index).cloned() else {
                    self.cancel_modal();
                    return DashboardAction::None;
                };
                match dialog.focus {
                    0 => {
                        self.mode = Mode::ConfigId(ConfigIdEditor {
                            kind: ConfigEntryKind::Target,
                            value: TextInput::from_value(target_id.clone()).with_max_chars(64),
                            old_id: target_id,
                            focus: RenameFocus::Field,
                            return_to_targets: true,
                        });
                        return DashboardAction::None;
                    }
                    1 if dialog.testing.is_none() => {
                        dialog.testing = Some(target_id.clone());
                        dialog.result = None;
                        self.mode = Mode::TargetActions(dialog);
                        return DashboardAction::TestTarget { target_id };
                    }
                    2 => {
                        self.cancel_modal();
                        return DashboardAction::None;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        self.mode = Mode::TargetActions(dialog);
        DashboardAction::None
    }

    pub(crate) fn handle_config_id_key(
        &mut self,
        key: KeyEvent,
        mut editor: ConfigIdEditor,
    ) -> DashboardAction {
        match key.code {
            KeyCode::Esc => {
                if editor.return_to_targets {
                    self.begin_target_actions();
                } else {
                    self.cancel_modal();
                }
                DashboardAction::None
            }
            KeyCode::Enter if editor.focus == RenameFocus::Cancel => {
                if editor.return_to_targets {
                    self.begin_target_actions();
                } else {
                    self.cancel_modal();
                }
                DashboardAction::None
            }
            KeyCode::Tab | KeyCode::BackTab => {
                editor.focus = cycle_control(
                    editor.focus,
                    &RENAME_FOCUS_ORDER,
                    key.code == KeyCode::BackTab,
                );
                self.mode = Mode::ConfigId(editor);
                DashboardAction::None
            }
            KeyCode::Left | KeyCode::Right if editor.focus != RenameFocus::Field => {
                editor.focus = RenameFocus::from_button_index(cycle_button_focus(
                    editor.focus.button_index(),
                    RENAME_BUTTONS.len(),
                    key.code == KeyCode::Left,
                ));
                self.mode = Mode::ConfigId(editor);
                DashboardAction::None
            }
            KeyCode::Enter if editor.value.trim().is_empty() => {
                self.notices.set("Configuration ID cannot be empty.");
                self.mode = Mode::ConfigId(editor);
                DashboardAction::None
            }
            KeyCode::Enter => {
                self.cancel_modal();
                match editor.kind {
                    ConfigEntryKind::Profile => DashboardAction::RenameProfile {
                        old_id: editor.old_id,
                        new_id: editor.value.into_value(),
                    },
                    ConfigEntryKind::Target => DashboardAction::RenameTarget {
                        old_id: editor.old_id,
                        new_id: editor.value.into_value(),
                    },
                }
            }
            _ if editor.focus == RenameFocus::Field => {
                editor.value.handle_key(key);
                self.mode = Mode::ConfigId(editor);
                DashboardAction::None
            }
            _ => {
                self.mode = Mode::ConfigId(editor);
                DashboardAction::None
            }
        }
    }

    pub fn apply_target_test(&mut self, target_id: String, result: Result<(), String>) {
        if let Mode::TargetActions(dialog) = &mut self.mode
            && dialog.testing.as_deref() == Some(&target_id)
        {
            dialog.testing = None;
            dialog.result = Some((target_id, result));
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
            focus: RepositoryOriginFocus::Field,
            launch: Box::new(launch),
        });
    }

    pub fn apply_repository_origin_failure(&mut self, repository_id: &str, error: String) {
        if let Mode::RepositoryOrigin(dialog) = &mut self.mode
            && dialog.repository_id == repository_id
        {
            dialog.error = Some(error);
            dialog.focus = RepositoryOriginFocus::Field;
        }
    }

    pub fn finish_resume_repository_preflight(&mut self) {
        self.cancel_modal();
    }

    pub(crate) fn handle_repository_origin_key(
        &mut self,
        key: KeyEvent,
        mut dialog: RepositoryOriginDialog,
    ) -> DashboardAction {
        let code = key.code;
        match code {
            KeyCode::Esc => {
                self.cancel_modal();
                return DashboardAction::None;
            }
            KeyCode::Tab | KeyCode::BackTab => {
                dialog.focus = cycle_control(
                    dialog.focus,
                    &[
                        RepositoryOriginFocus::Field,
                        RepositoryOriginFocus::Cancel,
                        RepositoryOriginFocus::Validate,
                    ],
                    code == KeyCode::BackTab,
                );
            }
            KeyCode::Enter if dialog.focus == RepositoryOriginFocus::Cancel => {
                self.cancel_modal();
                return DashboardAction::None;
            }
            KeyCode::Enter
                if matches!(
                    dialog.focus,
                    RepositoryOriginFocus::Field | RepositoryOriginFocus::Validate
                ) =>
            {
                if dialog.replacement.trim().is_empty() {
                    dialog.error = Some("Enter the repository's new origin.".into());
                    dialog.focus = RepositoryOriginFocus::Field;
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
            _ if dialog.focus == RepositoryOriginFocus::Field
                && dialog.replacement.handle_key(key).changed() =>
            {
                dialog.error = None;
            }
            _ => {}
        }
        self.mode = Mode::RepositoryOrigin(dialog);
        DashboardAction::None
    }

    /// Open the container editor for the selected session, if that session
    /// runs on a container-backed target.
    pub(crate) fn begin_container_edit(&mut self) {
        let Some(session) = self.selected_container_session() else {
            self.notices
                .set("Container size and mounts apply to container targets only.");
            return;
        };
        let suggestions = self
            .config
            .targets
            .get(&session.target_template_id)
            .and_then(mount_history_host)
            .and_then(|host| self.state.mount_history.get(host))
            .cloned()
            .unwrap_or_default();
        self.mode = Mode::EditContainer(ContainerEditor {
            session_id: session.id.clone(),
            cpus: session.container_cpus.clone().unwrap_or_default().into(),
            memory: session.container_memory.clone().unwrap_or_default().into(),
            mounts: session.additional_mounts.clone(),
            suggestions,
            source: TextInput::new(),
            destination: TextInput::new(),
            read_only: false,
            focus: ContainerEditFocus::Cpus,
            mount_index: 0,
            suggestion_index: 0,
            error: None,
        });
    }

    pub(crate) fn handle_container_edit_key(
        &mut self,
        key: KeyEvent,
        mut editor: ContainerEditor,
    ) -> DashboardAction {
        let code = key.code;
        let action = match code {
            KeyCode::Esc => {
                self.cancel_modal();
                return DashboardAction::None;
            }
            KeyCode::Tab | KeyCode::BackTab => {
                editor.focus = cycle_control(
                    editor.focus,
                    &editor.focus_order(),
                    code == KeyCode::BackTab,
                );
                DashboardAction::None
            }
            KeyCode::Up | KeyCode::Down => {
                let reverse = code == KeyCode::Up;
                match editor.focus {
                    ContainerEditFocus::Mounts => move_index(
                        &mut editor.mount_index,
                        editor.mounts.len(),
                        if reverse { -1 } else { 1 },
                    ),
                    ContainerEditFocus::Suggestions => move_index(
                        &mut editor.suggestion_index,
                        editor.suggestions.len(),
                        if reverse { -1 } else { 1 },
                    ),
                    _ => {
                        editor.focus = cycle_control(editor.focus, &editor.focus_order(), reverse);
                    }
                }
                DashboardAction::None
            }
            KeyCode::Left | KeyCode::Right
                if matches!(
                    editor.focus,
                    ContainerEditFocus::Cancel | ContainerEditFocus::Save
                ) =>
            {
                editor.focus = if editor.focus == ContainerEditFocus::Cancel {
                    ContainerEditFocus::Save
                } else {
                    ContainerEditFocus::Cancel
                };
                DashboardAction::None
            }
            KeyCode::Enter if editor.focus == ContainerEditFocus::Cancel => {
                self.cancel_modal();
                return DashboardAction::None;
            }
            KeyCode::Enter if editor.focus == ContainerEditFocus::Suggestions => {
                editor.take_suggestion();
                editor.error = None;
                DashboardAction::None
            }
            // Enter belongs to Save everywhere else, so only the checkbox
            // itself answers to it; Space also toggles the selected row.
            KeyCode::Enter if editor.focus == ContainerEditFocus::ReadOnly => {
                editor.toggle_read_only();
                DashboardAction::None
            }
            KeyCode::Char(' ')
                if matches!(
                    editor.focus,
                    ContainerEditFocus::ReadOnly | ContainerEditFocus::Mounts
                ) =>
            {
                editor.toggle_read_only();
                DashboardAction::None
            }
            KeyCode::Enter
                if matches!(
                    editor.focus,
                    ContainerEditFocus::Source | ContainerEditFocus::Destination
                ) =>
            {
                editor.error = editor.add_mount();
                DashboardAction::None
            }
            KeyCode::Enter => match editor.save() {
                Ok(action) => {
                    self.cancel_modal();
                    return action;
                }
                Err(error) => {
                    editor.error = Some(error);
                    DashboardAction::None
                }
            },
            KeyCode::Delete | KeyCode::Char('d')
                if matches!(
                    editor.focus,
                    ContainerEditFocus::Mounts | ContainerEditFocus::Suggestions
                ) =>
            {
                editor.remove_selected();
                DashboardAction::None
            }
            _ if editor.field().is_some() => {
                if editor
                    .field_mut()
                    .is_some_and(|field| field.handle_key(key).changed())
                {
                    editor.error = None;
                }
                DashboardAction::None
            }
            _ => DashboardAction::None,
        };
        self.mode = Mode::EditContainer(editor);
        action
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
        self.mode = Mode::ConfirmImportBundle(ImportBundleConfirmation {
            dirty_git_roots,
            omitted_non_git_dirs,
            scratch_git_roots,
            has_untracked_files,
            ignore_untracked: has_untracked_files,
            focus: primary_button(IMPORT_BUNDLE_BUTTONS),
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
            focus: RenameFocus::Field,
        });
    }

    pub(crate) fn handle_rename_key(
        &mut self,
        key: KeyEvent,
        mut editor: RenameEditor,
    ) -> DashboardAction {
        let code = key.code;
        match code {
            KeyCode::Esc => {
                self.cancel_modal();
                DashboardAction::None
            }
            KeyCode::Tab | KeyCode::BackTab => {
                editor.focus =
                    cycle_control(editor.focus, &RENAME_FOCUS_ORDER, code == KeyCode::BackTab);
                self.mode = Mode::Rename(editor);
                DashboardAction::None
            }
            KeyCode::Left | KeyCode::Right if editor.focus != RenameFocus::Field => {
                editor.focus = RenameFocus::from_button_index(cycle_button_focus(
                    editor.focus.button_index(),
                    RENAME_BUTTONS.len(),
                    code == KeyCode::Left,
                ));
                self.mode = Mode::Rename(editor);
                DashboardAction::None
            }
            KeyCode::Enter if editor.focus == RenameFocus::Cancel => {
                self.cancel_modal();
                DashboardAction::None
            }
            KeyCode::Enter if editor.title.trim().is_empty() => {
                self.notices.set("Session name cannot be empty.");
                self.mode = Mode::Rename(editor);
                DashboardAction::None
            }
            KeyCode::Enter => {
                self.cancel_modal();
                DashboardAction::RenameSession {
                    session_id: editor.session_id,
                    title: editor.title.into_value(),
                }
            }
            _ if editor.focus == RenameFocus::Field => {
                editor.title.handle_key(key);
                self.mode = Mode::Rename(editor);
                DashboardAction::None
            }
            _ => {
                self.mode = Mode::Rename(editor);
                DashboardAction::None
            }
        }
    }

    pub(crate) fn handle_import_bundle_key(
        &mut self,
        code: KeyCode,
        mut confirmation: ImportBundleConfirmation,
    ) -> DashboardAction {
        // The checkbox toggle is independent of which button has focus.
        if code == KeyCode::Char(' ') && confirmation.has_untracked_files {
            confirmation.ignore_untracked = !confirmation.ignore_untracked;
            self.mode = Mode::ConfirmImportBundle(confirmation);
            return DashboardAction::None;
        }
        let cancelled = DashboardAction::ConfirmImportBundle {
            accepted: false,
            include_untracked: false,
        };
        confirmation.focus =
            match button_row_key(code, confirmation.focus, IMPORT_BUNDLE_BUTTONS.len()) {
                ButtonKey::Activate(index) if index == primary_button(IMPORT_BUNDLE_BUTTONS) => {
                    return DashboardAction::ConfirmImportBundle {
                        accepted: true,
                        include_untracked: !confirmation.ignore_untracked,
                    };
                }
                ButtonKey::Activate(_) | ButtonKey::Cancel => return cancelled,
                ButtonKey::Focus(focus) => focus,
                ButtonKey::Ignored => confirmation.focus,
            };
        self.mode = Mode::ConfirmImportBundle(confirmation);
        DashboardAction::None
    }

    pub(crate) fn handle_confirmation_key(
        &mut self,
        key: KeyEvent,
        dialog: ConfirmDialog,
    ) -> DashboardAction {
        let code = key.code;
        let ConfirmDialog {
            confirmation,
            focus,
        } = dialog;
        let buttons = confirmation_buttons(&confirmation);
        if buttons.is_empty() {
            return self.handle_typed_confirmation_key(key, confirmation);
        }
        let focus = match button_row_key(code, focus, buttons.len()) {
            ButtonKey::Activate(index) => {
                return self.activate_confirmation_button(confirmation, index);
            }
            ButtonKey::Cancel => {
                if let Confirmation::DestroyStopped { reopen, .. } = confirmation {
                    self.restore_after_confirmation(reopen);
                } else {
                    self.cancel_modal();
                }
                return DashboardAction::None;
            }
            ButtonKey::Focus(next) => next,
            ButtonKey::Ignored => focus,
        };
        self.mode = Mode::Confirm(ConfirmDialog {
            confirmation,
            focus,
        });
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
                        .with_filter(hel::hel_text_input::InputFilter::AsciiAlphabeticUppercase),
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

    fn handle_typed_confirmation_key(
        &mut self,
        key: KeyEvent,
        confirmation: Confirmation,
    ) -> DashboardAction {
        let code = key.code;
        match confirmation {
            Confirmation::ForceStop {
                session_id,
                mut typed,
            } => match code {
                KeyCode::Esc => {
                    self.cancel_modal();
                    DashboardAction::None
                }
                KeyCode::Enter if typed == FORCE_STOP_CONFIRMATION => {
                    self.cancel_modal();
                    DashboardAction::ForceStop { session_id }
                }
                _ => {
                    typed.handle_key(key);
                    self.mode = Mode::Confirm(ConfirmDialog::new(Confirmation::ForceStop {
                        session_id,
                        typed,
                    }));
                    DashboardAction::None
                }
            },
            // Button dialogs are handled by `handle_confirmation_key`.
            other => {
                self.mode = Mode::Confirm(ConfirmDialog::new(other));
                DashboardAction::None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use crossterm::event::{KeyCode, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
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
            container_editor(&dashboard).focus,
            ContainerEditFocus::Memory
        );

        // Take the remembered directory as the next mount.
        while container_editor(&dashboard).focus != ContainerEditFocus::Suggestions {
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
        while container_editor(&dashboard).focus != ContainerEditFocus::Suggestions {
            dashboard.handle_key(key(KeyCode::Tab));
        }
        dashboard.handle_key(key(KeyCode::Char('d')));
        assert!(container_editor(&dashboard).suggestions.is_empty());
        while container_editor(&dashboard).focus != ContainerEditFocus::Mounts {
            dashboard.handle_key(key(KeyCode::Tab));
        }
        dashboard.handle_key(key(KeyCode::Up));
        assert_eq!(container_editor(&dashboard).mount_index, 0);
        dashboard.handle_key(key(KeyCode::Char('d')));

        while container_editor(&dashboard).focus != ContainerEditFocus::Save {
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
        while container_editor(&dashboard).focus != ContainerEditFocus::Source {
            dashboard.handle_key(key(KeyCode::Tab));
        }
        for character in "/nfs/share".chars() {
            dashboard.handle_key(key(KeyCode::Char(character)));
        }
        dashboard.handle_key(key(KeyCode::Tab));
        dashboard.handle_key(key(KeyCode::Tab));
        assert_eq!(
            container_editor(&dashboard).focus,
            ContainerEditFocus::ReadOnly
        );
        dashboard.handle_key(key(KeyCode::Char(' ')));
        assert!(container_editor(&dashboard).read_only);
        while container_editor(&dashboard).focus != ContainerEditFocus::Source {
            dashboard.handle_key(key(KeyCode::Tab));
        }
        dashboard.handle_key(key(KeyCode::Enter));

        // Space on a listed row toggles that row, and the flag is saved.
        while container_editor(&dashboard).focus != ContainerEditFocus::Mounts {
            dashboard.handle_key(key(KeyCode::Tab));
        }
        dashboard.handle_key(key(KeyCode::Up));
        assert_eq!(container_editor(&dashboard).mount_index, 0);
        dashboard.handle_key(key(KeyCode::Char(' ')));

        while container_editor(&dashboard).focus != ContainerEditFocus::Save {
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
        assert_eq!(editor.focus, RenameFocus::Field);
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

    fn rename_focus(dashboard: &DashboardState) -> RenameFocus {
        let Mode::Rename(editor) = &dashboard.mode else {
            panic!("expected rename editor");
        };
        editor.focus
    }

    #[test]
    fn rename_editor_cycles_focus_from_the_field_through_both_buttons() {
        let mut dashboard = dashboard_with_rename_editor();
        for expected in [
            RenameFocus::Cancel,
            RenameFocus::Save,
            RenameFocus::Field,
            RenameFocus::Cancel,
        ] {
            assert_eq!(
                dashboard.handle_key(key(KeyCode::Tab)),
                DashboardAction::None
            );
            assert_eq!(rename_focus(&dashboard), expected);
        }

        let mut dashboard = dashboard_with_rename_editor();
        for expected in [RenameFocus::Save, RenameFocus::Cancel, RenameFocus::Field] {
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
            assert_eq!(rename_focus(&dashboard), RenameFocus::Field);
        }

        dashboard.handle_key(key(KeyCode::Tab));
        assert_eq!(rename_focus(&dashboard), RenameFocus::Cancel);
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Right)),
            DashboardAction::None
        );
        assert_eq!(rename_focus(&dashboard), RenameFocus::Save);
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Left)),
            DashboardAction::None
        );
        assert_eq!(rename_focus(&dashboard), RenameFocus::Cancel);
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
        assert_eq!(editor.focus, RenameFocus::Cancel);
    }

    #[test]
    fn rename_editor_cancel_button_closes_without_renaming() {
        let mut dashboard = dashboard_with_rename_editor();
        dashboard.handle_key(key(KeyCode::Tab));
        assert_eq!(rename_focus(&dashboard), RenameFocus::Cancel);
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
        assert_eq!(rename_focus(&dashboard), RenameFocus::Save);
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

        // The field submits, so Save stays lit while the field has focus.
        assert_eq!(
            button_styles(&mut dashboard),
            (Color::DarkGray, Color::Cyan)
        );

        dashboard.handle_key(key(KeyCode::Tab));
        assert_eq!(
            button_styles(&mut dashboard),
            (Color::Cyan, Color::DarkGray)
        );

        dashboard.handle_key(key(KeyCode::Tab));
        assert_eq!(
            button_styles(&mut dashboard),
            (Color::DarkGray, Color::Cyan)
        );
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
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Char(' '))),
            DashboardAction::None
        );
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

        // Focus starts on Continue; moving to Cancel does not disturb the checkbox.
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
        assert_eq!(confirmation.focus, 0);
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
        assert_eq!(dialog.focus, 0);
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::None
        );
        assert!(matches!(dashboard.mode, Mode::Dashboard));
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
        assert_eq!(dialog.focus, 1);
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
            assert_eq!(dialog.focus, 0, "{cycle_keys:?}");
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
        assert_eq!(buffer[(cancel_x, button_y)].bg, Color::DarkGray);
        // Each label keeps its one-cell padding inside the button background.
        assert_eq!(buffer[(cancel_x - 1, button_y)].bg, Color::DarkGray);
        assert_eq!(buffer[(stop_x - 1, button_y)].bg, Color::Cyan);
        assert!(!lines.iter().any(|line| line.contains("Press y/Enter")));
    }

    #[test]
    fn stopping_a_session_warns_about_a_review_it_would_end() {
        let quiet = confirmation_lines(&Confirmation::Close {
            session_id: "session-1".into(),
            reviewer_conversation: false,
        });
        assert!(
            !quiet.iter().any(|line| line.contains("second opinion")),
            "a session with no review says nothing about one: {quiet:?}"
        );

        let warned = confirmation_lines(&Confirmation::Close {
            session_id: "session-1".into(),
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
                reviewer_conversation: false,
            },
            // The warning adds rows, so the taller variant has to keep its
            // buttons on screen too.
            Confirmation::Close {
                session_id: "session-1".into(),
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

        let buffer = terminal.backend().buffer();
        let lines = buffer_lines(buffer);
        let source_row = lines
            .iter()
            .position(|line| line.contains("Source:") && line.contains('▏'))
            .expect("focused source field");
        let source_y = buffer.area.y + source_row as u16;
        let cursor_x = buffer.area.x + cell_column(&lines[source_row], "▏");
        assert_eq!(buffer[(cursor_x, source_y)].bg, Color::Cyan);
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
        assert_eq!(buffer[(cancel_x, button_y)].bg, Color::DarkGray);
        assert_eq!(buffer[(check_x, button_y)].bg, Color::DarkGray);

        dashboard.handle_key(key(KeyCode::Tab));
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw repository origin dialog with cancel focused");
        let buffer = terminal.backend().buffer();
        let lines = buffer_lines(buffer);
        assert!(!lines.iter().any(|line| line.contains('▏')));
        let button_row = lines
            .iter()
            .position(|line| line.contains(" Cancel ") && line.contains(" Check origin "))
            .expect("button row");
        let button_y = buffer.area.y + button_row as u16;
        let cancel_x = buffer.area.x + cell_column(&lines[button_row], "Cancel");
        assert_eq!(buffer[(cancel_x, button_y)].bg, Color::Cyan);
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
        assert_eq!(dialog.focus, RepositoryOriginFocus::Field);
        assert_eq!(
            dialog.error.as_deref(),
            Some("That origin does not contain checkpoint base b41dc78.")
        );
    }
}
