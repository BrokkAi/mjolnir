//! Dashboard rendering: pane layout, session tables, capacity, quotas, footer.
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
use mj_core::state::{SessionRecord, SessionState, SessionTransitionKind};

use mj_chat::chat::render_agent_message_head;
use mj_chat::components::{render_scrollbar, scrollbar_geometry};
use mj_chat::theme;
use mj_client::quota::{API_LABEL, ProfileQuota, QuotaWindow};
use mj_client::review::RuntimeReviewView;
use mj_core::targets::DeploymentCapacityKind;

use crate::dialogs::{
    render_config_id_editor, render_confirmation, render_container_editor,
    render_import_bundle_confirmation, render_import_progress, render_rename_editor,
    render_repository_origin, render_target_actions, render_web_dialog,
};
use crate::ingest::{CapacityDetail, SessionDetail, SessionOperationDisplay};
use crate::resume::render_resume_dialog;
use crate::widgets::format_resource_bytes;
use crate::wizards::{render_new_wizard, render_resume_wizard};
use crate::workspaces::render_workspace_manager;
use crate::{
    DashboardState, Focus, Mode, PaneSize, SelectionDirection, SessionOperationKind, SessionsRow,
    SupportPane,
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

fn render_dashboard_title(frame: &mut Frame, area: Rect, workspace_name: &str) {
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("✦ MJOLNIR", theme::title(true)),
            Span::styled(format!("  /  {workspace_name}"), theme::muted()),
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
    crate::combined::render_combined(frame, dashboard, None, false);
}

pub(crate) const MINIMUM_TERMINAL_WIDTH: u16 = 80;

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

fn render_onboarding(frame: &mut Frame, area: Rect, dashboard: &DashboardState) {
    let missing = [
        (
            dashboard.config.enabled_profiles().next().is_none(),
            "an enabled agent profile",
        ),
        (dashboard.config.targets.is_empty(), "a target template"),
    ]
    .into_iter()
    .filter_map(|(missing, label)| missing.then_some(label))
    .collect::<Vec<_>>()
    .join(", ");
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                "Mjolnir needs a little fuel.",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::raw(""),
            Line::raw(format!("Settings can create {missing} from this machine.")),
            Line::raw(
                "Press F7 for Settings to add accounts or connections. Local runtimes are checked automatically.",
            ),
        ])
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: true })
        .block(theme::panel(false).title(" ✦ Get started ")),
        area,
    );
}

/// Session-row rendering results that the caller folds back into the
/// combined surface's mouse hitboxes once the borrow of the session list has
/// ended.
pub(crate) struct SessionRowsRendered {
    pub(crate) session_row_areas: Vec<(usize, Rect)>,
    pub(crate) project_heading_areas: Vec<(String, Rect)>,
}

const PANE_SIZE_CONTROLS_WIDTH: u16 = 11;
const PANE_SIZE_CONTROLS_WIDTH_WITHOUT_MAXIMUM: u16 = 7;
const PANE_SIZE_CONTROL_WIDTH: u16 = 3;

fn pane_size_controls_width(maximize_enabled: bool) -> u16 {
    if maximize_enabled {
        PANE_SIZE_CONTROLS_WIDTH
    } else {
        PANE_SIZE_CONTROLS_WIDTH_WITHOUT_MAXIMUM
    }
}

/// The width left for a pane's left title after preserving the border, a gap,
/// and the currently visible right-aligned size controls.
pub(crate) fn pane_title_content_width(width: u16, maximize_enabled: bool) -> u16 {
    width.saturating_sub(2 + 1 + pane_size_controls_width(maximize_enabled))
}

fn displayed_pane_size(active: PaneSize, maximize_enabled: bool) -> PaneSize {
    if active == PaneSize::Maximized && !maximize_enabled {
        PaneSize::Standard
    } else {
        active
    }
}

/// The title-bar controls. Their padded backgrounds are the buttons; the
/// unstyled cells between them keep inactive controls visually distinct.
pub(crate) fn pane_size_controls(active: PaneSize, maximize_enabled: bool) -> Line<'static> {
    let active = displayed_pane_size(active, maximize_enabled);
    let mut spans = Vec::new();
    for (index, (size, glyph)) in [
        (PaneSize::Minimized, "▁"),
        (PaneSize::Standard, "▪"),
        (PaneSize::Maximized, "□"),
    ]
    .into_iter()
    .filter(|(size, _)| *size != PaneSize::Maximized || maximize_enabled)
    .enumerate()
    {
        if index > 0 {
            spans.push(Span::raw(" "));
        }
        let style = if size == active {
            Style::default()
                .fg(theme::palette().accent)
                .bg(theme::palette().surface_raised)
                .add_modifier(Modifier::BOLD)
        } else {
            theme::muted().bg(theme::palette().surface)
        };
        spans.push(Span::styled(format!(" {glyph} "), style));
    }
    Line::from(spans).right_aligned()
}

/// Minimized panes have no right border, but keep its column as horizontal
/// rule so their controls line up with those in a fully bordered pane.
pub(crate) fn minimized_pane_size_controls(
    _focused: bool,
    maximize_enabled: bool,
) -> Line<'static> {
    let mut controls = pane_size_controls(PaneSize::Minimized, maximize_enabled);
    controls.spans.push(Span::raw("─"));
    controls
}

/// Screen rectangles occupied by the padded control chips in a bordered
/// pane's right-aligned title.
pub(crate) fn pane_size_control_areas(
    area: Rect,
    pane: SupportPane,
    maximize_enabled: bool,
) -> Vec<(SupportPane, PaneSize, Rect)> {
    let start = area
        .right()
        .saturating_sub(1)
        .saturating_sub(pane_size_controls_width(maximize_enabled));
    [PaneSize::Minimized, PaneSize::Standard, PaneSize::Maximized]
        .into_iter()
        .filter(|size| *size != PaneSize::Maximized || maximize_enabled)
        .enumerate()
        .map(|(index, size)| {
            (
                pane,
                size,
                Rect::new(
                    start.saturating_add(index as u16 * 4),
                    area.y,
                    PANE_SIZE_CONTROL_WIDTH,
                    1,
                ),
            )
        })
        .collect()
}

/// One table row of the Sessions pane, already laid out.
struct DrawnSessionRow {
    /// Index into `ordered_sessions()` for the session this row draws.
    session: Option<usize>,
    /// Project key of the heading this row carries, if it opens a group.
    heading: Option<String>,
    lines: Vec<Line<'static>>,
    /// Blank rows drawn under this one, to separate groups.
    spacing: u16,
}

#[derive(Debug, Clone, Copy)]
struct SessionRowsRenderOptions {
    force_expanded: bool,
    summary_only: bool,
    show_project_numbers: bool,
    show_selection: bool,
}

impl SessionRowsRenderOptions {
    const DASHBOARD: Self = Self {
        force_expanded: false,
        summary_only: false,
        show_project_numbers: true,
        show_selection: true,
    };

    const MINIMIZED: Self = Self {
        force_expanded: false,
        summary_only: true,
        show_project_numbers: true,
        show_selection: true,
    };
}

impl DrawnSessionRow {
    fn content_height(&self) -> u16 {
        u16::try_from(self.lines.len()).unwrap_or(u16::MAX)
    }
}

/// Lays the pane out without drawing it, so the layout can ask how tall it
/// wants to be before it has any rows to give it.
fn drawn_session_rows(dashboard: &DashboardState, width: u16) -> Vec<DrawnSessionRow> {
    drawn_session_rows_with_options(dashboard, width, SessionRowsRenderOptions::DASHBOARD)
}

