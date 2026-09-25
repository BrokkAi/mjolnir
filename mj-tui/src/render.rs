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
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Margin, Rect};
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
    render_rename_editor, render_repository_origin, render_target_actions, render_web_dialog,
};
use crate::ingest::{CapacityDetail, SessionDetail, SessionOperationDisplay};
use crate::resume::render_resume_dialog;
use crate::widgets::{Truncate, format_resource_bytes, truncate_to_cells};
use crate::wizards::{render_new_wizard, render_resume_wizard};
use crate::workspaces::render_workspace_manager;
use crate::{
    AttentionLevel, DashboardState, Focus, Mode, PaneSize, SelectionDirection,
    SessionOperationKind, SessionsRow, SupportPane,
};

const SESSION_TABLE_CHROME_HEIGHT: u16 = 3;
/// One fixed row at the top of Sessions for Create and Resume.
pub(crate) const SESSION_ACTIONS_HEIGHT: u16 = 1;

/// Move a table viewport just far enough to show the row beyond the selection
/// in the direction the user moved. Variable-height rows only get that margin
/// when the selected row and its neighbor fit together.
fn offset_with_directional_lookahead(
    current_offset: usize,
    selected: usize,
    direction: SelectionDirection,
    row_heights: &[usize],
    viewport_height: usize,
) -> usize {
    if row_heights.is_empty() || viewport_height == 0 || selected >= row_heights.len() {
        return current_offset;
    }
    let neighbor = match direction {
        SelectionDirection::Up => selected.checked_sub(1),
        SelectionDirection::Down => selected
            .checked_add(1)
            .filter(|index| *index < row_heights.len()),
    };
    let Some(neighbor) = neighbor else {
        return current_offset;
    };
    let pair_start = selected.min(neighbor);
    let pair_end = selected.max(neighbor);
    let pair_height = row_heights[pair_start..=pair_end]
        .iter()
        .copied()
        .sum::<usize>();
    if pair_height > viewport_height {
        return current_offset;
    }

    match direction {
        SelectionDirection::Up => {
            let offset = current_offset.min(neighbor);
            if row_heights[offset..=selected]
                .iter()
                .copied()
                .sum::<usize>()
                <= viewport_height
            {
                offset
            } else {
                neighbor
            }
        }
        SelectionDirection::Down => {
            let mut offset = current_offset.min(selected);
            let mut visible_height = row_heights[offset..=neighbor]
                .iter()
                .copied()
                .sum::<usize>();
            while offset < selected && visible_height > viewport_height {
                visible_height = visible_height.saturating_sub(row_heights[offset]);
                offset += 1;
            }
            offset
        }
    }
}

fn take_scroll_lookahead(dashboard: &DashboardState, focus: Focus) -> Option<SelectionDirection> {
    let pending = dashboard.scroll_lookahead.get();
    if pending.is_some_and(|(pending_focus, _)| pending_focus == focus) {
        dashboard.scroll_lookahead.set(None);
        pending.map(|(_, direction)| direction)
    } else {
        None
    }
}

/// Draws the first-run screen: there is no conversation and no session list
/// yet, so the surface explains how to get one and shows the support panes
/// under it.
pub(crate) fn render_onboarding_surface(frame: &mut Frame, dashboard: &mut DashboardState) {
    let area = frame.area();
    frame.render_widget(Block::default().style(theme::base()), area);
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(8),
            Constraint::Length(5),
            Constraint::Length(8),
            Constraint::Length(1),
        ])
        .split(area);
    render_dashboard_title(
        frame,
        Rect::new(layout[0].x, layout[0].y, layout[0].width, 1),
        &dashboard.workspace_name,
        &dashboard.version_label,
    );
    crate::surface_controls::render_onboarding_actions(
        frame,
        Rect::new(
            layout[0].x,
            layout[0].y.saturating_add(1),
            layout[0].width,
            layout[0].height.saturating_sub(1),
        ),
        dashboard,
    );

    render_onboarding(frame, layout[1], dashboard);
    render_capacity(frame, layout[2], dashboard, None);
    render_quotas(frame, layout[3], dashboard, None);
    render_footer(frame, layout[4], dashboard);
    render_modal(frame, area, dashboard);
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
        Mode::Dashboard => {}
    }
    dashboard.render_dialog_confirmation(frame, area, &mut surfaces);
    dashboard.frame_surfaces = surfaces;
}

