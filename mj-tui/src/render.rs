//! Dashboard rendering: pane layout, session tables, capacity, quotas, footer.
mod capacity;
mod footer;
mod quotas;
pub(crate) mod sessions;
pub(crate) use capacity::*;
pub(crate) use footer::*;
pub(crate) use quotas::*;
pub(crate) use sessions::*;

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Cell, Clear, HighlightSpacing, Paragraph, Row, Table, TableState, Wrap,
};

use mj_core::config::{Config, PermissionMode};
use mj_core::state::{SessionRecord, SessionState, SessionTransitionKind, State};

use mj_chat::chat::render_agent_message_head;
use mj_chat::components::{render_scrollbar, scrollbar_geometry};
use mj_chat::theme;
use mj_client::quota::{API_LABEL, ProfileQuota, QuotaWindow};
use mj_client::review::RuntimeReviewView;
use mj_core::targets::DeploymentCapacityKind;

use crate::dialogs::{
    render_changed_files, render_config_id_editor, render_confirmation, render_container_editor,
    render_import_bundle_confirmation, render_import_progress, render_notice_log,
    render_rename_editor, render_repository_origin, render_session_cpu_report,
    render_target_actions, render_web_dialog,
};
use crate::ingest::{CapacityDetail, SessionDetail, SessionOperationDisplay};
use crate::resume::render_resume_dialog;
use crate::widgets::{Truncate, format_resource_bytes, truncate_to_cells};
use crate::wizards::{render_new_wizard, render_resume_wizard};
use crate::workspaces::render_workspace_manager;
use crate::{
    AttentionLevel, DashboardState, Focus, Mode, PaneSize, SessionOperationKind, SessionsRow,
    SupportPane,
};

const SESSION_TABLE_CHROME_HEIGHT: u16 = 3;
/// One fixed row at the top of Sessions for Create and Resume.
pub(crate) const SESSION_ACTIONS_HEIGHT: u16 = 1;

/// Whether the selection of `focus` moved by keyboard since the last frame.
/// Only then does a table re-centre on it; a click or a refresh leaves the
/// rows where they are.
fn take_selection_recenter(dashboard: &DashboardState, focus: Focus) -> bool {
    if dashboard.recenter_on_selection.get() == Some(focus) {
        dashboard.recenter_on_selection.set(None);
        true
    } else {
        false
    }
}

/// Draws the active modal over the dashboard already on the frame. Each modal
/// clears its own centered rect, so the panes stay visible around it.
///
/// The registry moves out for the call because the modal renderers read the
/// rest of the dashboard while they register their own surfaces.
pub(crate) fn render_modal(frame: &mut Frame, area: Rect, dashboard: &mut DashboardState) {
    dashboard.prepare_dialog_state();
    let mut surfaces = std::mem::take(&mut dashboard.frame_surfaces);
    match &dashboard.mode {
        Mode::New(wizard) => render_new_wizard(frame, area, dashboard, wizard, &mut surfaces),
        Mode::Resume(wizard) => render_resume_wizard(frame, area, dashboard, wizard, &mut surfaces),
        Mode::ResumeDialog(dialog) => {
            render_resume_dialog(frame, area, dashboard, dialog, &mut surfaces)
        }
        Mode::RepositoryOrigin(dialog) => {
            render_repository_origin(frame, area, dialog, &mut surfaces)
        }
        Mode::ConfigId(editor) => render_config_id_editor(frame, area, editor, &mut surfaces),
        Mode::TargetActions(dialog) => {
            render_target_actions(frame, area, dashboard, dialog, &mut surfaces)
        }
        Mode::Web(dialog) => render_web_dialog(frame, area, dialog, &mut surfaces),
        Mode::WorkspaceManager(dialog) => {
            render_workspace_manager(frame, area, dialog, &mut surfaces)
        }
        Mode::Rename(editor) => render_rename_editor(frame, area, editor, &mut surfaces),
        Mode::ChangedFiles(dialog) => {
            render_changed_files(frame, area, dashboard, dialog, &mut surfaces)
        }
        Mode::SessionCpuReport(dialog) => {
            render_session_cpu_report(frame, area, dashboard, dialog, &mut surfaces)
        }
        Mode::NoticeLog(dialog) => render_notice_log(frame, area, dashboard, dialog, &mut surfaces),
        Mode::EditContainer(editor) => render_container_editor(frame, area, editor, &mut surfaces),
        Mode::Importing(progress) => render_import_progress(frame, area, progress, &mut surfaces),
        Mode::ConfirmImportBundle(confirmation) => {
            render_import_bundle_confirmation(frame, area, confirmation, &mut surfaces)
        }
        Mode::Confirm(dialog) => render_confirmation(frame, area, dialog, &mut surfaces),
        Mode::Help(overlay) => {
            crate::help::render_help(frame, area, dashboard, overlay, &mut surfaces)
        }
        Mode::Palette(palette) => {
            crate::palette::render_palette(frame, area, dashboard, palette, &mut surfaces)
        }
        Mode::Setup(dialog) => crate::setup::render_setup(frame, area, dialog, &mut surfaces),
        Mode::Welcome(dialog) => {
            crate::welcome::render_welcome(frame, area, dashboard, dialog, &mut surfaces)
        }
        Mode::Dashboard => {}
    }
    dashboard.render_dialog_confirmation(frame, area, &mut surfaces);
    dashboard.frame_surfaces = surfaces;
}

/// Draws the combined surface with no conversation on it.
///
/// Only tests use this: the binary always has an `ActiveChat` to pass, or a
/// workspace with no live session, which is what this stands for.
#[cfg(test)]
pub(crate) fn render(frame: &mut Frame, dashboard: &mut DashboardState) {
    let mut chats = std::collections::BTreeMap::new();
    let opening = std::collections::BTreeMap::new();
    crate::combined::render_combined_for_test(frame, dashboard, &mut chats, &opening, false);
}

/// The width from which the Sessions sidebar sits beside the conversation.
/// Below it, down to [`NARROW_TERMINAL_WIDTH`], the sidebar stacks above.
pub(crate) const MINIMUM_TERMINAL_WIDTH: u16 = 80;
/// The narrowest frame the dashboard draws at all.
pub(crate) const NARROW_TERMINAL_WIDTH: u16 = 60;

pub(crate) enum TerminalSizeRequirement {
    Width(u16),
    Height(u16),
}

pub(crate) fn render_terminal_too_small(
    frame: &mut Frame,
    area: Rect,
    requirement: TerminalSizeRequirement,
) {
    let instructions = match requirement {
        TerminalSizeRequirement::Width(required_width) => vec![
            Line::raw(format!("Need at least {required_width} columns.")),
            Line::raw(format!("Current width: {}.", area.width)),
        ],
        TerminalSizeRequirement::Height(required_height) => vec![Line::raw(format!(
            "Increase height to at least {required_height} rows (currently {}).",
            area.height
        ))],
    };
    frame.render_widget(Clear, area);
    let mut lines = vec![Line::styled(
        "Terminal too small",
        Style::default().add_modifier(Modifier::BOLD),
    )];
    lines.extend(instructions);
    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), area);
}

#[cfg(test)]
mod tests;