fn drawn_session_rows_with_options(
    dashboard: &DashboardState,
    width: u16,
    options: SessionRowsRenderOptions,
) -> Vec<DrawnSessionRow> {
    let now_epoch_seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let animation_ms = mj_chat::spinner::elapsed_ms();
    let sessions = dashboard.ordered_sessions();
    let targets = session_display_targets(dashboard, &sessions);
    let flow_rows = dashboard.sessions_rows().into_iter().map(|row| match row {
        SessionsRow::ProjectHeading { key, label, number } => SessionsRow::ProjectHeading {
            key,
            label,
            number: options.show_project_numbers.then_some(number).flatten(),
        },
        SessionsRow::Session { index, expanded } => SessionsRow::Session {
            index,
            expanded: options.force_expanded || expanded,
        },
    });
    let mut rows: Vec<DrawnSessionRow> = Vec::new();
    let mut pending_heading: Option<(String, Line<'static>)> = None;
    for row in flow_rows {
        match row {
            SessionsRow::ProjectHeading { key, label, number } => {
                let hotkey = number.map_or_else(String::new, |number| format!("[{number}] "));
                // A heading is drawn inside the row beneath it, so the table's
                // selection index keeps counting sessions and nothing else.
                if let Some(last) = rows.last_mut() {
                    last.spacing = 1;
                }
                pending_heading = Some((
                    key,
                    Line::styled(
                        format!("{hotkey}{label}"),
                        Style::default()
                            .fg(theme::palette().secondary)
                            .add_modifier(Modifier::BOLD),
                    ),
                ));
            }
            SessionsRow::Session { index, expanded } => {
                let Some(session) = sessions.get(index) else {
                    continue;
                };
                // Supply a display-only title without changing the durable session record.
                let named_session = dashboard.go.is_some().then(|| {
                    let mut named = (*session).clone();
                    named.session_title_override =
                        Some(dashboard.go_conversation_title(&session.id));
                    named
                });
                let session = named_session.as_ref().unwrap_or(session);
                let detail = dashboard.session_details.get(&session.id);
                let review = dashboard.session_review(&session.id);
                let unreachable = dashboard.unreachable_sessions.contains(&session.id);
                let facts = SessionRowFacts {
                    detail,
                    unreachable,
                    state: session.state,
                    now_epoch_seconds,
                };
                let primary_busy = !unreachable
                    && session.state == SessionState::Running
                    && detail.is_some_and(|detail| {
                        detail.activity.is_working(
                            detail.current_turn_started_at,
                            !detail.pending_elicitations.is_empty(),
                        )
                    });
                // Review work is independent of the session's primary
                // lifecycle. A review can keep animating while the session
                // is stopped or unreachable, so do not gate it on either.
                let busy = primary_busy || review.is_some_and(RuntimeReviewView::is_working);
                let spinner = busy.then(|| {
                    mj_chat::spinner::compact_frame(dashboard.config.spinner, animation_ms)
                });
                let operation = dashboard.session_operations.get(&session.id);
                let target = targets.get(index).cloned().unwrap_or_default();
                let permission = session_permission_badge(session, operation, &dashboard.config);
                // The selection drives which conversation is on screen, so
                // the caret marks it in both forms.
                let selected = options.show_selection
                    && dashboard.selected_session_id.as_deref() == Some(session.id.as_str());
                let symbol = dashboard
                    .transition_kind(&session.id)
                    .map(|transition| match transition {
                        SessionTransitionKind::Starting => "↑",
                        SessionTransitionKind::Resuming => "↻",
                        SessionTransitionKind::Moving => "⇄",
                        SessionTransitionKind::Stopping => "↓",
                        SessionTransitionKind::Destroying => "⊗",
                    })
                    .or_else(|| dashboard.transition_failure_kind(&session.id).map(|_| "×"))
                    .unwrap_or_else(|| facts.status_symbol(review, operation));
                let prefix = format!("{}{symbol} ", if selected { "› " } else { "  " });
                let (heading_key, heading_line) = match pending_heading.take() {
                    Some((key, line)) => (Some(key), Some(line)),
                    None => (None, None),
                };
                let mut lines = Vec::new();
                lines.extend(heading_line);
                let spacing = u16::from(expanded && !options.summary_only);
                if session.configuration_issue(&dashboard.config).is_some() {
                    lines.push(Line::styled(
                        format!("{prefix}{}", session_name(session)),
                        Style::default().fg(theme::palette().session_error),
                    ));
                    lines.push(Line::styled(
                        "  Needs config repair",
                        Style::default().fg(theme::palette().session_error),
                    ));
                    if expanded && !options.summary_only {
                        lines.push(Line::from("  Enter for repair details · F7 settings"));
                    }
                    rows.push(DrawnSessionRow {
                        session: Some(index),
                        heading: heading_key,
                        lines,
                        spacing,
                    });
                    continue;
                }

                if let Some(transition) = dashboard.transition_kind(&session.id) {
                    lines.push(session_transition_line(
                        &prefix,
                        session,
                        transition,
                        operation,
                        now_epoch_seconds,
                        &target,
                        width,
                        &dashboard.config,
                        None,
                    ));
                    rows.push(DrawnSessionRow {
                        session: Some(index),
                        heading: heading_key,
                        lines,
                        spacing,
                    });
                    continue;
                }
                if let Some(transition) = dashboard.transition_failure_kind(&session.id) {
                    lines.push(session_transition_line(
                        &prefix,
                        session,
                        transition,
                        None,
                        now_epoch_seconds,
                        &target,
                        width,
                        &dashboard.config,
                        session.last_error.as_deref(),
                    ));
                    rows.push(DrawnSessionRow {
                        session: Some(index),
                        heading: heading_key,
                        lines,
                        spacing,
                    });
                    continue;
                }
                if expanded && !options.summary_only {
                    expanded_session_lines(
                        &mut lines,
                        session,
                        detail,
                        review,
                        unreachable,
                        operation,
                        now_epoch_seconds,
                        &target,
                        permission,
                        width,
                        &prefix,
                        spinner,
                        dashboard.config.advanced.detailed_activity_clocks,
                    );
                } else {
                    compact_session_lines(
                        &mut lines,
                        &prefix,
                        session,
                        detail,
                        review,
                        unreachable,
                        operation,
                        now_epoch_seconds,
                        &target,
                        permission,
                        spinner,
                        width,
                    );
                }
                if selected {
                    for line in lines.iter_mut().skip(usize::from(heading_key.is_some())) {
                        line.style = line.style.bg(theme::palette().surface_raised);
                    }
                }
                rows.push(DrawnSessionRow {
                    session: Some(index),
                    heading: heading_key,
                    lines,
                    spacing,
                });
            }
        }
    }
    // The last group never needs a trailing blank row.
    if let Some(last) = rows.last_mut() {
        last.spacing = 0;
    }
    rows
}

/// The four rows an expanded session draws: name, status and identity, and
/// two wrapped rows of the current output. The output block is always two
/// rows, even with nothing to say, so every expanded session is the same
/// height and the layout can be computed from a count.
#[allow(clippy::too_many_arguments)]
fn expanded_session_lines(
    lines: &mut Vec<Line<'static>>,
    session: &SessionRecord,
    detail: Option<&SessionDetail>,
    review: Option<&RuntimeReviewView>,
    unreachable: bool,
    operation: Option<&SessionOperationDisplay>,
    now_epoch_seconds: u64,
    target: &str,
    permission: Option<Span<'static>>,
    width: u16,
    prefix: &str,
    spinner: Option<&'static str>,
    detailed_activity_clocks: bool,
) {
    let style = Style::default().fg(session_band_color(detail, unreachable, session.state));
    let name = recovery_warning_name(session, session_name(session).to_owned(), now_epoch_seconds);
    // The ellipsis action occupies the last three cells of the first line.
    // Keep the activity and output lines at the full content width so a
    // running clock and queued count remain readable in a compact pane.
    let title_width = width.saturating_sub(3);
    lines.push(Line::styled(
        format!(
            "{prefix}{}",
            truncate_display_text(
                &name,
                usize::from(title_width).saturating_sub(Line::raw(prefix).width())
            )
        ),
        style,
    ));
    lines.push(session_activity_line(
        "  ",
        session,
        detail,
        review,
        unreachable,
        operation,
        now_epoch_seconds,
        target,
        permission,
        spinner,
        width,
        detailed_activity_clocks,
    ));

    let (label, message, muted) = match detail.and_then(current_agent_excerpt) {
        Some(message) => ("", Some(message), false),
        None => match detail.and_then(|detail| detail.last_user_message.as_deref()) {
            Some(message) => ("You: ", Some(message), true),
            None => ("", None, false),
        },
    };
    let output_width = usize::from(width.saturating_sub(2));
    let mut output = message
        .map(|message| {
            let text = if label.is_empty() {
                message.replace('\n', " ")
            } else {
                format!("{label}{message}")
            };
            render_agent_message_head(&text, output_width, 2)
        })
        .unwrap_or_default();
    if output.is_empty() {
        output.push(Line::raw("No messages yet"));
    }
    output.resize(2, Line::default());
    for mut line in output.into_iter().take(2) {
        let mut spans = vec![Span::raw("  ")];
        spans.append(&mut line.spans);
        let mut line = Line::from(spans);
        if muted {
            line.style = Style::default().fg(theme::palette().muted);
        }
        lines.push(line);
    }
}

/// The second row of a session. Actionable state and queued work come first so
/// a narrow pane cannot hide them behind the target or profile identity.
#[allow(clippy::too_many_arguments)]
fn session_activity_line(
    prefix: &str,
    session: &SessionRecord,
    detail: Option<&SessionDetail>,
    review: Option<&RuntimeReviewView>,
    unreachable: bool,
    operation: Option<&SessionOperationDisplay>,
    now_epoch_seconds: u64,
    target: &str,
    permission: Option<Span<'static>>,
    spinner: Option<&'static str>,
    width: u16,
    detailed_activity_clocks: bool,
) -> Line<'static> {
    let facts = SessionRowFacts {
        detail,
        unreachable,
        state: session.state,
        now_epoch_seconds,
    };
    let mut status = if let Some(operation) = operation {
        let (label, started_at) = operation_status(operation);
        format!(
            "{label} {}",
            mj_client::usage_format::format_clock(now_epoch_seconds.saturating_sub(started_at))
        )
    } else if facts.state == SessionState::Error {
        "Error".to_owned()
    } else if session.state == SessionState::Provisioning {
        let started_at = session_updated_at_epoch_seconds(session).unwrap_or(now_epoch_seconds);
        format!(
            "Launch {}",
            mj_client::usage_format::format_clock(now_epoch_seconds.saturating_sub(started_at))
        )
    } else if facts.unreachable {
        "Unreachable".to_owned()
    } else if facts.needs_input() {
        "Question".to_owned()
    } else if let Some(label) = review_status_label(review) {
        label.to_owned()
    } else if facts.state.is_active() {
        facts.clock(detailed_activity_clocks)
    } else {
        format!("{:?}", facts.state)
    };
    let queue = detail
        .map(|detail| detail.queued_prompts.len())
        .filter(|count| *count > 0)
        .map(|count| {
            if width <= 24 {
                format!("Q{count}")
            } else {
                format!("[Q {count}]")
            }
        });
    // At the minimum sidebar width, retain both the attention marker and the
    // queue count while using a compact status word.
    if width <= 24 && status == "Unreachable" {
        status = "Offline".to_owned();
    }
    let available = usize::from(width);
    // A minimized cell's second line is reserved for activity and its queue.
    // Keep it two cells in from the pane edge so the summary reads as a
    // continuation of the title line while retaining enough room for status.
    let compact = width <= 24;
    let prefix = if compact { "  " } else { prefix };
    let queue_width = queue
        .as_ref()
        .map_or(0, |queue| Line::raw(queue.as_str()).width() + 1);
    let spinner = spinner.filter(|spinner| {
        Line::raw(prefix).width()
            + Line::raw(*spinner).width()
            + 1
            + Line::raw(status.as_str()).width()
            + queue_width
            <= available
    });
    let spinner_width = spinner.map_or(0, |spinner| Line::raw(spinner).width() + 1);
    let status = truncate_display_text(
        &status,
        available.saturating_sub(Line::raw(prefix).width() + spinner_width + queue_width),
    );
    let status_width = Line::raw(status.as_str()).width() + queue_width + 2;
    let identity_width = if compact {
        0
    } else {
        available.saturating_sub(Line::raw(prefix).width() + spinner_width + status_width)
    };
    let badge_text = permission
        .as_ref()
        .map_or_else(String::new, |permission| format!(" {}", permission.content));
    let badge_width = Line::raw(badge_text.as_str()).width();
    let show_permission = badge_width > 0 && identity_width >= badge_width.saturating_add(1);
    let identity_width =
        identity_width.saturating_sub(if show_permission { badge_width } else { 0 });
    let profile = operation
        .and_then(|operation| operation.resume_destination.as_ref())
        .map(|(profile, _)| profile.as_str())
        .unwrap_or(&session.last_profile);
    let (target_width, profile_width) =
        if Line::raw(target).width() + 2 + Line::raw(profile).width() <= identity_width {
            (Line::raw(target).width(), Line::raw(profile).width())
        } else {
            let half = identity_width.saturating_sub(2) / 2;
            (half, identity_width.saturating_sub(2).saturating_sub(half))
        };
    let target = truncate_display_text(target, target_width);
    let profile = truncate_display_text(profile, profile_width);
    let identity = match (target.is_empty(), profile.is_empty()) {
        (true, true) => String::new(),
        (true, false) => profile,
        (false, true) => target,
        (false, false) => format!("{target}  {profile}"),
    };
    let mut spans = vec![Span::raw(prefix.to_owned())];
    if let Some(spinner) = spinner {
        spans.push(Span::styled(format!("{spinner} "), facts.style()));
    }
    let status_separator = if identity.is_empty() && !show_permission {
        ""
    } else {
        "  "
    };
    spans.push(Span::styled(identity, facts.style()));
    if show_permission && let Some(permission) = permission {
        spans.push(Span::raw(" "));
        spans.push(permission);
    }
    spans.push(Span::styled(
        format!("{status_separator}{status}"),
        facts.style().add_modifier(if facts.needs_input() {
            Modifier::BOLD
        } else {
            Modifier::empty()
        }),
    ));
    if let Some(queue) = queue {
        spans.push(Span::styled(
            format!(" {queue}"),
            facts.style().add_modifier(Modifier::BOLD),
        ));
    }
    Line::from(spans)
}