fn render_dashboard_title(frame: &mut Frame, area: Rect, workspace_name: &str, version: &str) {
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!("{} MJOLNIR", theme::glyphs().spark),
                theme::title(true),
            ),
            // The first-run screen has no workspace pane to carry the build
            // number, so the brand line names it here instead.
            Span::styled(format!("  {version}  /  {workspace_name}"), theme::muted()),
        ]))
        .alignment(Alignment::Center),
        area,
    );
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

/// The quickstart section for a machine with no coding agent installed.
const NO_AGENT_HELP_URL: &str =
    "https://mjolnir.brokk.ai/quickstart/#if-no-coding-agent-is-installed";

fn render_onboarding(frame: &mut Frame, area: Rect, dashboard: &DashboardState) {
    let settings_key = dashboard.first_key_label(crate::CommandId::OpenConfig);
    let mut lines = vec![
        Line::from(Span::styled(
            "Make room for your next idea.",
            theme::title(true),
        )),
        Line::raw(""),
    ];
    if dashboard.config.enabled_profiles().next().is_none() {
        lines.extend(
            onboarding_agent_lines(
                &dashboard.config,
                dashboard.installed_agents.as_deref(),
                settings_key.as_deref(),
            )
            .into_iter()
            .map(Line::raw),
        );
    }
    if dashboard.config.targets.is_empty() {
        lines.push(Line::raw(format!(
            "No target is set up yet. {} to add one.",
            open_settings(settings_key.as_deref())
        )));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true })
            .block(theme::panel(false).title(format!(" {} Get started ", theme::glyphs().spark))),
        area,
    );
}

/// "Press ctrl+b s for Settings", or the palette route when no key opens
/// Settings.
fn open_settings(settings_key: Option<&str>) -> String {
    match settings_key {
        Some(key) => format!("Press {key} for Settings"),
        None => "Open Settings from the command palette".to_owned(),
    }
}

/// What the Get started panel says about agent profiles when none is
/// enabled. It states only what is known: the look for installed agents
/// decides between "none was found" and naming what was found, and before
/// that look answers the panel says nothing about the machine (launch finding
/// R13-4).
fn onboarding_agent_lines(
    config: &mj_core::config::Config,
    installed: Option<&[mj_core::config::HarnessKind]>,
    settings_key: Option<&str>,
) -> Vec<String> {
    let settings = open_settings(settings_key);
    if !config.profiles.is_empty() {
        return vec![
            "Every agent profile is turned off.".to_owned(),
            format!("{settings} to turn one on."),
        ];
    }
    match installed {
        None => vec![
            "No agent profile is set up yet.".to_owned(),
            format!("{settings} to add one."),
        ],
        Some([]) => vec![
            "No coding agent was found on this machine.".to_owned(),
            format!(
                "Install {}, and sign in to it once.",
                mj_core::config::HarnessKind::every_display_name_or()
            ),
            match settings_key {
                Some(key) => format!("Then press {key} for Settings and choose Detect profiles."),
                None => "Then open Settings from the command palette and choose Detect profiles."
                    .to_owned(),
            },
            format!("How to install one: {NO_AGENT_HELP_URL}"),
        ],
        Some(found) => vec![
            format!(
                "Found on this machine: {}.",
                found
                    .iter()
                    .map(|kind| kind.display_name())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            format!(
                "{settings} and choose Detect profiles to add {}.",
                if found.len() == 1 { "it" } else { "them" }
            ),
        ],
    }
}

#[cfg(test)]
mod tests;