fn truncate_display_text(text: &str, width: usize) -> String {
    if Line::raw(text).width() <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".to_owned();
    }
    let mut output = String::new();
    for character in text.chars() {
        let candidate = format!("{output}{character}…");
        if Line::raw(candidate.as_str()).width() > width {
            break;
        }
        output.push(character);
    }
    format!("{output}…")
}

/// Folded and minimized sessions retain a fixed two-line summary: identity on
/// the first line, then status, clock, and queued work on the second.
#[allow(clippy::too_many_arguments)]
fn compact_session_lines(
    lines: &mut Vec<Line<'static>>,
    prefix: &str,
    session: &SessionRecord,
    detail: Option<&SessionDetail>,
    review: Option<&RuntimeReviewView>,
    unreachable: bool,
    operation: Option<&SessionOperationDisplay>,
    now_epoch_seconds: u64,
    target: &str,
    permission: Option<Span<'static>>,
    spinner: Option<&'static str>,
    width: u16,
) {
    let facts = SessionRowFacts {
        detail,
        unreachable,
        state: session.state,
        now_epoch_seconds,
    };
    let style = facts.style();
    let name = recovery_warning_name(session, session_name(session).to_owned(), now_epoch_seconds);
    // The ellipsis action occupies the last three cells of the first line;
    // retain the full width for the status line below it.
    let title_width = width.saturating_sub(3);
    lines.push(Line::styled(
        format!(
            "{prefix}{}",
            truncate_display_text(
                &name,
                usize::from(title_width).saturating_sub(Line::raw(prefix).width()),
            )
        ),
        style,
    ));
    lines.push(session_activity_line(
        "  ",
        session,
        detail,
        review,
        unreachable,
        operation,
        now_epoch_seconds,
        target,
        permission,
        spinner,
        width,
        false,
    ));
}

/// Shared state used to derive a session's status, activity clock, and colour.
#[derive(Clone, Copy)]
struct SessionRowFacts<'a> {
    detail: Option<&'a SessionDetail>,
    unreachable: bool,
    state: SessionState,
    now_epoch_seconds: u64,
}

impl SessionRowFacts<'_> {
    /// One status symbol is used in expanded, compact, and minimized rows so
    /// lifecycle and attention state remains visible at every width.
    fn status_symbol(
        &self,
        review: Option<&RuntimeReviewView>,
        operation: Option<&SessionOperationDisplay>,
    ) -> &'static str {
        use mj_core::review::driver::TurnReviewPhase;
        use mj_core::review::verdict::ReviewVerdict;

        if operation.is_some() {
            return "◐";
        }
        match self.state {
            SessionState::Lost | SessionState::Error | SessionState::DestroyedWithDataLoss => {
                return "×";
            }
            SessionState::Stopped => return "■",
            SessionState::Provisioning => return "↑",
            SessionState::Checkpointing => return "▣",
            SessionState::Closing => return "↓",
            SessionState::Destroying => return "⊗",
            SessionState::Disconnected => return "?",
            SessionState::Running => {}
        }
        if self.unreachable {
            return "?";
        }
        if self.needs_input() {
            return "!";
        }
        if let Some(review) = review.filter(|review| review.activity_label().is_some()) {
            if review.is_working() {
                return "◐";
            }
            return match &review.phase {
                TurnReviewPhase::Verdict(ReviewVerdict::Clean) => "✓",
                TurnReviewPhase::Verdict(ReviewVerdict::Failed { .. })
                | TurnReviewPhase::Forwarding { error: Some(_), .. } => "×",
                _ => "!",
            };
        }
        let Some(detail) = self.detail else {
            return "·";
        };
        if !detail.activity.is_idle(detail.current_turn_started_at) {
            "◐"
        } else if detail.materialized_applied_event_ordinal.is_none()
            && detail.activity.execution.is_none()
            && detail.activity.idle_since_ms.is_none()
        {
            "·"
        } else if detail.has_unread() {
            "✓"
        } else {
            "○"
        }
    }

    fn style(&self) -> Style {
        Style::default().fg(session_band_color(
            self.detail,
            self.unreachable,
            self.state,
        ))
    }

    fn clock(&self, detailed: bool) -> String {
        let activity = self.detail.map(|detail| &detail.activity);
        activity.unwrap_or(&*EMPTY_ACTIVITY).display_clock(
            self.now_epoch_seconds,
            self.detail
                .and_then(|detail| detail.current_turn_started_at),
            self.detail
                .and_then(|detail| detail.current_step_started_at_ms),
            detailed,
        )
    }

    fn needs_input(&self) -> bool {
        self.detail
            .is_some_and(|detail| !detail.pending_elicitations.is_empty())
    }
}

/// Return the moving value currently used in a session's activity line.
/// Keeping this decision beside the renderer prevents invalidation from
/// inventing a second precedence order for operation, lifecycle, and activity
/// clocks.
pub(crate) fn session_display_clock(
    dashboard: &DashboardState,
    session: &SessionRecord,
    detail: Option<&SessionDetail>,
    review: Option<&RuntimeReviewView>,
    unreachable: bool,
    operation: Option<&SessionOperationDisplay>,
    now_epoch_seconds: u64,
) -> Option<String> {
    let started_at = if let Some(operation) = operation {
        Some(operation_status(operation).1)
    } else if dashboard.transition_kind(&session.id).is_some()
        || dashboard.transition_failure_kind(&session.id).is_some()
        || session.state == SessionState::Provisioning
    {
        session_updated_at_epoch_seconds(session)
    } else {
        None
    };
    if let Some(started_at) = started_at {
        return Some(mj_client::usage_format::format_clock(
            now_epoch_seconds.saturating_sub(started_at),
        ));
    }

    if session.last_error.is_some()
        || unreachable
        || detail.is_some_and(|detail| !detail.pending_elicitations.is_empty())
        || review.is_some_and(|review| review.activity_label().is_some())
        || !session.state.is_active()
    {
        return None;
    }
    let detail = detail?;
    if detail.activity.is_idle(detail.current_turn_started_at)
        || matches!(
            detail.activity.execution,
            Some(mj_core::relay::RelayExecutionState::Closing)
                | Some(mj_core::relay::RelayExecutionState::Closed)
        )
    {
        return None;
    }
    let detailed = dashboard.pane_size(SupportPane::Sessions) != PaneSize::Minimized
        && dashboard.project_is_expanded(session)
        && dashboard.config.advanced.detailed_activity_clocks;
    Some(
        SessionRowFacts {
            detail: Some(detail),
            unreachable,
            state: session.state,
            now_epoch_seconds,
        }
        .clock(detailed),
    )
}

/// The review fields that can alter a session row: its compact activity label
/// and whether the row owns an animation frame. Controller progress text and
/// role details are intentionally omitted because the dashboard does not draw
/// them.
pub(crate) fn session_review_display_signature(
    review: Option<&RuntimeReviewView>,
) -> (Option<&'static str>, bool) {
    (
        review.and_then(RuntimeReviewView::activity_label),
        review.is_some_and(RuntimeReviewView::is_working),
    )
}

/// Select only content authored by the agent for the expanded output rows.
/// The user prompt is a fallback for the compact summary, never an agent
/// excerpt with a misleading prefix.
fn current_agent_excerpt(detail: &SessionDetail) -> Option<&str> {
    if detail.last_agent_message_follows_last_user {
        detail
            .last_agent_message
            .as_deref()
            .or(detail.latest_agent_activity_after_last_user.as_deref())
    } else {
        detail.latest_agent_activity_after_last_user.as_deref()
    }
}

/// The target label shown for each session, in `ordered_sessions()` order.
///
/// A target repeated inside one project is ambiguous on its own, so repeats
/// are numbered `[1]`, `[2]`, … in the order they appear. Every Sessions
/// representation reads from this so labels remain consistent.
fn session_display_targets(dashboard: &DashboardState, sessions: &[&SessionRecord]) -> Vec<String> {
    let mut counts = BTreeMap::<(String, String), usize>::new();
    for session in sessions {
        let key = (
            dashboard.project_source(session).key,
            session_target_label(
                session,
                dashboard.session_operations.get(&session.id),
                &dashboard.config,
            ),
        );
        *counts.entry(key).or_default() += 1;
    }
    let mut occurrences = BTreeMap::<(String, String), usize>::new();
    sessions
        .iter()
        .map(|session| {
            let base = session_target_label(
                session,
                dashboard.session_operations.get(&session.id),
                &dashboard.config,
            );
            let key = (dashboard.project_source(session).key, base.clone());
            let occurrence = occurrences.entry(key.clone()).or_default();
            *occurrence += 1;
            if counts.get(&key).copied().unwrap_or_default() > 1 {
                format!("{base} [{}]", *occurrence)
            } else {
                base
            }
        })
        .collect()
}

/// Content rows the Sessions pane wants, excluding its border.
pub(crate) fn sessions_content_height(dashboard: &DashboardState, width: u16) -> u16 {
    drawn_session_rows(dashboard, width)
        .iter()
        .map(|row| row.content_height().saturating_add(row.spacing))
        .fold(0, u16::saturating_add)
        .saturating_add(SESSION_ACTIONS_HEIGHT)
}

pub(crate) fn minimized_sessions_content_height(dashboard: &DashboardState, width: u16) -> u16 {
    drawn_session_rows_with_options(dashboard, width, SessionRowsRenderOptions::MINIMIZED)
        .iter()
        .map(|row| row.content_height().saturating_add(row.spacing))
        .fold(0, u16::saturating_add)
}

/// The Sessions title keeps the workspace ahead of the long pane label when
/// the screen is narrow, while visible size controls retain their cells.
fn sessions_title(workspace_name: &str, width: u16, maximize_enabled: bool) -> Line<'static> {
    let budget = usize::from(pane_title_content_width(width, maximize_enabled));
    if workspace_name.is_empty() {
        return Line::raw(crate::widgets::truncate_text(" Sessions ", budget));
    }
    let full_prefix = " Sessions · ";
    let prefix = if full_prefix.chars().count() + workspace_name.chars().count() < budget {
        full_prefix
    } else {
        " S · "
    };
    let workspace_room = budget.saturating_sub(prefix.chars().count() + 1);
    Line::from(vec![
        Span::raw(prefix),
        Span::styled(
            crate::widgets::truncate_text(workspace_name, workspace_room),
            Style::default().fg(theme::palette().muted),
        ),
        Span::raw(" "),
    ])
}

/// Reserve a title suffix for the number of pending questions. At narrow
/// widths a compact form keeps that count visible while preserving the
/// Sessions label.
fn sessions_title_with_pending(
    workspace_name: &str,
    width: u16,
    pending_count: usize,
    maximize_enabled: bool,
) -> Line<'static> {
    let base = sessions_title(workspace_name, width, maximize_enabled);
    if pending_count == 0 {
        return base;
    }
    let budget = usize::from(pane_title_content_width(width, maximize_enabled));
    let suffix = Span::styled(
        format!(" · Needs input: {pending_count}"),
        Style::default()
            .fg(theme::palette().session_attention)
            .add_modifier(Modifier::BOLD),
    );
    if base.width().saturating_add(suffix.width()) <= budget {
        let mut spans = base.spans;
        spans.push(suffix);
        return Line::from(spans);
    }
    let compact = Line::styled(
        format!(" Sessions [{pending_count}]"),
        Style::default()
            .fg(theme::palette().session_attention)
            .add_modifier(Modifier::BOLD),
    );
    if compact.width() <= budget {
        return compact;
    }
    let tiny = Line::styled(
        format!(" Q{pending_count}"),
        Style::default()
            .fg(theme::palette().session_attention)
            .add_modifier(Modifier::BOLD),
    );
    if tiny.width() <= budget {
        return tiny;
    }
    base
}

fn sessions_block(
    focused: bool,
    workspace_name: &str,
    width: u16,
    size: PaneSize,
    pending_count: usize,
    maximize_enabled: bool,
) -> Block<'static> {
    theme::panel(focused)
        .title(sessions_title_with_pending(
            workspace_name,
            width,
            if size == PaneSize::Minimized {
                pending_count
            } else {
                0
            },
            maximize_enabled,
        ))
        .title(pane_size_controls(size, maximize_enabled))
}

/// Draws the Sessions pane and reports the per-row mouse hitboxes.
pub(crate) fn render_sessions(
    frame: &mut Frame,
    area: Rect,
    dashboard: &DashboardState,
) -> SessionRowsRendered {
    let content = area.inner(Margin {
        horizontal: 1,
        vertical: 1,
    });
    // Keep three cells at the right edge clear for the session ellipsis
    // control on each session's title line. The activity and output lines
    // use the full content width; a narrow sidebar must not hide their clock
    // or queued count behind the action button.
    let actions_area = Rect::new(
        content.x,
        content.y,
        content.width,
        content.height.min(SESSION_ACTIONS_HEIGHT),
    );
    let rows_area = Rect::new(
        content.x,
        content.y.saturating_add(SESSION_ACTIONS_HEIGHT),
        content.width,
        content.height.saturating_sub(SESSION_ACTIONS_HEIGHT),
    );
    let width = content.width;
    let drawn = if dashboard.sessions_minimized() {
        drawn_session_rows_with_options(dashboard, width, SessionRowsRenderOptions::MINIMIZED)
    } else {
        drawn_session_rows(dashboard, width)
    };
    let focused = dashboard.focus() == Focus::Sessions;
    frame.render_widget(
        sessions_block(
            focused,
            "",
            area.width,
            dashboard.pane_size(SupportPane::Sessions),
            dashboard.pending_input_count(),
            dashboard.pane_maximize_enabled(SupportPane::Sessions),
        ),
        area,
    );
    crate::surface_controls::render_session_buttons(frame, actions_area, dashboard);
    let table = Table::new(
        drawn.iter().map(|row| {
            Row::new([Cell::from(Text::from(row.lines.clone()))])
                .height(row.content_height())
                .bottom_margin(row.spacing)
        }),
        [Constraint::Min(1)],
    );
    let selected = dashboard
        .selected_visible_index()
        .filter(|index| *index < drawn.len());
    let mut offset = dashboard.sessions_scroll.get();
    if let (Some(selected), Some(direction)) =
        (selected, take_scroll_lookahead(dashboard, Focus::Sessions))
    {
        let row_heights = drawn
            .iter()
            .map(|row| usize::from(row.content_height().saturating_add(row.spacing)))
            .collect::<Vec<_>>();
        offset = offset_with_directional_lookahead(
            offset,
            selected,
            direction,
            &row_heights,
            usize::from(rows_area.height),
        );
    }
    let mut state = TableState::default()
        .with_offset(offset)
        .with_selected(selected);
    frame.render_stateful_widget(table, rows_area, &mut state);
    // The table scrolled only as far as it had to; remember where it settled
    // so the next frame does not scroll back to the top.
    dashboard.sessions_scroll.set(state.offset());

    let offset = state.offset();
    let mut row_y = rows_area.y;
    let mut visible = 0;
    let mut session_row_areas = Vec::new();
    let mut project_heading_areas = Vec::new();
    for row in drawn.iter().skip(offset) {
        if row_y >= rows_area.bottom() {
            break;
        }
        visible += 1;
        let heading_rows = u16::from(row.heading.is_some());
        if let Some(key) = row.heading.clone() {
            project_heading_areas.push((key, Rect::new(rows_area.x, row_y, rows_area.width, 1)));
        }
        if let Some(index) = row.session {
            let session_y = row_y.saturating_add(heading_rows);
            let height = row.content_height().saturating_sub(heading_rows);
            session_row_areas.push((
                index,
                Rect::new(
                    rows_area.x,
                    session_y,
                    rows_area.width,
                    height.min(rows_area.bottom().saturating_sub(session_y)),
                ),
            ));
        }
        row_y = row_y.saturating_add(row.content_height().saturating_add(row.spacing));
    }
    render_session_scrollbar(frame, area, drawn.len(), offset, visible);

    SessionRowsRendered {
        session_row_areas,
        project_heading_areas,
    }
}

/// The single row used while a lifecycle owns a session. It deliberately
/// contains no transcript excerpt: the identity, operation, active stages,
/// and elapsed time remain stable across expanded, collapsed, and minimized
/// layouts while another session can still be selected and used.
#[allow(clippy::too_many_arguments)]
fn session_transition_line(
    prefix: &str,
    session: &SessionRecord,
    transition: SessionTransitionKind,
    operation: Option<&SessionOperationDisplay>,
    now_epoch_seconds: u64,
    target: &str,
    width: u16,
    config: &Config,
    failure: Option<&str>,
) -> Line<'static> {
    let started_at = operation
        .map(|operation| {
            operation
                .active_stages
                .values()
                .copied()
                .min()
                .unwrap_or(operation.started_at_epoch_seconds)
        })
        .or_else(|| session_updated_at_epoch_seconds(session))
        .unwrap_or(now_epoch_seconds);
    let stages = operation
        .map(|operation| {
            operation
                .active_stages
                .keys()
                .map(|stage| stage.label())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .filter(|stages| !stages.is_empty())
        .unwrap_or_else(|| "waiting".to_owned());
    let elapsed =
        mj_client::usage_format::format_clock(now_epoch_seconds.saturating_sub(started_at));
    let (profile, _) = operation
        .and_then(|operation| operation.resume_destination.clone())
        .unwrap_or_else(|| {
            (
                session.last_profile.clone(),
                session.target_template_id.clone(),
            )
        });
    let identity = format!(
        "{} · {}",
        session.project_name(config),
        session_name(session)
    );
    let line = format!(
        "{prefix}{target}  {} · {stages} · {elapsed}  {profile} · {identity}{}",
        transition.label(),
        failure.map_or_else(String::new, |error| format!(" · failed: {error}"))
    );
    Line::styled(
        crate::widgets::truncate_text(&line, usize::from(width.saturating_sub(3))),
        Style::default()
            .fg(if failure.is_some() {
                theme::palette().session_error
            } else {
                theme::palette().session_activity
            })
            .add_modifier(Modifier::BOLD),
    )
}

/// A session the dashboard has heard nothing operational about yet.
static EMPTY_ACTIVITY: std::sync::LazyLock<mj_client::usage_format::SessionActivity> =
    std::sync::LazyLock::new(mj_client::usage_format::SessionActivity::default);

fn session_target_label(
    session: &SessionRecord,
    operation: Option<&SessionOperationDisplay>,
    config: &Config,
) -> String {
    let target_id = operation
        .and_then(|operation| operation.resume_destination.as_ref())
        .map(|(_, target_id)| target_id)
        .unwrap_or(&session.target_template_id);
    session.project_target(config, target_id)
}

fn session_permission_badge(
    session: &SessionRecord,
    operation: Option<&SessionOperationDisplay>,
    config: &Config,
) -> Option<Span<'static>> {
    let target_id = operation
        .and_then(|operation| operation.resume_destination.as_ref())
        .map(|(_, target_id)| target_id)
        .unwrap_or(&session.target_template_id);
    config
        .targets
        .get(target_id)
        .and_then(|target| permission_badge(target.permission_mode()))
}

fn permission_badge(mode: Option<PermissionMode>) -> Option<Span<'static>> {
    mode.map(|mode| match mode {
        PermissionMode::Guardian => Span::styled(
            "[G]",
            Style::default()
                .fg(theme::palette().success)
                .add_modifier(Modifier::BOLD),
        ),
        PermissionMode::Yolo => Span::styled(
            "[Y]",
            Style::default()
                .fg(theme::palette().error)
                .add_modifier(Modifier::BOLD),
        ),
    })
}

fn capacity_target_labels(target_ids: &[String], config: &Config) -> Line<'static> {
    let mut spans = Vec::new();
    for (index, target_id) in target_ids.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw(", "));
        }
        spans.push(Span::raw(target_id.clone()));
        if let Some(badge) = config
            .targets
            .get(target_id)
            .and_then(|target| permission_badge(target.permission_mode()))
        {
            spans.push(Span::raw(" "));
            spans.push(badge);
        }
    }
    Line::from(spans)
}

pub(crate) fn render_session_scrollbar(
    frame: &mut Frame,
    area: Rect,
    content_length: usize,
    position: usize,
    viewport_content_length: usize,
) {
    if area.width == 0 || content_length <= viewport_content_length {
        return;
    }
    let track = Rect::new(
        area.right().saturating_sub(1),
        area.y.saturating_add(1),
        1,
        area.height.saturating_sub(2),
    );
    if let Some(geometry) =
        scrollbar_geometry(track, content_length, position, viewport_content_length)
    {
        render_scrollbar(frame, geometry);
    }
}

pub(crate) fn operation_status(operation: &SessionOperationDisplay) -> (String, u64) {
    if matches!(
        operation.kind,
        SessionOperationKind::Launching
            | SessionOperationKind::Resuming
            | SessionOperationKind::Moving
    ) && !operation.active_stages.is_empty()
    {
        let label = operation
            .active_stages
            .keys()
            .map(|stage| stage.label())
            .collect::<Vec<_>>()
            .join(", ");
        let started_at = operation
            .active_stages
            .values()
            .copied()
            .min()
            .unwrap_or(operation.started_at_epoch_seconds);
        (label, started_at)
    } else {
        (
            operation.kind.label().to_owned(),
            operation.started_at_epoch_seconds,
        )
    }
}

pub(crate) fn session_updated_at_epoch_seconds(session: &SessionRecord) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(&session.updated_at)
        .ok()?
        .timestamp()
        .try_into()
        .ok()
}

fn session_name(session: &SessionRecord) -> &str {
    session.display_title()
}

/// Maps the controller's review projection to the short overlay that fits in
/// every session row. The controller owns the detailed wording and verdict;
/// the TUI only compresses that authoritative view for the list.
fn review_status_label(review: Option<&RuntimeReviewView>) -> Option<&'static str> {
    review.and_then(RuntimeReviewView::activity_label)
}

/// The colour a session's summary rows carry.
///
/// Red means the session needs attention rather than reading: its relay is
/// unreachable, or the session itself failed. A live, truly idle session is
/// blue even after its messages have been read. Pending questions remain an
/// attention signal, and lifecycle states do not claim to be idle.
fn session_band_color(
    detail: Option<&SessionDetail>,
    unreachable: bool,
    state: SessionState,
) -> Color {
    if unreachable || state == SessionState::Error {
        return theme::palette().session_error;
    }
    let Some(detail) = detail else {
        return theme::palette().session_activity;
    };
    if !detail.pending_elicitations.is_empty() {
        return theme::palette().session_attention;
    }
    if state == SessionState::Running && detail.activity.is_idle(detail.current_turn_started_at) {
        return theme::palette().session_idle;
    }
    if detail.has_unread() {
        return theme::palette().session_attention;
    }
    theme::palette().session_activity
}

pub(crate) fn checkpoint_age(now_epoch_seconds: u64, checkpointed_at: &str) -> String {
    let Ok(checkpointed_at) = chrono::DateTime::parse_from_rfc3339(checkpointed_at) else {
        return "unknown".into();
    };
    let checkpointed_at = checkpointed_at.timestamp().max(0) as u64;
    let age = now_epoch_seconds.saturating_sub(checkpointed_at);
    if age < 60 {
        format!("{age}s")
    } else if age < 3_600 {
        format!("{}m", age / 60)
    } else if age < 86_400 {
        format!("{}h", age / 3_600)
    } else {
        format!("{}d", age / 86_400)
    }
}

fn recovery_warning_name(session: &SessionRecord, name: String, now_epoch_seconds: u64) -> String {
    if session.last_checkpoint_error.is_none() {
        return name;
    }
    match &session.checkpoint {
        Some(checkpoint) => format!(
            "{name}  ⚠ Recovery copy {} old",
            checkpoint_age(now_epoch_seconds, &checkpoint.created_at)
        ),
        None => format!("{name}  ⚠ Recovery unavailable"),
    }
}

/// How many machines an EC2 fleet is running, as the fleet's answer to the
/// "In Use" question.
///
/// A fleet gets one probe per live instance, so the probe list is the fleet's
/// size. A fleet has no CPU percentage of its own, and how many machines are
/// up is what it costs.
fn fleet_vm_label(detail: &CapacityDetail) -> String {
    let count = detail.target.probes.len();
    format!("{count} VM{}", if count == 1 { "" } else { "s" })
}

/// A reading older than this stopped tracking the host: the poller samples
/// every 30 seconds, so three missed rounds mean the number on screen is no
/// longer what the host is doing.
pub(crate) const CAPACITY_SAMPLE_STALE_AFTER_SECONDS: u64 = 90;

/// Why the row's reading cannot be trusted, if it cannot: a probe that failed,
/// or a sample that stopped refreshing. `None` means the reading is current.
pub(crate) fn capacity_staleness(
    detail: &CapacityDetail,
    now_epoch_seconds: u64,
) -> Option<String> {
    if let Some(error) = &detail.probe_error {
        return Some(format!("stale: {error}"));
    }
    let sampled_at = detail.sampled_at_epoch_seconds?;
    (now_epoch_seconds.saturating_sub(sampled_at) > CAPACITY_SAMPLE_STALE_AFTER_SECONDS).then(
        || {
            format!(
                "stale: sampled {}",
                refresh_age(now_epoch_seconds, sampled_at)
            )
        },
    )
}

struct CapacityTableRow {
    host: String,
    targets: Line<'static>,
    in_use: Line<'static>,
}

fn capacity_table_rows(
    dashboard: &DashboardState,
    now_epoch_seconds: u64,
) -> Vec<CapacityTableRow> {
    dashboard
        .capacity_details
        .values()
        .map(|detail| {
            let capacity = if detail.refreshing {
                "refreshing…".into()
            } else {
                match (&detail.target.kind, &detail.usage) {
                    (DeploymentCapacityKind::Host, Some(usage)) => {
                        let memory_percent = if usage.memory_total_bytes == 0 {
                            0
                        } else {
                            (u128::from(usage.memory_used_bytes) * 100
                                / u128::from(usage.memory_total_bytes))
                            .min(100)
                        };
                        format!(
                            "{}% CPU · {memory_percent}% RAM",
                            usage.cpu_percent.unwrap_or(0)
                        )
                    }
                    (DeploymentCapacityKind::AwsFleet, Some(usage)) => format!(
                        "{} · {} cores · {} RAM · {} disk",
                        fleet_vm_label(detail),
                        usage.logical_cores,
                        format_resource_bytes(usage.memory_total_bytes),
                        format_resource_bytes(usage.disk_total_bytes.unwrap_or(0))
                    ),
                    // A fleet with nothing running has no capacity figures,
                    // and the count is the whole answer.
                    (DeploymentCapacityKind::AwsFleet, None) if detail.on_demand => {
                        fleet_vm_label(detail)
                    }
                    _ => "unavailable".into(),
                }
            };
            let mut in_use = vec![Span::raw(capacity)];
            if let Some(staleness) = capacity_staleness(detail, now_epoch_seconds) {
                in_use.push(Span::styled(
                    format!("  · {staleness}"),
                    Style::default().fg(theme::palette().muted),
                ));
            }
            CapacityTableRow {
                host: detail.target.host.clone(),
                targets: capacity_target_labels(&detail.target.target_ids, &dashboard.config),
                in_use: Line::from(in_use),
            }
        })
        .collect()
}

fn capacity_column_widths(rows: &[CapacityTableRow]) -> [u16; 3] {
    [
        quota_column_width(
            "Host / fleet",
            rows.iter().map(|row| Line::raw(row.host.as_str()).width()),
            u16::MAX,
        ),
        quota_column_width(
            "Targets",
            rows.iter().map(|row| row.targets.width()),
            u16::MAX,
        ),
        quota_column_width(
            "In Use",
            rows.iter().map(|row| row.in_use.width()),
            u16::MAX,
        ),
    ]
}

/// Width needed to draw the complete Targets table, including its table
/// spacing, border, and always-present selection marker.
pub(crate) fn capacity_table_width(dashboard: &DashboardState) -> u16 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let widths = capacity_column_widths(&capacity_table_rows(dashboard, now));
    widths
        .into_iter()
        .fold(0_u16, u16::saturating_add)
        .saturating_add(4) // two spaces between each of the three columns
        .saturating_add(4) // two borders and two highlight cells
}

pub(crate) fn render_capacity(
    frame: &mut Frame,
    area: Rect,
    dashboard: &mut DashboardState,
    size: Option<PaneSize>,
) {
    let now_epoch_seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let rows = capacity_table_rows(dashboard, now_epoch_seconds);
    let column_widths = capacity_column_widths(&rows);
    let focused = dashboard.focus == Focus::Targets;
    let block = theme::panel(focused).title(" Targets ");
    let block = size.map_or(block.clone(), |size| {
        block.title(pane_size_controls(
            size,
            dashboard.pane_maximize_enabled(SupportPane::Targets),
        ))
    });
    let table = Table::new(
        rows.into_iter().map(|row| {
            Row::new([
                Cell::from(row.host),
                Cell::from(row.targets),
                Cell::from(row.in_use),
            ])
        }),
        column_widths.map(Constraint::Length),
    )
    .column_spacing(2)
    .header(
        Row::new(["Host / fleet", "Targets", "In Use"])
            .style(theme::muted().add_modifier(Modifier::BOLD)),
    )
    .row_highlight_style(if focused {
        theme::selection(true)
    } else {
        Style::default()
    })
    .highlight_symbol(if focused { "› " } else { "  " })
    .highlight_spacing(HighlightSpacing::Always)
    .block(block);
    let mut offset = dashboard.targets_scroll.get();
    if let Some(direction) = take_scroll_lookahead(dashboard, Focus::Targets) {
        let row_heights = vec![1; dashboard.capacity_details.len()];
        offset = offset_with_directional_lookahead(
            offset,
            dashboard.capacity_index,
            direction,
            &row_heights,
            usize::from(area.height.saturating_sub(SESSION_TABLE_CHROME_HEIGHT)),
        );
    }
    let mut state = TableState::default().with_offset(offset).with_selected(
        (!dashboard.capacity_details.is_empty()).then_some(dashboard.capacity_index),
    );
    frame.render_stateful_widget(table, area, &mut state);
    dashboard.targets_scroll.set(state.offset());
    render_session_scrollbar(
        frame,
        area,
        dashboard.capacity_details.len(),
        state.offset(),
        usize::from(area.height.saturating_sub(SESSION_TABLE_CHROME_HEIGHT)),
    );
}

/// The colour the quota bar gives a percentage of headroom left.
///
/// Both minimized panes read the same scale, which is why it lives in one
/// place: a quota reports the headroom it has left directly, and a CPU reading
/// is the inverse — a busy host has little left.
fn headroom_color(headroom_percent: u8) -> Color {
    match headroom_percent {
        0..=20 => theme::palette().error,
        21..=50 => theme::palette().warning,
        _ => theme::palette().success,
    }
}

/// One reading in a minimized pane: a name, its value, and how healthy the
/// value is. A reading with no health to report draws in the ordinary
/// foreground rather than claiming a colour it has not earned.
struct SummaryReading {
    name: String,
    value: String,
    color: Option<Color>,
}

/// One row summarising every target host and its CPU load, for the minimized
/// Targets pane.
///
/// A reading that cannot be trusted says so rather than showing a number: an
/// unavailable probe, a sample that stopped refreshing, and a fleet reading
/// that carries no CPU figure are all named explicitly.
pub(crate) fn minimized_targets_line(
    dashboard: &DashboardState,
    width: u16,
    focused: bool,
) -> Line<'static> {
    let now_epoch_seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let readings = dashboard
        .capacity_details
        .values()
        .map(|detail| {
            if detail.refreshing {
                return SummaryReading {
                    name: detail.target.host.clone(),
                    value: "refreshing…".into(),
                    color: None,
                };
            }
            let (value, cpu_percent) = match (&detail.target.kind, &detail.usage) {
                (DeploymentCapacityKind::Host, Some(usage)) => match usage.cpu_percent {
                    Some(cpu) => (format!("{cpu}%"), Some(cpu)),
                    None => ("no CPU".to_string(), None),
                },
                // A fleet has no CPU percentage of its own, so what it reports
                // is how many machines it is running.
                (DeploymentCapacityKind::AwsFleet, Some(_)) => (fleet_vm_label(detail), None),
                (DeploymentCapacityKind::AwsFleet, None) if detail.on_demand => {
                    (fleet_vm_label(detail), None)
                }
                _ => ("unavailable".to_string(), None),
            };
            let stale = capacity_staleness(detail, now_epoch_seconds).is_some();
            SummaryReading {
                name: detail.target.host.clone(),
                value: if stale {
                    format!("{value} (stale)")
                } else {
                    value
                },
                // A busy host has little headroom, so the scale runs the other
                // way round from a quota's.
                color: cpu_percent
                    .filter(|_| !stale)
                    .map(|cpu| headroom_color(100_u8.saturating_sub(cpu))),
            }
        })
        .collect::<Vec<_>>();
    summary_row("Targets", &readings, width, focused)
}

/// One row summarising every profile's quota, for the minimized Quota pane.
///
/// The figures are percentages *remaining*, which is the number the full
/// pane's bar prints beside itself: an exhausted profile reads 0% in both. A
/// profile with weekly headroom to spare reads its weekly figure alone
/// (`claude 100%`); once the weekly window has been dipped into and a
/// five-hour window is reported too, both appear as `weekly%/5h%` (`claude
/// 96%/40%`), because a full week is no comfort while the next five hours are
/// spent. The colour follows the tighter of the two figures, so the two panes
/// agree about when a profile is in trouble.
///
/// Usage-priced profiles are left out of the row entirely: they bill per token
/// and have no window to summarise, so a placeholder would only spend width.
pub(crate) fn minimized_quota_line(
    dashboard: &DashboardState,
    width: u16,
    focused: bool,
) -> Line<'static> {
    let readings = dashboard
        .config
        .enabled_profiles()
        .filter_map(|(id, _profile)| {
            let quota = dashboard.quotas.get(id);
            // The report says for itself that it is usage-priced.
            let usage_priced = quota.is_some_and(|quota| quota.is_usage_priced());
            if usage_priced {
                return None;
            }
            if dashboard.quota_refreshing.contains(id) {
                return Some(SummaryReading {
                    name: id.to_owned(),
                    value: "refreshing…".into(),
                    color: None,
                });
            }
            let quota = quota.filter(|quota| quota.error.is_none());
            let Some(weekly) = quota
                .and_then(ProfileQuota::weekly_window)
                .and_then(quota_remaining_percent)
            else {
                return Some(SummaryReading {
                    name: id.to_owned(),
                    value: "unavailable".into(),
                    color: None,
                });
            };
            // An untouched weekly window says everything there is to say; the
            // five-hour figure only matters once the week is being spent.
            let five_hour = quota
                .filter(|_| weekly < 100)
                .and_then(ProfileQuota::five_hour_window)
                .and_then(quota_remaining_percent);
            let value = match five_hour {
                Some(five_hour) => format!("{weekly}%/{five_hour}%"),
                None => format!("{weekly}%"),
            };
            Some(SummaryReading {
                name: id.to_owned(),
                value,
                color: Some(headroom_color(
                    five_hour.map_or(weekly, |five_hour| weekly.min(five_hour)),
                )),
            })
        })
        .collect::<Vec<_>>();
    summary_row("Quota", &readings, width, focused)
}

/// A minimized pane's single row, drawn as the pane's own title so it keeps
/// the rule the full pane has. Readings are comma-separated and truncated
/// rather than wrapped, because the row is one row.
fn summary_row(
    label: &str,
    readings: &[SummaryReading],
    width: u16,
    _focused: bool,
) -> Line<'static> {
    // A minimized pane is still a pane, so its one row opens the way a
    // bordered one does and the rule carries on between the label and the
    // readings: `─ Quota ── claude-1 63% ────`.
    let (opening, divider) = ("─ ", " ── ");
    let mut spans = vec![Span::raw(format!("{opening}{label}{divider}"))];
    let mut used = opening.chars().count() + label.chars().count() + divider.chars().count();
    // Leave room for the rule the title runs into, so the readings never push
    // it off the row and it never butts straight against a value.
    let budget = usize::from(width).saturating_sub(4);
    if readings.is_empty() {
        spans.push(Span::raw("none configured "));
        return Line::from(spans);
    }
    for (index, reading) in readings.iter().enumerate() {
        let separator = if index == 0 { "" } else { ", " };
        let text = format!("{separator}{} {}", reading.name, reading.value);
        let text_width = text.chars().count();
        if used + text_width > budget {
            spans.push(Span::raw(if index == 0 { "… " } else { ", … " }));
            return Line::from(spans);
        }
        used += text_width;
        if !separator.is_empty() {
            spans.push(Span::raw(separator));
        }
        spans.push(Span::raw(format!("{} ", reading.name)));
        spans.push(match reading.color {
            Some(color) => Span::styled(reading.value.clone(), Style::default().fg(color)),
            None => Span::raw(reading.value.clone()),
        });
    }
    // The rule picks up where the title stops, so the title closes with a
    // space the way a bordered pane's does.
    spans.push(Span::raw(" "));
    Line::from(spans)
}

fn quota_remaining_percent(window: &QuotaWindow) -> Option<u8> {
    window
        .remaining_percent
        .map(|value| value.min(100))
        .or_else(|| {
            let (Some(used), Some(limit)) = (window.used, window.limit) else {
                return None;
            };
            if limit <= 0 {
                return None;
            }
            let remaining = i128::from(limit.saturating_sub(used).clamp(0, limit));
            Some((remaining * 100 / i128::from(limit)) as u8)
        })
}

const EMPTY_QUOTA_CELL: &str = " ";
const QUOTA_CHART_LEFT_BORDER: &str = "▕";
const QUOTA_CHART_RIGHT_BORDER: &str = "▏";
// Both bar kinds occupy the same column, so they must agree on the cell count.
const QUOTA_BAR_CELLS: usize = 10;

fn quota_chart_border_style() -> Style {
    Style::default()
        .fg(theme::palette().muted)
        .bg(theme::palette().background)
}

fn quota_bar(window: Option<&QuotaWindow>) -> Line<'static> {
    const CELLS: usize = QUOTA_BAR_CELLS;
    const EIGHTHS_PER_CELL: usize = 8;
    let Some(remaining) = window.and_then(quota_remaining_percent) else {
        return Line::default();
    };
    let eighths = (usize::from(remaining) * CELLS * EIGHTHS_PER_CELL + 50) / 100;
    let full_cells = eighths / EIGHTHS_PER_CELL;
    let partial_eighths = eighths % EIGHTHS_PER_CELL;
    let partial = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉"][partial_eighths];
    let empty_cells = CELLS
        .saturating_sub(full_cells)
        .saturating_sub(usize::from(partial_eighths > 0));
    let color = match remaining {
        0..=20 => theme::palette().error,
        21..=50 => theme::palette().warning,
        _ => theme::palette().success,
    };
    let bar_style = Style::default()
        .fg(color)
        .bg(theme::palette().background)
        .add_modifier(Modifier::BOLD);
    Line::from(vec![
        Span::styled("█".repeat(full_cells), bar_style),
        Span::styled(partial.to_string(), bar_style),
        Span::styled(
            EMPTY_QUOTA_CELL.repeat(empty_cells),
            Style::default().bg(theme::palette().background),
        ),
        // The percentage follows the chart rail without an extra separator.
        Span::styled(QUOTA_CHART_RIGHT_BORDER, quota_chart_border_style()),
        Span::styled(
            format!("{remaining}%"),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
    ])
}

/// Renders the API label in the same black, bordered field as capacity charts.
fn api_quota_bar() -> Line<'static> {
    let label = API_LABEL;
    let label_cells = label.chars().count().min(QUOTA_BAR_CELLS);
    let left = (QUOTA_BAR_CELLS - label_cells) / 2;
    let right = QUOTA_BAR_CELLS - label_cells - left;
    let field = Style::default().bg(theme::palette().background);
    Line::from(vec![
        Span::styled(EMPTY_QUOTA_CELL.repeat(left), field),
        Span::styled(label, field.add_modifier(Modifier::BOLD)),
        Span::styled(EMPTY_QUOTA_CELL.repeat(right), field),
        Span::styled(QUOTA_CHART_RIGHT_BORDER, quota_chart_border_style()),
    ])
}

fn weekly_quota_exhausted(quota: &ProfileQuota) -> bool {
    quota
        .weekly_window()
        .and_then(quota_remaining_percent)
        .is_some_and(|remaining| remaining < 1)
}

fn five_hour_quota_bar(quota: &ProfileQuota) -> Line<'static> {
    let five_hour = if weekly_quota_exhausted(quota) {
        None
    } else {
        quota.five_hour_window()
    };
    quota_bar(five_hour)
}

fn quota_reset_countdown(now: u64, reset_at_epoch_seconds: i64) -> String {
    let Ok(reset) = u64::try_from(reset_at_epoch_seconds) else {
        return "now".into();
    };
    let remaining = reset.saturating_sub(now);
    if remaining == 0 {
        return "now".into();
    }

    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;
    if remaining >= DAY {
        let days = remaining / DAY;
        let hours = remaining % DAY / HOUR;
        format!("{days}d {hours}h")
    } else if remaining >= HOUR {
        let hours = remaining / HOUR;
        let minutes = remaining % HOUR / MINUTE;
        if hours == 1 && minutes > 0 {
            format!("{hours}h {minutes}m")
        } else {
            format!("{hours}h")
        }
    } else if remaining >= MINUTE {
        format!("{}m", remaining / MINUTE)
    } else {
        "<1m".into()
    }
}

fn five_hour_quota_reset_countdown(now: u64, reset_at_epoch_seconds: i64) -> String {
    let Ok(reset) = u64::try_from(reset_at_epoch_seconds) else {
        return "now".into();
    };
    let remaining = reset.saturating_sub(now);
    if remaining == 0 {
        "now".into()
    } else if remaining < 60 {
        "<1m".into()
    } else if remaining < 60 * 60 {
        format!("{}m", remaining / 60)
    } else {
        let hours = remaining / (60 * 60);
        let minutes = remaining % (60 * 60) / 60;
        format!("{hours}h {minutes}m")
    }
}

pub(crate) fn quota_reset_cell(window: Option<&QuotaWindow>, now: u64) -> String {
    let Some(window) = window else {
        return String::new();
    };
    window
        .resets_at_epoch_seconds
        .map(|reset| quota_reset_countdown(now, reset))
        .or_else(|| window.resets.clone())
        .unwrap_or_default()
}

pub(crate) fn quota_reset_cells(quota: &ProfileQuota, now: u64) -> (String, String) {
    let mut weekly = quota_reset_cell(quota.weekly_window(), now);
    if let Some(extra) = quota.extra.as_deref() {
        if !weekly.is_empty() {
            weekly.push_str(" · ");
        }
        weekly.push_str(extra);
    }
    let five_hour = if weekly_quota_exhausted(quota) {
        String::new()
    } else {
        quota
            .five_hour_window()
            .map(|window| {
                window
                    .resets_at_epoch_seconds
                    .map(|reset| five_hour_quota_reset_countdown(now, reset))
                    .or_else(|| window.resets.clone())
                    .unwrap_or_default()
            })
            .unwrap_or_default()
    };
    (weekly, five_hour)
}

struct QuotaTableRow {
    profile: String,
    harness: String,
    weekly: Line<'static>,
    weekly_reset: String,
    five_hour: Line<'static>,
    five_hour_reset: String,
}

impl QuotaTableRow {
    fn into_row(self) -> Row<'static> {
        Row::new([
            Cell::from(self.profile),
            Cell::from(self.harness),
            Cell::from(self.weekly),
            Cell::from(self.weekly_reset),
            Cell::from(self.five_hour),
            Cell::from(self.five_hour_reset),
        ])
    }
}

fn quota_chart(mut chart: Line<'static>, chart_present: bool) -> Line<'static> {
    let mut spans = Vec::new();
    if chart_present {
        spans.push(Span::styled(
            QUOTA_CHART_LEFT_BORDER,
            quota_chart_border_style(),
        ));
    }
    spans.append(&mut chart.spans);
    Line::from(spans)
}

/// Trailing room after each content column. Quota-window resets sit one cell
/// after their percentage; the larger table groups retain two-cell gaps.
const QUOTA_COLUMN_GAPS: [u16; 6] = [2, 2, 1, 2, 1, 0];

fn quota_column_width(
    header: &str,
    content_widths: impl Iterator<Item = usize>,
    maximum: u16,
) -> u16 {
    let width = content_widths.fold(Line::raw(header).width(), usize::max);
    u16::try_from(width).unwrap_or(u16::MAX).min(maximum)
}

fn quota_table_rows(dashboard: &DashboardState, now: u64) -> Vec<QuotaTableRow> {
    dashboard
        .config
        .enabled_profiles()
        .map(|(id, profile)| {
            let (weekly, weekly_reset, five_hour, five_hour_reset, weekly_chart, five_hour_chart) =
                if dashboard
                    .quotas
                    .get(id)
                    .is_some_and(|quota| quota.is_usage_priced())
                {
                    (
                        api_quota_bar(),
                        String::new(),
                        Line::default(),
                        String::new(),
                        true,
                        false,
                    )
                } else if dashboard.quota_refreshing.contains(id) {
                    (
                        Line::raw("refreshing…"),
                        String::new(),
                        Line::default(),
                        String::new(),
                        false,
                        false,
                    )
                } else {
                    match dashboard.quotas.get(id) {
                        Some(quota) if quota.error.is_none() => {
                            let (weekly_reset, five_hour_reset) = quota_reset_cells(quota, now);
                            let weekly = quota_bar(quota.weekly_window());
                            let five_hour = five_hour_quota_bar(quota);
                            let weekly_chart = !weekly.spans.is_empty();
                            let five_hour_chart = !five_hour.spans.is_empty();
                            (
                                weekly,
                                weekly_reset,
                                five_hour,
                                five_hour_reset,
                                weekly_chart,
                                five_hour_chart,
                            )
                        }
                        Some(quota) => (
                            Line::raw(quota.error_label().unwrap_or_else(|| "unavailable".into())),
                            String::new(),
                            Line::default(),
                            String::new(),
                            false,
                            false,
                        ),
                        None => (
                            Line::raw("refreshing…"),
                            String::new(),
                            Line::default(),
                            String::new(),
                            false,
                            false,
                        ),
                    }
                };
            QuotaTableRow {
                profile: id.to_owned(),
                harness: profile.kind.display_name().into(),
                weekly: quota_chart(weekly, weekly_chart),
                weekly_reset,
                five_hour: quota_chart(five_hour, five_hour_chart),
                five_hour_reset,
            }
        })
        .collect()
}

fn quota_table_column_widths(rows: &[QuotaTableRow]) -> [u16; 6] {
    [
        quota_column_width(
            "Profile",
            rows.iter()
                .map(|row| Line::raw(row.profile.as_str()).width()),
            u16::MAX,
        ),
        quota_column_width(
            "Harness",
            rows.iter()
                .map(|row| Line::raw(row.harness.as_str()).width()),
            u16::MAX,
        ),
        quota_column_width(
            "Weekly",
            rows.iter().map(|row| row.weekly.width()),
            u16::MAX,
        ),
        quota_column_width(
            "Resets",
            rows.iter()
                .map(|row| Line::raw(row.weekly_reset.as_str()).width()),
            u16::MAX,
        ),
        quota_column_width("5H", rows.iter().map(|row| row.five_hour.width()), u16::MAX),
        quota_column_width(
            "Resets",
            rows.iter()
                .map(|row| Line::raw(row.five_hour_reset.as_str()).width()),
            u16::MAX,
        ),
    ]
}

/// Width needed to draw the complete Quota table, including its
/// inter-column spacing, border, and always-present selection marker.
pub(crate) fn quota_table_width(dashboard: &DashboardState) -> u16 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let content_widths = quota_table_column_widths(&quota_table_rows(dashboard, now));
    content_widths
        .into_iter()
        .fold(0_u16, u16::saturating_add)
        .saturating_add(QUOTA_COLUMN_GAPS.into_iter().sum())
        .saturating_add(4) // two borders and two highlight cells
}

pub(crate) fn render_quotas(
    frame: &mut Frame,
    area: Rect,
    dashboard: &mut DashboardState,
    size: Option<PaneSize>,
) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let rows = quota_table_rows(dashboard, now);
    let refresh_status = if !dashboard.quota_refreshing.is_empty() {
        "refreshing…".to_string()
    } else {
        dashboard
            .quotas
            .values()
            .map(|quota| quota.refreshed_at_epoch_seconds)
            .min()
            .map(|refreshed| format!("refreshed {}", refresh_age(now, refreshed)))
            .unwrap_or_else(|| "not refreshed".to_string())
    };
    let label = " Quota ";
    let title_budget = size.map_or_else(
        || area.width.saturating_sub(2),
        |_| {
            pane_title_content_width(
                area.width,
                dashboard.pane_maximize_enabled(SupportPane::Quota),
            )
        },
    );
    let status_budget = usize::from(title_budget).saturating_sub(label.chars().count());
    let status = crate::widgets::truncate_text(&format!("({refresh_status}) "), status_budget);
    let title = Line::from(vec![
        Span::raw(label),
        Span::styled(status, Style::default().fg(theme::palette().muted)),
    ]);
    let quotas_focused = dashboard.focus == Focus::Quota;
    let content_widths = quota_table_column_widths(&rows);
    let widths: [Constraint; 6] = std::array::from_fn(|index| {
        Constraint::Length(content_widths[index].saturating_add(QUOTA_COLUMN_GAPS[index]))
    });
    let block = theme::panel(quotas_focused).title(title);
    let block = size.map_or(block.clone(), |size| {
        block.title(pane_size_controls(
            size,
            dashboard.pane_maximize_enabled(SupportPane::Quota),
        ))
    });
    let table = Table::new(rows.into_iter().map(QuotaTableRow::into_row), widths)
        .column_spacing(0)
        .header(
            Row::new(["Profile", "Harness", "Weekly", "Resets", "5H", "Resets"])
                .style(theme::muted().add_modifier(Modifier::BOLD)),
        )
        .row_highlight_style(if quotas_focused {
            theme::selection(true)
        } else {
            Style::default()
        })
        .highlight_symbol(if quotas_focused { "› " } else { "  " })
        .highlight_spacing(HighlightSpacing::Always)
        .block(block);
    let mut offset = dashboard.quota_scroll.get();
    if let Some(direction) = take_scroll_lookahead(dashboard, Focus::Quota) {
        let row_heights = vec![1; dashboard.config.enabled_profiles().count()];
        offset = offset_with_directional_lookahead(
            offset,
            dashboard.quota_index,
            direction,
            &row_heights,
            usize::from(area.height.saturating_sub(SESSION_TABLE_CHROME_HEIGHT)),
        );
    }
    let mut state = TableState::default().with_offset(offset).with_selected(
        dashboard
            .config
            .enabled_profiles()
            .next()
            .is_some()
            .then_some(dashboard.quota_index),
    );
    frame.render_stateful_widget(table, area, &mut state);
    dashboard.quota_scroll.set(state.offset());
    render_session_scrollbar(
        frame,
        area,
        dashboard.config.enabled_profiles().count(),
        state.offset(),
        usize::from(area.height.saturating_sub(SESSION_TABLE_CHROME_HEIGHT)),
    );
}

/// Registry commands shared by pane and composer footers, retaining their identities.
pub(crate) fn footer_commands(
    dashboard: &DashboardState,
    group: crate::actions::FooterGroup,
) -> Vec<(crate::CommandId, String)> {
    let mut hints = crate::actions::available(dashboard, None)
        .into_iter()
        .filter_map(|id| {
            let spec = crate::actions::spec(id);
            if spec.footer_group != group {
                return None;
            }
            let word = (spec.footer)(dashboard)?;
            let hint = spec.keys.first()?;
            Some((spec.footer_rank, id, format!("{} {word}", hint.label)))
        })
        .collect::<Vec<_>>();
    hints.sort_by_key(|(rank, _, _)| *rank);
    hints.into_iter().map(|(_, id, text)| (id, text)).collect()
}

/// The hotkey hints for whatever applies right now.
///
/// Built from the action registry ([`crate::actions`]) rather than written out
/// as a string per pane, so a hint can never name a key the surface does not
/// answer, and a command can never be added without the footer knowing.
///
/// The row is three groups separated by a vertical bar, always in this order:
/// what the focused pane answers, the `Alt` chords that answer from anywhere,
/// and the function keys. The reader therefore always looks in the same place
/// for a given kind of key, and the row does not reshuffle itself as the pane
/// changes. Each group's order comes from `footer_group` and `footer_rank` on
/// the spec, not from where the command sits in the table.
///
/// `width` is the row's width in cells. When the hints do not fit, whole
/// segments are dropped from the right — never truncated mid-word, because
/// half a hint names a key that does not exist — first from the pane group,
/// then from the chords. Function keys give way last, with palette and help
/// retained longest so the user can find everything the narrow row leaves out.
///
/// The composer's own hints come from the chat itself, because they depend on
/// what it is doing (a queued prompt, dictation, a history search); this text
/// is only drawn when a pane has the keyboard.
pub(crate) fn combined_footer_text(dashboard: &DashboardState, width: u16) -> String {
    let groups = [
        footer_commands(dashboard, crate::actions::FooterGroup::Pane),
        footer_commands(dashboard, crate::actions::FooterGroup::Chord),
        footer_commands(dashboard, crate::actions::FooterGroup::Function),
    ];
    let groups = theme::fit_footer_items(groups, width, |(_, text)| text.as_str());
    theme::footer_items_text(&groups, |(_, text)| text.as_str())
}

/// Draws the shared footer row.
///
/// A notice replaces the hints while one is showing, the same way the
/// composer's own footer works, so the two are interchangeable and the row
/// costs one line whichever surface drew it.
pub(crate) fn render_footer(frame: &mut Frame, area: Rect, dashboard: &DashboardState) {
    let notice = dashboard.notices.current();
    let groups = [
        footer_commands(dashboard, crate::actions::FooterGroup::Pane),
        footer_commands(dashboard, crate::actions::FooterGroup::Chord),
        footer_commands(dashboard, crate::actions::FooterGroup::Function),
    ];
    let groups = theme::fit_footer_items(groups, area.width, |(_, text)| text.as_str());
    let line = match notice.as_deref() {
        Some(notice) => Line::styled(
            notice.to_owned(),
            Style::default().fg(theme::palette().warning),
        ),
        None => theme::hints(&combined_footer_text(dashboard, area.width)),
    };
    frame.render_widget(
        Paragraph::new(line).style(theme::muted().bg(theme::palette().surface)),
        area,
    );
    // The dashboard has no active composer to register these controls for us.
    // Register the same fitted segments that were drawn so a footer click
    // dispatches the command represented by that exact hint.
    if notice.is_none() {
        let mut x = area.x;
        for group in groups.iter().filter(|group| !group.is_empty()) {
            if x > area.x {
                x = x.saturating_add(Line::raw(theme::FOOTER_GROUP_SEPARATOR).width() as u16);
            }
            for (index, (id, text)) in group.iter().enumerate() {
                if index > 0 {
                    x = x.saturating_add(Line::raw(theme::FOOTER_SEPARATOR).width() as u16);
                }
                let width = Line::raw(text.as_str()).width() as u16;
                crate::surface_controls::render_footer_command(
                    frame,
                    Rect::new(
                        x,
                        area.y,
                        width.min(area.right().saturating_sub(x)),
                        area.height,
                    ),
                    dashboard,
                    *id,
                    text,
                );
                x = x.saturating_add(width);
            }
        }
    }
}

pub(crate) fn refresh_age(now: u64, refreshed: u64) -> String {
    if refreshed == 0 {
        return "unknown".into();
    }
    let age = now.saturating_sub(refreshed);
    let (value, unit) = if age < 60 {
        (age, "s")
    } else if age < 3_600 {
        (age / 60, "m")
    } else {
        (age / 3_600, "h")
    };
    format!("{value}{unit} ago")
}

#[cfg(test)]
mod tests;
