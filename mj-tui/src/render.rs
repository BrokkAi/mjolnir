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

use mj_core::config::{Config, HarnessKind, PermissionMode};
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
            Line::raw(format!("Setup can create {missing} from this machine.")),
            Line::raw(
                "Press F4 to open Setup, then Detect machine to find your accounts and runtimes.",
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
                        lines.push(Line::from("  Enter for repair details · F7 setup"));
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
        .filter_map(|(id, profile)| {
            let quota = dashboard.quotas.get(id);
            // The report says for itself that it is usage-priced. Before the
            // first refresh there is no report to ask, so the profile's kind
            // stands in until one arrives.
            let usage_priced = match quota {
                Some(quota) => quota.is_usage_priced(),
                None => profile.kind == HarnessKind::Deepseek,
            };
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
        if days == 1 && hours > 0 {
            format!("{days}d{hours}h")
        } else {
            format!("{days}d")
        }
    } else if remaining >= HOUR {
        let hours = remaining / HOUR;
        let minutes = remaining % HOUR / MINUTE;
        if hours == 1 && minutes > 0 {
            format!("{hours}h{minutes}m")
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
        format!("{hours}h{minutes}m")
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
                if profile.kind == HarnessKind::Deepseek {
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
mod tests {
    use std::collections::BTreeMap;

    use crossterm::event::{KeyCode, MouseButton, MouseEventKind};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::Color;

    use mj_core::config::{Config, HarnessKind};
    use mj_core::state::{
        MaterializedExecutionState, STATE_VERSION, SessionState, State, TranscriptBody,
    };

    use mj_chat::selection::SurfaceId;
    use mj_client::quota::{API_LABEL, ProfileQuota, QuotaWindow};
    use mj_core::targets::{DeploymentCapacityUsage, ProvisionStage};

    use super::*;
    use crate::test_support::*;

    use crate::ingest::SessionDetail;
    use crate::{DashboardAction, DashboardState, Focus, SessionOperationKind};

    fn session_metadata_text(
        session: &SessionRecord,
        detail: Option<&SessionDetail>,
        operation: Option<&SessionOperationDisplay>,
        now_epoch_seconds: u64,
        config: &Config,
    ) -> String {
        session_activity_line(
            "",
            session,
            detail,
            None,
            false,
            operation,
            now_epoch_seconds,
            &session_target_label(session, operation, config),
            session_permission_badge(session, operation, config),
            None,
            120,
            false,
        )
        .to_string()
    }

    #[test]
    fn scrollbar_thumb_reaches_both_ends_of_the_viewport() {
        let mut terminal = Terminal::new(TestBackend::new(1, 10)).unwrap();
        for (position, thumb_row) in [(0, 1), (90, 8)] {
            terminal
                .draw(|frame| {
                    render_session_scrollbar(frame, Rect::new(0, 0, 1, 10), 100, position, 10);
                })
                .unwrap();
            assert_eq!(terminal.backend().buffer()[(0, thumb_row)].symbol(), "▐");
        }
        terminal
            .draw(|frame| {
                render_session_scrollbar(frame, Rect::new(0, 0, 1, 10), 10, 0, 10);
            })
            .unwrap();
        assert!((0..10).all(|row| terminal.backend().buffer()[(0, row)].symbol() == " "));
    }

    fn minimize_all_panes(dashboard: &mut DashboardState) {
        for pane in [
            SupportPane::Sessions,
            SupportPane::Targets,
            SupportPane::Quota,
        ] {
            dashboard.set_pane_size(pane, PaneSize::Minimized);
        }
    }

    #[test]
    fn directional_lookahead_reserves_an_adjacent_variable_height_row() {
        let heights = [2, 3, 4, 2];

        assert_eq!(
            offset_with_directional_lookahead(0, 1, SelectionDirection::Down, &heights, 7),
            1,
            "the row after the selection is brought fully into view"
        );
        assert_eq!(
            offset_with_directional_lookahead(2, 2, SelectionDirection::Up, &heights, 7),
            1,
            "the row before the selection is brought fully into view"
        );
        assert_eq!(
            offset_with_directional_lookahead(0, 1, SelectionDirection::Down, &heights, 6),
            0,
            "an impossible two-row margin does not displace the selected row"
        );
    }

    #[test]
    fn grouped_dashboard_has_no_column_header_and_uses_fixed_session_summaries() {
        let mut dashboard = dashboard_with_session(running_session());
        apply_materialized_transcript(&mut dashboard, numbered_conversation(2));
        dashboard
            .session_details
            .get_mut("session-1")
            .unwrap()
            .current_turn_started_at = Some(now_seconds().saturating_sub(10));
        dashboard
            .session_details
            .get_mut("session-1")
            .unwrap()
            .queued_prompts
            .push(mj_core::relay::QueuedPrompt {
                id: "queued-1".into(),
                text: "later".into(),
                attachments: Vec::new(),
                created_at_ms: 1,
            });
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("terminal");
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw dashboard");
        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");

        assert!(rendered.contains("hel"));
        assert!(!rendered.contains("[1] hel"));
        assert!(!rendered.contains("Turn clock"));
        assert!(!rendered.contains("Session name"));
        assert!(rendered.contains("podman"), "{rendered}");
        assert!(rendered.contains("codex-1"), "{rendered}");
        assert!(rendered.contains("[Q 1]"));
        assert!(rendered.contains("Sessions"));
        assert!(!rendered.contains("Turn=time"));
        assert!(!rendered.contains("Step=time"));
        assert!(rendered.contains("codex-1"));
        assert!(!rendered.contains("queued]"));
        assert!(rendered.contains("answer 1"));
    }

    #[test]
    fn pending_questions_mark_the_session_and_minimized_navigator() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard
            .state
            .sessions
            .get_mut("session-1")
            .unwrap()
            .session_title_override = Some("長いセッション名のテスト".repeat(4));
        let mut session = materialized_session_for("session-1", Vec::new());
        session.pending_elicitations = vec![
            mj_core::elicitation::ElicitationRequest::from_acp_params(
                "request-1",
                serde_json::json!({
                    "mode": "form",
                    "sessionId": "session-1",
                    "message": "Choose a path",
                    "requestedSchema": {
                        "type": "object",
                        "properties": {
                            "path": {"type": "string"}
                        }
                    }
                }),
            )
            .expect("valid test question"),
        ];
        dashboard.apply_materialized_session(&session);
        let mut foreign = running_session();
        foreign.id = "foreign-session".into();
        foreign.workspace_id = "other-workspace".into();
        dashboard.session_details.insert(
            foreign.id.clone(),
            SessionDetail {
                pending_elicitations: dashboard.session_details["session-1"]
                    .pending_elicitations
                    .clone(),
                ..SessionDetail::default()
            },
        );
        dashboard.state.sessions.insert(foreign.id.clone(), foreign);
        assert_eq!(dashboard.pending_input_count(), 1);

        let expanded = drawn(&mut dashboard, 120, 30).join("\n");
        assert!(expanded.contains("Question"), "{expanded}");

        minimize_all_panes(&mut dashboard);
        let minimized = drawn(&mut dashboard, 120, 40).join("\n");
        assert!(minimized.contains("Question"), "{minimized}");
        assert!(
            minimized.contains("Sessions [1]") || minimized.contains("Q1"),
            "{minimized}"
        );
    }

    #[test]
    fn a_modal_overlays_the_dashboard_instead_of_replacing_it() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.set_workspace_name("UNDERLYING DASHBOARD SENTINEL".into());
        // The rename editor is reached through the command palette now.
        dashboard.focus_sessions();
        dashboard.handle_key(crate::test_support::key(KeyCode::F(2)));
        for character in "rename".chars() {
            dashboard.handle_key(crate::test_support::key(KeyCode::Char(character)));
        }
        assert_eq!(
            dashboard.handle_key(crate::test_support::key(KeyCode::Enter)),
            DashboardAction::None
        );
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("terminal");

        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw rename dialog");
        let lines = buffer_lines(terminal.backend().buffer());

        let row_of = |needle: &str| {
            lines
                .iter()
                .position(|line| line.contains(needle))
                .unwrap_or_else(|| panic!("missing {needle} in {lines:#?}"))
        };
        let popup_top = row_of("Rename session");
        // The dashboard underneath still shows through every row the modal's
        // centred popup does not cover.
        assert!(row_of("Sessions") < popup_top);
        assert!(
            row_of("podman") < popup_top,
            "the session row behind the popup still shows"
        );
    }

    #[test]
    fn drawing_the_dashboard_registers_each_pane_interior_for_selection() {
        let mut dashboard = dashboard_with_session(running_session());
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("terminal");

        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw dashboard");

        let panes = dashboard.pane_areas.expect("dashboard pane hitboxes");
        let surfaces = dashboard.frame_surfaces();
        for (index, pane) in panes.iter().enumerate() {
            let id = SurfaceId::DashboardPane(index as u8);
            let surface = surfaces
                .surface(id)
                .unwrap_or_else(|| panic!("pane {index} registered"));
            assert_eq!(surface.rect, crate::widgets::bordered_content(*pane));
            assert_eq!(
                surfaces
                    .surface_at(surface.rect.x, surface.rect.y)
                    .map(|surface| surface.id),
                Some(id)
            );
        }
        // The border rows and the scrollbar column stay out of every surface,
        // so a selection can never pick up their glyphs.
        assert!(surfaces.surface_at(panes[0].x, panes[0].y).is_none());
        assert!(
            surfaces
                .surface_at(panes[0].right() - 1, panes[0].y + 1)
                .is_none()
        );
    }

    #[test]
    fn an_open_dialog_registers_its_body_and_list_above_the_panes() {
        let mut dashboard = dashboard_with_session(stopped_session());
        dashboard.show_resume_dialog(1, Vec::new());
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("terminal");

        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw resume dialog");

        let surfaces = dashboard.frame_surfaces();
        let body = surfaces.surface(SurfaceId::ModalBody).expect("dialog body");
        let list = surfaces
            .surface(SurfaceId::ResumeList)
            .expect("session list");
        // The dialog covers the panes, and its list covers the dialog.
        assert_eq!(
            surfaces
                .surface_at(body.rect.x, body.rect.y)
                .map(|surface| surface.id),
            Some(SurfaceId::ModalBody)
        );
        assert_eq!(
            surfaces
                .surface_at(list.rect.x, list.rect.y)
                .map(|surface| surface.id),
            Some(SurfaceId::ResumeList)
        );
        // Away from the popup the panes underneath still own their cells:
        // the dialog covers them, it does not clear them.
        let sessions = surfaces
            .surface(SurfaceId::DashboardPane(0))
            .expect("sessions pane");
        assert_eq!(
            surfaces
                .surface_at(sessions.rect.x, sessions.rect.y)
                .map(|surface| surface.id),
            Some(SurfaceId::DashboardPane(0))
        );
    }

    #[test]
    fn unanswered_user_line_stays_bright_and_shows_the_latest_agent_activity() {
        let mut dashboard = dashboard_with_session(running_session());
        let mut transcript = numbered_conversation(1);
        transcript.push(transcript_item(
            3,
            TranscriptBody::User {
                content: vec![serde_json::json!({
                    "type": "text",
                    "text": "unanswered follow-up"
                })],
            },
        ));
        transcript.push(thought(4, "Checking the workspace"));
        apply_materialized_transcript(&mut dashboard, transcript);
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("terminal");

        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw dashboard");
        let buffer = terminal.backend().buffer();
        let lines = buffer_lines(buffer);
        let rendered = lines.join("\n");

        assert!(rendered.contains("Checking the workspace"));
        assert!(!rendered.contains("You: unanswered follow-up"));
        assert!(!rendered.contains("answer 0"));
        let (user_row, user_line) = lines
            .iter()
            .enumerate()
            .find(|(_, line)| line.contains("Checking the workspace"))
            .expect("current activity line");
        let user_column = cell_column(user_line, "Checking the workspace");
        assert_ne!(
            buffer[(buffer.area.x + user_column, buffer.area.y + user_row as u16)].fg,
            theme::palette().muted
        );
    }

    #[test]
    fn the_sessions_title_prioritizes_workspace_and_controls_at_minimum_width() {
        let wide = sessions_title("a-workspace", 120, true).to_string();
        assert_eq!(wide, " Sessions · a-workspace ");
        assert!(!wide.contains("Turn"));
        assert!(!wide.contains("Step"));

        let narrow = sessions_title("a-rather-long-workspace-name", 32, true).to_string();
        assert!(narrow.starts_with(" S · "), "{narrow:?}");
        assert!(narrow.contains('…'), "{narrow:?}");
        assert!(narrow.chars().count() <= usize::from(pane_title_content_width(32, true)));
    }

    #[test]
    fn actual_sessions_renderer_keeps_actions_and_row_shapes_across_widths() {
        use mj_core::config::SessionsSide;

        for (width, expected_sidebar) in [(80, 40), (120, 40), (180, 60)] {
            for side in [SessionsSide::Left, SessionsSide::Right] {
                let mut dashboard = dashboard_with_session(running_session());
                dashboard.config.sessions_side = side;
                dashboard
                    .session_details
                    .get_mut("session-1")
                    .expect("fixture detail")
                    .queued_prompts
                    .push(mj_core::relay::QueuedPrompt {
                        id: "queued".into(),
                        text: "follow-up".into(),
                        attachments: Vec::new(),
                        created_at_ms: 1,
                    });
                let rendered = drawn(&mut dashboard, width, 40).join("\n");
                let sessions = dashboard.pane_areas.expect("dashboard panes")[0];
                assert_eq!(sessions.width, expected_sidebar);
                assert!(rendered.contains("Create"), "{rendered}");
                assert!(rendered.contains("Resume"), "{rendered}");
                assert!(rendered.contains("Q"), "{rendered}");
                assert!(
                    dashboard
                        .session_row_areas
                        .iter()
                        .all(|(_, area)| area.height == 4)
                );
            }
        }

        let mut minimized = dashboard_with_session(running_session());
        minimized.set_pane_size(SupportPane::Sessions, PaneSize::Minimized);
        let rendered = drawn(&mut minimized, 80, 40).join("\n");
        let sessions = minimized.pane_areas.expect("minimized panes")[0];
        assert_eq!(sessions.width, 20);
        assert!(rendered.contains("Create"), "{rendered}");
        assert!(rendered.contains("Resume"), "{rendered}");
        assert!(
            minimized
                .session_row_areas
                .iter()
                .all(|(_, area)| area.height == 2)
        );
    }

    #[test]
    fn minimized_running_clocks_leave_the_queue_count_visible() {
        for elapsed in [45, 6_000, 172_800] {
            let mut dashboard = dashboard_with_session(running_session());
            dashboard.set_pane_size(SupportPane::Sessions, PaneSize::Minimized);
            let detail = dashboard.session_details.get_mut("session-1").unwrap();
            detail.current_turn_started_at = Some(now_seconds().saturating_sub(elapsed));
            detail.pending_elicitations.clear();
            detail.queued_prompts.push(mj_core::relay::QueuedPrompt {
                id: "queued-1".into(),
                text: "next task".into(),
                attachments: Vec::new(),
                created_at_ms: 1,
            });
            let mut terminal = Terminal::new(TestBackend::new(80, 30)).unwrap();
            terminal
                .draw(|frame| render(frame, &mut dashboard))
                .unwrap();
            let (_, row) = dashboard.session_row_areas[0];
            let status = (row.x..row.right())
                .map(|x| terminal.backend().buffer()[(x, row.y + 1)].symbol())
                .collect::<String>();
            assert!(status.contains("Working "), "{status:?}");
            assert!(status.contains(" Q1"), "{status:?}");
            assert!(!status.contains('…'), "{status:?}");
        }
    }

    #[test]
    fn pane_size_controls_are_styled_registered_and_clickable_without_moving_focus() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.workspace_name = "workspace".into();
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).expect("terminal");
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw controls");

        let expected_control_count = [
            SupportPane::Sessions,
            SupportPane::Targets,
            SupportPane::Quota,
        ]
        .into_iter()
        .map(|pane| {
            if dashboard.pane_maximize_enabled(pane) {
                3
            } else {
                2
            }
        })
        .sum::<usize>();
        assert_eq!(
            dashboard.pane_size_control_areas.len(),
            expected_control_count
        );
        let buffer = terminal.backend().buffer();
        for pane in [
            SupportPane::Sessions,
            SupportPane::Targets,
            SupportPane::Quota,
        ] {
            let maximize_enabled = dashboard.pane_maximize_enabled(pane);
            let expected_sizes = [PaneSize::Minimized, PaneSize::Standard, PaneSize::Maximized]
                .into_iter()
                .filter(|size| *size != PaneSize::Maximized || maximize_enabled)
                .collect::<Vec<_>>();
            let controls = dashboard
                .pane_size_control_areas
                .iter()
                .copied()
                .filter(|(candidate, _, _)| *candidate == pane)
                .collect::<Vec<_>>();
            assert_eq!(controls.len(), expected_sizes.len());
            for ((_, size, area), expected_size) in controls.into_iter().zip(expected_sizes) {
                assert_eq!(size, expected_size);
                let cell = &buffer[(area.x + 1, area.y)];
                let glyph = match size {
                    PaneSize::Minimized => "▁",
                    PaneSize::Standard => "▪",
                    PaneSize::Maximized => "□",
                };
                assert_eq!(cell.symbol(), glyph);
                if size == PaneSize::Standard {
                    assert_eq!(cell.bg, theme::palette().surface_raised);
                    assert_eq!(cell.fg, theme::palette().accent);
                    assert!(cell.modifier.contains(Modifier::BOLD));
                } else {
                    assert_eq!(cell.bg, theme::palette().surface);
                    assert_eq!(cell.fg, theme::palette().muted);
                }
            }
        }

        dashboard.set_pane_size(SupportPane::Targets, PaneSize::Minimized);
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw minimized controls");
        let buffer = terminal.backend().buffer();
        let target_controls = dashboard
            .pane_size_control_areas
            .iter()
            .copied()
            .filter(|(pane, _, _)| *pane == SupportPane::Targets)
            .collect::<Vec<_>>();
        assert_eq!(target_controls.len(), 2);
        for ((_, size, area), (glyph, expected_size)) in target_controls
            .into_iter()
            .zip([("▁", PaneSize::Minimized), ("▪", PaneSize::Standard)])
        {
            assert_eq!(size, expected_size);
            assert_eq!(buffer[(area.x + 1, area.y)].symbol(), glyph);
        }
        let targets_area = dashboard.pane_areas.expect("pane areas")[1];
        assert_eq!(
            buffer[(targets_area.right() - 1, targets_area.y)].symbol(),
            "─"
        );

        dashboard.focus = Focus::Targets;
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw focused minimized controls");
        let focused_targets = buffer_lines(terminal.backend().buffer())
            .into_iter()
            .find(|line| line.contains("Targets"))
            .expect("focused Targets row");
        assert!(focused_targets.contains("─ Targets ──"));
        assert!(focused_targets.ends_with('─'));
        assert!(!focused_targets.contains('═'));
        dashboard.focus = Focus::Sessions;
        assert!(
            dashboard
                .pane_size_control_areas
                .iter()
                .all(
                    |(pane, size, _)| *pane != SupportPane::Targets || *size != PaneSize::Maximized
                )
        );
        assert_eq!(dashboard.focus(), Focus::Sessions);
    }

    #[test]
    fn unavailable_sessions_maximum_is_hidden_and_returns_on_resize() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.set_pane_size(SupportPane::Sessions, PaneSize::Maximized);

        let mut narrow = Terminal::new(TestBackend::new(80, 40)).expect("narrow terminal");
        narrow
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw narrow dashboard");
        assert_eq!(
            dashboard.pane_size(SupportPane::Sessions),
            PaneSize::Maximized
        );
        assert!(!dashboard.pane_maximize_enabled(SupportPane::Sessions));
        assert_eq!(dashboard.pane_areas.expect("pane areas")[0].width, 40);
        let session_controls = dashboard
            .pane_size_control_areas
            .iter()
            .copied()
            .filter(|(pane, _, _)| *pane == SupportPane::Sessions)
            .collect::<Vec<_>>();
        assert_eq!(session_controls.len(), 2);
        assert!(
            session_controls
                .iter()
                .all(|(_, size, _)| *size != PaneSize::Maximized)
        );
        let sessions_area = dashboard.pane_areas.expect("pane areas")[0];
        let session_title = (sessions_area.x..sessions_area.right())
            .map(|x| narrow.backend().buffer()[(x, sessions_area.y)].symbol())
            .collect::<String>();
        assert!(!session_title.contains('□'), "{session_title:?}");
        assert_eq!(session_controls[1].2.x, session_controls[0].2.x + 4);
        assert_eq!(session_controls[1].2.right(), sessions_area.right() - 1);
        let standard = session_controls
            .iter()
            .find(|(_, size, _)| *size == PaneSize::Standard)
            .map(|(_, _, area)| *area)
            .expect("visible Standard control");
        let cell = &narrow.backend().buffer()[(standard.x + 1, standard.y)];
        assert_eq!(cell.bg, theme::palette().surface_raised);
        assert_eq!(cell.fg, theme::palette().accent);
        assert!(cell.modifier.contains(Modifier::BOLD));

        dashboard.focus_sessions();
        dashboard.set_pane_size(SupportPane::Sessions, PaneSize::Standard);
        dashboard.handle_key(alt_key('z'));
        assert_eq!(
            dashboard.pane_size(SupportPane::Sessions),
            PaneSize::Minimized,
            "Alt-Z skips unavailable Maximized from Standard"
        );

        dashboard.set_pane_size(SupportPane::Sessions, PaneSize::Maximized);
        let mut wide = Terminal::new(TestBackend::new(120, 40)).expect("wide terminal");
        wide.draw(|frame| render(frame, &mut dashboard))
            .expect("draw wide dashboard");
        assert_eq!(
            dashboard.pane_size(SupportPane::Sessions),
            PaneSize::Maximized
        );
        assert!(dashboard.pane_maximize_enabled(SupportPane::Sessions));
        assert_eq!(dashboard.pane_areas.expect("pane areas")[0].width, 60);
        let maximum = dashboard
            .pane_size_control_areas
            .iter()
            .find(|(pane, size, _)| *pane == SupportPane::Sessions && *size == PaneSize::Maximized)
            .map(|(_, _, area)| *area)
            .expect("visible Maximized control");
        let sessions_area = dashboard.pane_areas.expect("pane areas")[0];
        assert_eq!(maximum.right(), sessions_area.right() - 1);
        let cell = &wide.backend().buffer()[(maximum.x + 1, maximum.y)];
        assert_eq!(cell.bg, theme::palette().surface_raised);
        assert_eq!(cell.fg, theme::palette().accent);
        assert!(cell.modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn sessions_sidebar_layout_uses_fractional_sizes_and_caps() {
        for (width, standard, maximized) in
            [(80, 40, 40), (120, 40, 60), (240, 80, 100), (480, 80, 100)]
        {
            let mut standard_dashboard = dashboard_with_session(running_session());
            standard_dashboard.set_pane_size(SupportPane::Sessions, PaneSize::Standard);
            drawn(&mut standard_dashboard, width, 40);
            assert_eq!(
                standard_dashboard.pane_areas.expect("standard pane areas")[0].width,
                standard,
                "standard width at {width} columns"
            );

            let mut maximized_dashboard = dashboard_with_session(running_session());
            maximized_dashboard.set_pane_size(SupportPane::Sessions, PaneSize::Maximized);
            drawn(&mut maximized_dashboard, width, 40);
            assert_eq!(
                maximized_dashboard
                    .pane_areas
                    .expect("maximized pane areas")[0]
                    .width,
                maximized,
                "maximized width at {width} columns"
            );
        }
    }

    #[test]
    fn maximize_availability_tracks_content_and_works_from_minimized() {
        let mut dashboard = dashboard_with_session(running_session());
        drawn(&mut dashboard, 120, 40);
        assert!(!dashboard.pane_maximize_enabled(SupportPane::Quota));

        let profile = dashboard.config.profiles.values().next().unwrap().clone();
        for index in 0..20 {
            dashboard
                .config
                .profiles
                .insert(format!("extra-{index}"), profile.clone());
        }
        drawn(&mut dashboard, 120, 40);
        assert!(dashboard.pane_maximize_enabled(SupportPane::Quota));
        let standard_height = dashboard.pane_areas.unwrap()[2].height;
        dashboard.set_pane_size(SupportPane::Quota, PaneSize::Minimized);
        drawn(&mut dashboard, 120, 40);
        assert!(dashboard.pane_maximize_enabled(SupportPane::Quota));
        let maximum = dashboard
            .pane_size_control_areas
            .iter()
            .find(|(pane, size, _)| *pane == SupportPane::Quota && *size == PaneSize::Maximized)
            .unwrap()
            .2;
        let focus = dashboard.focus();
        dashboard.handle_mouse(mouse_at_row(
            MouseEventKind::Down(MouseButton::Left),
            maximum,
            0,
        ));
        assert_eq!(dashboard.pane_size(SupportPane::Quota), PaneSize::Maximized);
        assert_eq!(dashboard.focus(), focus);
        drawn(&mut dashboard, 120, 40);
        assert!(dashboard.pane_areas.unwrap()[2].height > standard_height);

        drawn(&mut dashboard, 120, 120);
        assert!(!dashboard.pane_maximize_enabled(SupportPane::Quota));
    }

    #[test]
    fn alt_g_compacts_sessions_and_returns_space_to_the_conversation() {
        for (height, expected_sessions_height) in [(32, 28), (44, 40)] {
            let mut dashboard = minimized_sessions_dashboard(3, 2);
            dashboard
                .restore_pane_sizes(crate::PaneSizes::default())
                .unwrap();
            dashboard.focus_sessions();
            let standard = drawn(&mut dashboard, 120, height).join("\n");
            let standard_panes = dashboard.pane_areas.unwrap();
            let standard_transcript = dashboard.chat_transcript_area.unwrap();
            assert!(standard.contains("Idle"), "{standard}");
            assert!(standard.contains("codex-1"), "{standard}");

            dashboard.handle_key(alt_key('g'));
            let compact = drawn(&mut dashboard, 120, height).join("\n");
            let compact_panes = dashboard.pane_areas.unwrap();
            assert_eq!(compact_panes[0].height, expected_sessions_height);
            assert_eq!(compact_panes[1].height, 1);
            assert_eq!(compact_panes[2].height, 1);
            assert!(compact_panes[0].width < standard_panes[0].width);
            assert!(dashboard.chat_transcript_area.unwrap().height > standard_transcript.height);
            assert!(!compact.contains("You:"), "{compact}");
            assert!(!compact.contains("Agent:"), "{compact}");
            assert!(!dashboard.session_row_areas.is_empty());

            dashboard.handle_key(alt_key('g'));
            drawn(&mut dashboard, 120, height);
            assert_eq!(dashboard.pane_areas.unwrap(), standard_panes);
            assert_eq!(dashboard.chat_transcript_area.unwrap(), standard_transcript);
        }
    }

    #[test]
    fn tab_focus_never_changes_band_geometry() {
        let mut dashboard = minimized_sessions_dashboard(3, 2);
        for pane in [
            SupportPane::Sessions,
            SupportPane::Targets,
            SupportPane::Quota,
        ] {
            dashboard.set_pane_size(pane, PaneSize::Standard);
        }
        drawn(&mut dashboard, 120, 44);
        let expected = (
            dashboard.pane_areas,
            dashboard.chat_transcript_area,
            dashboard.chat_prompt_area,
        );
        for _ in 0..4 {
            dashboard.cycle_focus(false);
            drawn(&mut dashboard, 120, 44);
            assert_eq!(
                (
                    dashboard.pane_areas,
                    dashboard.chat_transcript_area,
                    dashboard.chat_prompt_area,
                ),
                expected
            );
        }
    }

    #[test]
    fn sessions_in_an_expanded_project_have_a_blank_row_and_only_the_caret_marks_selection() {
        let mut first = running_session();
        first.id = "session-first".into();
        first.project_directory = Some("/projects/shared".into());
        first.session_title_override = Some("First session".into());
        let mut second = running_session();
        second.id = "session-second".into();
        second.project_directory = Some("/projects/shared".into());
        second.session_title_override = Some("Second session".into());
        second.created_at = "2026-08-10T00:00:00Z".into();
        let state = State {
            version: STATE_VERSION,
            sessions: BTreeMap::from([(first.id.clone(), first), (second.id.clone(), second)]),
            mount_history: BTreeMap::new(),
            container_sizes: BTreeMap::new(),
        };
        let mut dashboard = DashboardState::new(config(), state, BTreeMap::new());
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("terminal");

        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw dashboard");

        let buffer = terminal.backend().buffer();
        let lines = buffer_lines(buffer);
        let first_y = lines
            .iter()
            .position(|line| line.contains("First session"))
            .expect("first session row") as u16;
        let second_y = lines
            .iter()
            .position(|line| line.contains("Second session"))
            .expect("second session row") as u16;
        assert!(
            (first_y..first_y + 4).all(|y| {
                (dashboard.pane_areas.expect("pane areas")[0].x + 1
                    ..dashboard.pane_areas.expect("pane areas")[0].right() - 1)
                    .all(|x| buffer[(x, y)].bg != theme::palette().muted)
            }),
            "selection must not paint a background"
        );
        assert!(lines[first_y as usize].contains("› "));
        assert!(lines[first_y as usize].contains("First session"));
        assert_eq!(
            second_y,
            first_y + 5,
            "sessions in an expanded project have one blank row between them"
        );
        let pane = dashboard.pane_areas.expect("pane areas")[0];
        assert!(
            (pane.x + 1..pane.right() - 1)
                .all(|x| buffer[(x, first_y + 4)].symbol().trim().is_empty())
        );
    }

    #[test]
    fn missing_configuration_renders_repair_status_and_clears_after_restore() {
        let session = running_session();
        let bundle_id = session.bundle_id.clone();
        let mut dashboard = dashboard_with_session(session);
        let bundle = dashboard.config.bundles.remove(&bundle_id).unwrap();
        let broken = drawn(&mut dashboard, 120, 44).join("\n");
        assert!(broken.contains("Needs config repair"), "{broken}");
        dashboard.config.bundles.insert(bundle_id, bundle);
        let repaired = drawn(&mut dashboard, 120, 44).join("\n");
        assert!(!repaired.contains("Needs config repair"), "{repaired}");
    }

    #[test]
    fn session_transitions_preserve_the_blank_row_before_the_next_session() {
        let mut first = running_session();
        first.project_directory = Some("/projects/shared".into());
        first.session_title_override = Some("First session".into());
        let mut second = first.clone();
        second.id = "session-second".into();
        second.session_title_override = Some("Second session".into());
        second.created_at = "2026-08-10T00:00:00Z".into();
        let mut dashboard = dashboard_with_session(first.clone());
        dashboard.state.sessions.insert(second.id.clone(), second);
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("terminal");

        for kind in [
            Some(SessionOperationKind::Launching),
            Some(SessionOperationKind::Resuming),
            Some(SessionOperationKind::Moving),
            Some(SessionOperationKind::Stopping),
            Some(SessionOperationKind::Destroying),
            None,
        ] {
            dashboard.session_operations.clear();
            if let Some(kind) = kind {
                dashboard
                    .session_operations
                    .insert(first.id.clone(), operation(kind, None));
            } else {
                let session = dashboard.state.sessions.get_mut(&first.id).unwrap();
                session.state = SessionState::Closing;
                session.last_error = Some("checkpoint failed".into());
            }
            terminal
                .draw(|frame| render(frame, &mut dashboard))
                .expect("draw session transition");
            let buffer = terminal.backend().buffer();
            let lines = buffer_lines(buffer);
            let first_y = lines
                .iter()
                .position(|line| line.contains("podman [1]"))
                .expect("transition row");
            let second_y = lines
                .iter()
                .position(|line| line.contains("Second session"))
                .expect("following session row");
            assert_eq!(second_y, first_y + 2, "{kind:?}: {lines:#?}");
            assert!(
                (dashboard.pane_areas.expect("pane areas")[0].x + 1
                    ..dashboard.pane_areas.expect("pane areas")[0].right() - 1)
                    .all(|x| { buffer[(x, (first_y + 1) as u16)].symbol().trim().is_empty() }),
                "{kind:?}: the separator must be blank"
            );
        }
    }

    #[test]
    fn project_groups_have_one_blank_row_between_them() {
        let mut first = running_session();
        first.id = "session-alpha".into();
        first.project_directory = Some("/projects/alpha".into());
        let mut second = running_session();
        second.id = "session-beta".into();
        second.project_directory = Some("/projects/beta".into());
        second.created_at = "2026-08-10T00:00:00Z".into();
        let state = State {
            version: STATE_VERSION,
            sessions: BTreeMap::from([(first.id.clone(), first), (second.id.clone(), second)]),
            mount_history: BTreeMap::new(),
            container_sizes: BTreeMap::new(),
        };
        let mut dashboard = DashboardState::new(config(), state, BTreeMap::new());
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("terminal");

        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw dashboard");

        let buffer = terminal.backend().buffer();
        let lines = buffer_lines(buffer);
        let first_y = lines
            .iter()
            .position(|line| line.contains("› ") && line.contains("ACP pretty name"))
            .expect("first session row") as u16;
        let second_heading_y = lines
            .iter()
            .position(|line| line.contains("beta"))
            .expect("second project heading") as u16;
        let first_bottom = first_y + 4;
        assert_eq!(second_heading_y, first_bottom + 1);
        let pane = dashboard.pane_areas.expect("pane areas")[0];
        assert!(
            (pane.x + 1..pane.right() - 1)
                .all(|x| buffer[(x, first_bottom)].symbol().trim().is_empty())
        );
    }

    #[test]
    fn project_hotkeys_collapse_and_expand_groups_independently() {
        let mut first = running_session();
        first.id = "session-alpha".into();
        first.project_directory = Some("/projects/alpha".into());
        let mut second = running_session();
        second.id = "session-beta".into();
        second.project_directory = Some("/projects/beta".into());
        second.created_at = "2026-08-10T00:00:00Z".into();
        let state = State {
            version: STATE_VERSION,
            sessions: BTreeMap::from([(first.id.clone(), first), (second.id.clone(), second)]),
            mount_history: BTreeMap::new(),
            container_sizes: BTreeMap::new(),
        };
        let mut dashboard = DashboardState::new(config(), state, BTreeMap::new());
        dashboard.apply_materialized_session(&materialized_session_for(
            "session-alpha",
            numbered_conversation(1),
        ));
        dashboard.apply_materialized_session(&materialized_session_for(
            "session-beta",
            vec![
                transcript_item(
                    1,
                    TranscriptBody::User {
                        content: vec![serde_json::json!({"type":"text","text":"beta question"})],
                    },
                ),
                agent_message(2, "beta answer"),
            ],
        ));
        let mut terminal = Terminal::new(TestBackend::new(120, 44)).expect("terminal");
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw first project");
        let first_draw = buffer_lines(terminal.backend().buffer()).join("\n");
        // Every project starts expanded, so both groups show their full form.
        assert!(first_draw.contains("[1] alpha"));
        assert!(first_draw.contains("[2] beta"));
        assert!(first_draw.contains("answer 0"));
        assert!(first_draw.contains("beta answer"));

        // The numbered hotkey collapses only its own project.
        assert_eq!(
            dashboard.handle_key(crate::test_support::key(KeyCode::Char('2'))),
            DashboardAction::None
        );
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw with beta collapsed");
        let second_draw = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(second_draw.contains("answer 0"), "{second_draw}");
        assert!(!second_draw.contains("beta answer"), "{second_draw}");
        assert!(
            second_draw.contains("[2] beta") && second_draw.contains("Working"),
            "the collapsed group keeps a two-line summary per session: {second_draw}"
        );

        // Collapsing alpha too leaves both groups collapsed at once.
        dashboard.handle_key(crate::test_support::key(KeyCode::Char('1')));
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw with both collapsed");
        let third_draw = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(!third_draw.contains("answer 0"), "{third_draw}");
        assert!(!third_draw.contains("beta answer"), "{third_draw}");

        // And the hotkey is a toggle, so pressing it again brings beta back.
        dashboard.handle_key(crate::test_support::key(KeyCode::Char('2')));
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw with beta expanded again");
        let fourth_draw = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(fourth_draw.contains("beta answer"), "{fourth_draw}");
        assert!(!fourth_draw.contains("answer 0"), "{fourth_draw}");
    }

    #[test]
    fn collapsed_duplicate_targets_are_numbered_within_their_project() {
        let mut alpha = running_session();
        alpha.id = "session-alpha".into();
        alpha.project_directory = Some("/projects/alpha".into());
        let mut beta_first = running_session();
        beta_first.id = "session-beta-first".into();
        beta_first.project_directory = Some("/projects/beta".into());
        beta_first.created_at = "2026-08-10T00:00:00Z".into();
        let mut beta_second = beta_first.clone();
        beta_second.id = "session-beta-second".into();
        beta_second.created_at = "2026-08-11T00:00:00Z".into();
        let state = State {
            version: STATE_VERSION,
            sessions: [alpha, beta_first, beta_second]
                .into_iter()
                .map(|session| (session.id.clone(), session))
                .collect(),
            mount_history: BTreeMap::new(),
            container_sizes: BTreeMap::new(),
        };
        let mut dashboard = DashboardState::new(config(), state, BTreeMap::new());
        dashboard.apply_materialized_session(&materialized_session_for(
            "session-beta-first",
            vec![agent_message(1, "first tail")],
        ));
        dashboard.apply_materialized_session(&materialized_session_for(
            "session-beta-second",
            vec![agent_message(1, "second tail")],
        ));
        for id in ["session-beta-first", "session-beta-second"] {
            dashboard
                .session_details
                .get_mut(id)
                .unwrap()
                .current_turn_started_at = Some(now_seconds().saturating_sub(10));
        }
        dashboard.focus_sessions();
        let mut terminal = Terminal::new(TestBackend::new(120, 44)).expect("terminal");
        // Collapse the beta project so its sessions draw their compact form,
        // which is where duplicate targets need their numbering.
        dashboard.handle_key(crate::test_support::key(KeyCode::Char('2')));

        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw collapsed duplicate targets");
        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");

        assert_eq!(
            rendered
                .lines()
                .filter(|line| line.contains("podman [1]"))
                .count(),
            1,
            "{rendered}"
        );
        assert_eq!(
            rendered
                .lines()
                .filter(|line| line.contains("podman [2]"))
                .count(),
            1,
            "{rendered}"
        );
        assert!(!rendered.contains("first tail"), "{rendered}");
        assert!(!rendered.contains("second tail"), "{rendered}");
    }

    #[test]
    fn summary_band_colors_prioritize_attention_activity_and_lifecycle() {
        let normal = SessionDetail {
            current_turn_started_at: Some(1),
            ..SessionDetail::default()
        };
        assert_eq!(
            session_band_color(Some(&normal), false, SessionState::Running),
            theme::palette().session_activity
        );

        let unread = SessionDetail {
            current_turn_started_at: Some(1),
            unread_agent_messages: 1,
            ..SessionDetail::default()
        };
        assert_eq!(
            session_band_color(Some(&unread), false, SessionState::Running),
            theme::palette().session_attention
        );

        let unread_idle = SessionDetail {
            unread_agent_messages: 1,
            ..SessionDetail::default()
        };
        assert_eq!(
            session_band_color(Some(&unread_idle), false, SessionState::Running),
            theme::palette().session_idle
        );

        let read_idle = SessionDetail::default();
        assert_eq!(
            session_band_color(Some(&read_idle), false, SessionState::Running),
            theme::palette().session_idle
        );

        let foreground = SessionDetail {
            activity: mj_client::usage_format::SessionActivity {
                foreground_tool_started_at_ms: Some(1),
                ..mj_client::usage_format::SessionActivity::default()
            },
            ..SessionDetail::default()
        };
        assert_eq!(
            session_band_color(Some(&foreground), false, SessionState::Running),
            theme::palette().session_activity,
            "foreground work is not idle"
        );

        let unread_background = SessionDetail {
            unread_agent_messages: 1,
            activity: mj_client::usage_format::SessionActivity {
                background_commands: vec![mj_core::relay::BackgroundCommand {
                    id: "test-background".into(),
                    started_at_ms: 1,
                    command: "cargo test".into(),
                    can_stop: false,
                }],
                ..mj_client::usage_format::SessionActivity::default()
            },
            ..SessionDetail::default()
        };
        assert_eq!(
            session_band_color(Some(&unread_background), false, SessionState::Running),
            theme::palette().session_attention,
            "background work does not use the blue idle-unread band"
        );
        let read_background = SessionDetail {
            activity: unread_background.activity.clone(),
            ..SessionDetail::default()
        };
        assert_eq!(
            session_band_color(Some(&read_background), false, SessionState::Running),
            theme::palette().session_activity,
            "background work is not idle after it has been read"
        );

        let restarted_idle = SessionDetail {
            unread_session_restarts: 1,
            ..SessionDetail::default()
        };
        assert_eq!(
            session_band_color(Some(&restarted_idle), false, SessionState::Running),
            theme::palette().session_idle
        );

        let restarted_running = SessionDetail {
            current_turn_started_at: Some(1),
            unread_session_restarts: 1,
            ..SessionDetail::default()
        };
        assert_eq!(
            session_band_color(Some(&restarted_running), false, SessionState::Running),
            theme::palette().session_attention
        );

        let needs_input = SessionDetail {
            pending_elicitations: vec![
                mj_core::elicitation::ElicitationRequest::from_acp_params(
                    "request-1",
                    serde_json::json!({
                        "mode": "form",
                        "sessionId": "session-1",
                        "message": "Choose a path",
                        "requestedSchema": {
                            "type": "object",
                            "properties": {"path": {"type": "string"}}
                        }
                    }),
                )
                .expect("valid test question"),
            ],
            ..SessionDetail::default()
        };
        assert_eq!(
            session_band_color(Some(&needs_input), false, SessionState::Running),
            theme::palette().session_attention,
            "pending input overrides the idle blue"
        );

        assert_eq!(
            session_band_color(Some(&read_idle), false, SessionState::Provisioning),
            theme::palette().session_activity,
            "provisioning is a lifecycle state, not a live idle session"
        );
        assert_eq!(
            session_band_color(Some(&read_idle), false, SessionState::Error),
            theme::palette().session_error,
            "error overrides idle"
        );
        assert_eq!(
            session_band_color(None, false, SessionState::Running),
            theme::palette().session_activity,
            "unknown detail stays at the default"
        );

        // An unreachable target is red, overriding every other state.
        assert_eq!(
            session_band_color(Some(&unread), true, SessionState::Running),
            theme::palette().session_error
        );
        assert_eq!(
            session_band_color(None, true, SessionState::Running),
            theme::palette().session_error
        );
    }

    #[test]
    fn current_agent_excerpt_never_repeats_an_old_answer() {
        let waiting = SessionDetail {
            last_agent_message: Some("previous answer".into()),
            last_user_message: Some("new request".into()),
            ..SessionDetail::default()
        };
        assert_eq!(current_agent_excerpt(&waiting), None);

        let thinking = SessionDetail {
            last_agent_message: Some("previous answer".into()),
            last_user_message: Some("new request".into()),
            latest_agent_activity_after_last_user: Some("checking files".into()),
            ..SessionDetail::default()
        };
        assert_eq!(current_agent_excerpt(&thinking), Some("checking files"));

        let replying = SessionDetail {
            last_agent_message: Some("current answer".into()),
            last_user_message: Some("new request".into()),
            last_agent_message_follows_last_user: true,
            latest_agent_activity_after_last_user: Some("older thought".into()),
            ..SessionDetail::default()
        };
        assert_eq!(current_agent_excerpt(&replying), Some("current answer"));

        let old_only = SessionDetail {
            last_agent_message: Some("old answer".into()),
            ..SessionDetail::default()
        };
        assert_eq!(current_agent_excerpt(&old_only), None);
    }

    #[test]
    fn marking_all_read_removes_the_unread_tint_while_the_session_keeps_working() {
        let mut dashboard = dashboard_with_session(running_session());
        apply_materialized_transcript(&mut dashboard, vec![agent_message(4, "unread response")]);
        dashboard
            .session_details
            .get_mut("session-1")
            .unwrap()
            .current_turn_started_at = Some(1);
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("terminal");
        let mut row_color = |dashboard: &mut DashboardState| {
            terminal
                .draw(|frame| render(frame, dashboard))
                .expect("draw dashboard");
            let buffer = terminal.backend().buffer();
            let lines = buffer_lines(buffer);
            let row = lines
                .iter()
                .position(|line| line.contains("podman") && line.contains("Working"))
                .expect("session row");
            buffer[(cell_column(&lines[row], "podman"), row as u16)].fg
        };
        assert_eq!(
            row_color(&mut dashboard),
            theme::palette().session_attention
        );
        assert_eq!(
            dashboard.handle_key(alt_key('a')),
            DashboardAction::MarkAllRead {
                receipts: vec![("session-1".into(), 4)]
            }
        );
        assert_eq!(row_color(&mut dashboard), theme::palette().session_activity);
        assert_eq!(
            dashboard.session_details["session-1"].current_turn_started_at,
            Some(1)
        );
    }

    #[test]
    fn dashboard_replaces_too_short_layout_with_required_height() {
        let mut dashboard = DashboardState::new(config(), State::default(), BTreeMap::new());
        let mut terminal = Terminal::new(TestBackend::new(120, 10)).expect("terminal");
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw short dashboard");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Terminal too small"));
        assert!(
            rendered.contains("at least 13 rows (currently 10)"),
            "{rendered:?}"
        );

        let mut terminal = Terminal::new(TestBackend::new(120, 16)).expect("terminal");
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw exact minimum dashboard");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(!rendered.contains("Terminal too small"));
        assert!(rendered.contains("Sessions"));
        assert!(rendered.contains('▁'));
        assert!(rendered.contains('▪'));
        assert!(rendered.contains('□'));
        assert!(rendered.contains("Quota"));
    }

    #[test]
    fn dashboard_replaces_layouts_narrower_than_80_columns() {
        let mut dashboard = DashboardState::new(config(), State::default(), BTreeMap::new());
        let mut terminal = Terminal::new(TestBackend::new(79, 24)).expect("terminal");
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw narrow dashboard");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Terminal too small"));
        assert!(rendered.contains("Need at least 80 columns"));
        assert!(rendered.contains("Current width: 79"));

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw exact minimum-width dashboard");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(!rendered.contains("Terminal too small"));
        assert!(dashboard.pane_areas.unwrap()[0].width > 0);
    }

    #[test]
    fn new_session_picker_keeps_choices_and_controls_visible_at_minimum_width() {
        let mut dashboard = DashboardState::new(config(), State::default(), BTreeMap::new());
        assert_eq!(dashboard.handle_key(alt_key('w')), DashboardAction::None);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw minimum-width new-session picker");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(rendered.contains("claude-1"));
        assert!(rendered.contains("codex-2"));
        assert!(rendered.contains("Cancel"));
        assert!(rendered.contains("Next"));
    }

    #[test]
    fn the_footer_is_one_row_that_a_notice_takes_over() {
        let mut dashboard = DashboardState::new(config(), State::default(), BTreeMap::new());
        dashboard.set_workspace_name("personal".into());
        dashboard.set_notice("Transient dashboard message");
        let mut terminal = Terminal::new(TestBackend::new(120, 24)).expect("terminal");
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw dashboard");
        let buffer = terminal.backend().buffer();
        let line = |y| {
            (buffer.area.x..buffer.area.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        };

        // The workspace pane precedes Sessions, while the
        // transcript keeps the same upper band height.
        let sessions = dashboard.pane_areas.expect("pane geometry")[0];
        assert!(
            line(sessions.y).contains("Sessions"),
            "{:?}",
            line(sessions.y)
        );
        assert_eq!(dashboard.workspace_name, "personal");
        assert!(!line(sessions.y).contains("ACP sessions"));
        // The footer is one row: a notice replaces the hints while one is
        // showing, so the row costs one line whichever surface drew it.
        assert!(
            line(buffer.area.bottom() - 1).contains("Transient dashboard message"),
            "{:?}",
            line(buffer.area.bottom() - 1)
        );
        dashboard.notices.clear();
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw dashboard");
        let buffer = terminal.backend().buffer();
        let hotkeys = (buffer.area.x..buffer.area.right())
            .map(|x| buffer[(x, buffer.area.bottom() - 1)].symbol())
            .collect::<String>();
        assert!(hotkeys.contains("Alt-N create"), "{hotkeys:?}");
        assert!(hotkeys.contains("Alt-A read"), "{hotkeys:?}");
        assert!(!hotkeys.contains("[S]ort"));
    }

    /// The footer is the only place a beginner learns what `Alt-X` does, so it
    /// must name the operation it would cancel — and must not offer the key at
    /// all while there is nothing in flight.
    #[test]
    fn footer_lists_cancel_only_while_an_operation_is_in_flight() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        assert!(
            !combined_footer_text(&dashboard, 200).contains("cancel"),
            "{}",
            combined_footer_text(&dashboard, 200)
        );

        dashboard.begin_session_operation_at(
            "session-1".into(),
            SessionOperationKind::Launching,
            None,
            1_000,
        );
        let footer = combined_footer_text(&dashboard, 200);
        assert!(footer.contains("Alt-X cancel launch"), "{footer}");

        dashboard.finish_session_operation("session-1");
        assert!(
            !combined_footer_text(&dashboard, 200).contains("cancel"),
            "{}",
            combined_footer_text(&dashboard, 200)
        );
    }

    /// Help is the one hint that is worth more than any other, so it survives
    /// every focus and every width squeeze.
    #[test]
    fn footer_ends_with_f1_help_at_every_focus() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.set_deployment_capacity_targets(vec![test_capacity_target()]);
        for focus in [Focus::Sessions, Focus::Targets, Focus::Quota, Focus::Prompt] {
            dashboard.focus = focus;
            let footer = combined_footer_text(&dashboard, 200);
            assert!(footer.ends_with("F1 help"), "{focus:?}: {footer}");
        }
    }

    /// A hint cut in half names a key that does not exist. Narrow terminals
    /// therefore lose whole hints from the right, and never part of one.
    #[test]
    fn footer_drops_whole_hints_when_the_width_runs_out() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        let full = combined_footer_text(&dashboard, 200);
        assert!(full.chars().count() > 40, "{full}");

        for width in 0_u16..=80 {
            let footer = combined_footer_text(&dashboard, width);
            assert!(Line::raw(footer.as_str()).width() <= usize::from(width));
            if width >= 7 {
                assert!(footer.ends_with("F1 help"), "{width}: {footer}");
            }
            if width >= 20 {
                assert!(footer.contains("F2 palette"), "{width}: {footer}");
            }
            // Every hint that survived is a whole hint of the full text.
            for hint in footer_hints(&footer) {
                assert!(
                    footer_hints(&full).contains(&hint),
                    "{width}: {hint:?} is not a whole hint of {full:?}"
                );
            }
        }
    }

    /// Every hint in the footer, whichever separator it sits between.
    fn footer_hints(footer: &str) -> Vec<String> {
        footer
            .split(theme::FOOTER_GROUP_SEPARATOR)
            .flat_map(|group| group.split(theme::FOOTER_SEPARATOR))
            .filter(|hint| !hint.is_empty())
            .map(ToOwned::to_owned)
            .collect()
    }

    /// The row is read left to right by someone hunting one key, so the kinds
    /// of key never swap places: what this pane answers, then the chords that
    /// answer anywhere, then the function keys.
    #[test]
    fn footer_groups_pane_alt_and_function_keys_in_that_order() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        assert_eq!(
            combined_footer_text(&dashboard, 200),
            "Enter open · s stop · Del delete · Tab pane · r restart │ Alt-N create · Alt-S resume · Alt-A read · Alt-Z size · Alt-G panes \
             · Alt-Q detach │ F2 palette · F4 web · F5 refresh · F7 setup · F1 help"
        );

        // The cancel chord takes its fixed place before detach, and only while
        // there is something to cancel.
        dashboard.set_deployment_capacity_targets(vec![test_capacity_target()]);
        dashboard.begin_session_operation_at(
            "session-1".into(),
            SessionOperationKind::Launching,
            None,
            1_000,
        );
        let footer = combined_footer_text(&dashboard, 200);
        assert!(footer.contains("│ Alt-N"), "{footer}");
        assert!(
            footer.contains("Alt-G panes · Alt-X cancel launch · Alt-Q detach"),
            "{footer}"
        );
    }

    /// Pane hints give way before chords, and help and palette remain visible
    /// after the less important function-key hints have been dropped.
    #[test]
    fn footer_drops_pane_hints_before_alt_hints_and_keeps_help_longest() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.set_deployment_capacity_targets(vec![test_capacity_target()]);
        dashboard.focus_sessions();
        const FUNCTION_KEYS: &str = "F2 palette · F4 web · F5 refresh · F7 setup · F1 help";

        let full = combined_footer_text(&dashboard, 200);
        assert!(
            footer_hints(&full).contains(&"Enter open".to_owned()),
            "{full}"
        );

        // Narrow enough to lose the pane group, wide enough to keep chords.
        let squeezed = combined_footer_text(&dashboard, 90);
        assert!(!squeezed.contains("Enter open"), "{squeezed}");
        assert!(squeezed.contains("Alt-N create"), "{squeezed}");
        assert!(squeezed.ends_with(FUNCTION_KEYS), "{squeezed}");

        assert_eq!(
            combined_footer_text(&dashboard, 32),
            "F2 palette · F4 web · F1 help"
        );
        assert_eq!(combined_footer_text(&dashboard, 20), "F2 palette · F1 help");
        assert_eq!(combined_footer_text(&dashboard, 7), "F1 help");
        assert!(combined_footer_text(&dashboard, 5).is_empty());
        assert!(combined_footer_text(&dashboard, 0).is_empty());
    }

    /// The footer is generated from the same table the keyboard reads, so
    /// pressing what it names must do what it says. This is the test that
    /// makes the registry worth having.
    #[test]
    fn every_footer_hint_dispatches_the_command_it_names() {
        for focus in [Focus::Sessions, Focus::Targets, Focus::Quota] {
            let mut dashboard = dashboard_with_session(running_session());
            dashboard.set_deployment_capacity_targets(vec![test_capacity_target()]);
            dashboard.begin_session_operation_at(
                "session-1".into(),
                SessionOperationKind::Launching,
                None,
                1_000,
            );
            dashboard.focus = focus;

            for id in crate::actions::available(&dashboard, None) {
                let spec = crate::actions::spec(id);
                let Some(word) = (spec.footer)(&dashboard) else {
                    continue;
                };
                let Some(hint) = spec.keys.first() else {
                    continue;
                };
                let footer = combined_footer_text(&dashboard, 400);
                assert!(
                    footer.contains(&format!("{} {word}", hint.label)),
                    "{focus:?}: {footer} omits {:?}",
                    spec.label
                );

                // Pressing the advertised key and dispatching the command it
                // names have to leave the surface in the same place.
                let mut pressed = dashboard_with_session(running_session());
                pressed.set_deployment_capacity_targets(vec![test_capacity_target()]);
                pressed.begin_session_operation_at(
                    "session-1".into(),
                    SessionOperationKind::Launching,
                    None,
                    1_000,
                );
                pressed.focus = focus;
                let mut dispatched = dashboard_with_session(running_session());
                dispatched.set_deployment_capacity_targets(vec![test_capacity_target()]);
                dispatched.begin_session_operation_at(
                    "session-1".into(),
                    SessionOperationKind::Launching,
                    None,
                    1_000,
                );
                dispatched.focus = focus;

                // A hint that carries a modifier on a letter is an Alt chord.
                let key_event = match (hint.modifiers.is_empty(), hint.code) {
                    (false, KeyCode::Char(character)) => alt_key(character),
                    _ => key(hint.code),
                };
                let by_key = pressed.handle_key(key_event);
                let by_dispatch = dispatched.dispatch_command(id);
                assert_eq!(by_key, by_dispatch, "{focus:?}: {:?}", spec.label);
                assert_eq!(
                    std::mem::discriminant(&pressed.mode),
                    std::mem::discriminant(&dispatched.mode),
                    "{focus:?}: {:?}",
                    spec.label
                );
                assert_eq!(
                    pressed.focus, dispatched.focus,
                    "{focus:?}: {:?}",
                    spec.label
                );
            }
        }
    }

    /// Expanded output keeps the transcript's rich formatting without adding
    /// a second role rail.
    #[test]
    fn an_expanded_agent_excerpt_flattens_newlines_without_a_transcript_gutter() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        apply_materialized_transcript(
            &mut dashboard,
            vec![
                transcript_item(
                    1,
                    TranscriptBody::User {
                        content: vec![serde_json::json!({
                            "type": "text",
                            "text": "summarize the README"
                        })],
                    },
                ),
                agent_message(2, "reliability\nreply: summarize the README"),
            ],
        );

        let lines = drawn(&mut dashboard, 120, 44);
        let agent = lines
            .iter()
            .find(|line| line.contains("reliability reply"))
            .expect("the agent excerpt row");
        // The rounded pane contributes one edge on each side; the excerpt
        // must not duplicate the conversation's interior rail.
        let session_cell = agent.split("││").next().unwrap_or(agent.as_str());
        assert!(
            !session_cell.trim_matches('\u{2502}').contains('\u{2502}'),
            "the excerpt carries no transcript rail: {agent:?}"
        );
    }

    /// The empty band has two causes and they need different advice. Telling
    /// someone there is no live session while the pane above lists one is a
    /// plain lie.
    #[test]
    fn the_empty_prompt_distinguishes_no_session_from_no_conversation() {
        let mut empty = DashboardState::new(config(), State::default(), BTreeMap::new());
        let lines = drawn(&mut empty, 120, 44).join("\n");
        assert!(lines.contains("No live session"), "{lines}");
        assert!(lines.contains("Alt-N to create one"), "{lines}");

        // A live session that simply is not open says so instead.
        let mut live = dashboard_with_session(running_session());
        let lines = drawn(&mut live, 120, 44).join("\n");
        assert!(lines.contains("No conversation open"), "{lines}");
        assert!(lines.contains("Enter on the one to open"), "{lines}");
        assert!(!lines.contains("No live session"), "{lines}");
        assert!(!lines.contains("Opening session"), "{lines}");
    }

    /// Attaching is asynchronous, and until it lands the chat still loaded
    /// belongs to the row the selection has moved off. The band says the
    /// session is opening rather than showing the previous transcript under
    /// the new highlight.
    #[test]
    fn an_attach_in_flight_draws_an_empty_conversation_that_says_the_session_is_opening() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.set_opening_session(Some("session-1"));

        let lines = drawn(&mut dashboard, 120, 44).join("\n");
        assert!(lines.contains("Opening session"), "{lines}");
        assert!(lines.contains("Opening session"), "{lines}");
        assert!(
            lines.contains("Esc cancels · select another session to switch · Alt-Q quits"),
            "{lines}"
        );
        assert!(!lines.contains("No conversation open"), "{lines}");
    }

    /// The band order is the whole point of the surface: everything is on one
    /// screen, in one arrangement, at every size it draws at.
    #[test]
    fn the_session_sidebar_spans_all_other_panes_on_either_side() {
        for side in [
            mj_core::config::SessionsSide::Left,
            mj_core::config::SessionsSide::Right,
        ] {
            for (width, height) in [(140, 32), (80, 20), (80, 16)] {
                let mut dashboard = dashboard_with_session(running_session());
                dashboard.config.sessions_side = side;
                let lines = drawn(&mut dashboard, width, height);
                let [sessions, targets, quota] = dashboard.pane_areas.unwrap();
                let transcript = dashboard.chat_transcript_area.unwrap();
                let prompt = dashboard.chat_prompt_area.unwrap();
                assert_eq!(sessions.y, 3);
                assert!(sessions.height > 0);
                assert!(sessions.width > 0 && transcript.width > 0);
                for pane in [transcript, prompt, targets, quota] {
                    assert!(pane.height > 0);
                }
                for pane in [transcript, prompt] {
                    if side == mj_core::config::SessionsSide::Left {
                        assert_eq!(sessions.right(), pane.x);
                    } else {
                        assert_eq!(pane.right(), sessions.x);
                    }
                }
                let support_in_content = targets.width == transcript.width;
                if support_in_content {
                    if side == mj_core::config::SessionsSide::Left {
                        assert_eq!(sessions.right(), targets.x);
                    } else {
                        assert_eq!(targets.right(), sessions.x);
                    }
                    assert_eq!(sessions.bottom(), quota.bottom());
                } else {
                    assert_eq!(targets.x, 0);
                    assert_eq!(targets.width, width);
                    assert_eq!(sessions.bottom(), targets.y);
                }
                assert_eq!(transcript.bottom(), prompt.y);
                assert_eq!(prompt.bottom(), targets.y);
                assert_eq!(targets.bottom(), quota.y);
                assert!(!lines.last().unwrap().trim().is_empty());
            }
        }
    }

    /// Collapsing the support panes hands their freed rows to the transcript,
    /// and the transcript also absorbs whatever the Sessions pane gives up (or
    /// gives back) as it moves to its fixed mode-2 third — nothing appears or
    /// vanishes, so the gesture is measurable rather than merely visible.
    #[test]
    fn minimizing_the_support_panes_gives_their_rows_to_the_transcript() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.set_deployment_capacity_targets(vec![test_capacity_target()]);
        dashboard.focus_prompt();
        /// Rows from the start of one band to the start of the next.
        fn band(lines: &[String], from: &str, to: &str) -> isize {
            let start = lines
                .iter()
                .position(|line| line.contains(from))
                .unwrap_or_else(|| panic!("missing {from}: {lines:#?}"));
            let end = lines
                .iter()
                .position(|line| line.contains(to))
                .unwrap_or_else(|| panic!("missing {to}: {lines:#?}"));
            (end - start) as isize
        }

        let before = drawn(&mut dashboard, 140, 44);
        minimize_all_panes(&mut dashboard);
        let after = drawn(&mut dashboard, 140, 44);

        // Every row the tables and the Sessions pane give up lands in the
        // transcript; the composer and footer are untouched.
        let tables_freed = (band(&before, "Targets", "Quota") - band(&after, "Targets", "Quota"))
            + (band(&before, "Quota", "Alt-Q detach") - band(&after, "Quota", "Alt-Q detach"));
        let sessions_freed = 0;
        let transcript_gain = band(&after, "Conversation", "No conversation open")
            - band(&before, "Conversation", "No conversation open");
        assert!(tables_freed > 0, "the tables gave up nothing");
        assert_eq!(transcript_gain, tables_freed + sessions_freed);
        // Each minimized pane really is one row.
        assert_eq!(band(&after, "Targets", "Quota"), 1);
        assert_eq!(band(&after, "Quota", "Alt-Q detach"), 1);
    }

    fn now_seconds() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    fn host_usage(cpu_percent: u8) -> mj_core::targets::DeploymentCapacityUsage {
        mj_core::targets::DeploymentCapacityUsage {
            cpu_percent: Some(cpu_percent),
            memory_used_bytes: 1,
            memory_total_bytes: 4,
            logical_cores: 8,
            disk_total_bytes: Some(64),
        }
    }

    /// A profile quota with `remaining` percent of its weekly window left.
    fn weekly_quota(profile_id: &str, remaining: u8) -> ProfileQuota {
        ProfileQuota {
            profile_id: profile_id.into(),
            harness: HarnessKind::Claude,
            windows: vec![QuotaWindow {
                label: "weekly".into(),
                remaining_percent: Some(remaining),
                used: None,
                limit: None,
                resets: None,
                resets_at_epoch_seconds: None,
            }],
            extra: None,
            error: None,
            refreshed_at_epoch_seconds: now_seconds(),
        }
    }

    /// A profile quota reporting both windows, the way the subscription
    /// harnesses do.
    fn weekly_and_five_hour_quota(profile_id: &str, weekly: u8, five_hour: u8) -> ProfileQuota {
        let mut quota = weekly_quota(profile_id, weekly);
        quota.windows.push(QuotaWindow {
            label: "5h".into(),
            remaining_percent: Some(five_hour),
            used: None,
            limit: None,
            resets: None,
            resets_at_epoch_seconds: None,
        });
        quota
    }

    /// What a usage-priced harness reports: no window at all, and the API
    /// label in place of one.
    fn api_quota(profile_id: &str) -> ProfileQuota {
        ProfileQuota {
            profile_id: profile_id.into(),
            harness: HarnessKind::Deepseek,
            windows: Vec::new(),
            extra: Some(API_LABEL.into()),
            error: None,
            refreshed_at_epoch_seconds: now_seconds(),
        }
    }

    /// Adds a usage-priced profile to the dashboard's configuration, since the
    /// shared fixture only carries subscription profiles.
    fn add_deepseek_profile(dashboard: &mut DashboardState) {
        dashboard.config.profiles.insert(
            "deepseek".into(),
            mj_core::config::HarnessProfile {
                enabled: true,
                context_window_bytes: None,
                kind: HarnessKind::Deepseek,
                home: std::path::PathBuf::from("/profiles/deepseek"),
                environment: BTreeMap::new(),
            },
        );
    }

    fn drawn(dashboard: &mut DashboardState, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal
            .draw(|frame| render(frame, dashboard))
            .expect("draw the combined surface");
        buffer_lines(terminal.backend().buffer())
    }

    /// An agent that is idle but left a command running says so, in the wide
    /// rows and in the minimized grid, from the one fact the daemon forwards.
    #[test]
    fn background_work_reaches_both_session_row_forms() {
        let started_at_ms =
            i64::try_from(mj_core::clock::epoch_seconds()).unwrap() * 1_000 - 2_616_000;
        let activity = mj_client::usage_format::SessionActivity {
            capacity_retry: None,
            activity_turn_started_at_ms: None,
            prompt_in_flight: false,
            idle_since_ms: None,
            execution: None,
            harness_turn_started_at_ms: None,
            foreground_tool_started_at_ms: None,
            background_commands: vec![mj_core::relay::BackgroundCommand {
                id: "test-background".into(),
                started_at_ms,
                command: "cargo test".into(),
                can_stop: false,
            }],
            active_user_shells: Vec::new(),
        };

        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        assert!(
            drawn(&mut dashboard, 120, 44)
                .iter()
                .any(|line| line.contains("No messages yet")),
            "an idle session with no current output keeps the output block stable"
        );

        dashboard.set_session_activity("session-1", activity.clone());
        dashboard
            .session_details
            .get_mut("session-1")
            .expect("session detail")
            .current_turn_started_at = None;
        let expanded = drawn(&mut dashboard, 120, 44);
        assert!(
            expanded
                .iter()
                .any(|line| line.contains("1 task") && line.contains("43m3")),
            "the expanded row: {expanded:?}"
        );

        let mut minimized = dashboard_with_session(running_session());
        minimized.set_session_activity("session-1", activity);
        minimize_all_panes(&mut minimized);
        let summary = drawn(&mut minimized, 120, 44).join("\n");
        assert!(summary.contains("1 task"), "{summary}");
        assert!(!summary.contains("You:"), "{summary}");
        assert!(!summary.contains("Agent:"), "{summary}");
    }

    /// Every expanded session is the same height, so the layout can be
    /// computed from a count and rows never jitter as messages arrive. A
    /// session with nothing to show still draws its two agent rows.
    #[test]
    fn an_expanded_session_always_draws_four_rows() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();

        let lines = drawn(&mut dashboard, 120, 44);
        let first = lines
            .iter()
            .position(|line| line.contains("ACP pretty name"))
            .expect("the session's name row");
        assert!(
            lines[first + 1].contains("podman"),
            "{:?}",
            lines[first + 1]
        );
        assert!(
            lines[first + 2].contains("No messages yet"),
            "{:?}",
            lines[first + 2]
        );
        // The fourth row is the second agent row, blank here because there is
        // only one line to show.
        assert!(
            lines[first + 3].trim_matches(['│', '║', ' ']).is_empty(),
            "{:?}",
            lines[first + 3]
        );
    }

    #[test]
    fn runtime_review_activity_is_visible_on_an_unselected_session_row() {
        let mut first = running_session();
        first.id = "session-first".into();
        let mut second = running_session();
        second.id = "session-second".into();
        second.created_at = "2026-08-10T00:00:00Z".into();
        let mut dashboard = DashboardState::new(
            config(),
            State {
                version: mj_core::state::STATE_VERSION,
                sessions: BTreeMap::from([(first.id.clone(), first), (second.id.clone(), second)]),
                mount_history: BTreeMap::new(),
                container_sizes: BTreeMap::new(),
            },
            BTreeMap::new(),
        );
        dashboard.set_session_reviews([RuntimeReviewView {
            session_id: "session-second".into(),
            tier: mj_core::review::lanes::ReviewTier::Quick,
            phase: mj_core::review::driver::TurnReviewPhase::Running { roles: Vec::new() },
            roles: Vec::new(),
            status: "the reviewer is reading the change…".into(),
            verdict: None,
        }]);

        // Collapse the project so the test exercises the compact two-line
        // form; the reviewed session is intentionally unselected.
        dashboard.focus_sessions();
        dashboard.handle_key(crate::test_support::key(KeyCode::Char('1')));
        let rendered = drawn(&mut dashboard, 140, 44).join("\n");
        let review_line = rendered
            .lines()
            .find(|line| line.contains("Reviewing"))
            .expect("review activity on the unselected compact row");
        assert!(!review_line.contains("Idle"), "{review_line}");

        // Removing the complete runtime projection restores the primary
        // session's ordinary activity clock.
        dashboard.set_session_reviews(Vec::new());
        let restored = drawn(&mut dashboard, 140, 44).join("\n");
        assert!(
            restored
                .lines()
                .any(|line| line.contains("podman") && line.contains("Idle")),
            "{restored}"
        );

        // The minimized list uses the same compact activity slot and also
        // must not pair a live review with the primary's idle marker.
        dashboard.set_session_reviews([RuntimeReviewView {
            session_id: "session-second".into(),
            tier: mj_core::review::lanes::ReviewTier::Quick,
            phase: mj_core::review::driver::TurnReviewPhase::Running { roles: Vec::new() },
            roles: Vec::new(),
            status: "the reviewer is reading the change…".into(),
            verdict: None,
        }]);
        minimize_all_panes(&mut dashboard);
        let minimized = drawn(&mut dashboard, 140, 44).join("\n");
        let minimized_line = minimized
            .lines()
            .find(|line| line.contains("Reviewing"))
            .expect("review activity in the minimized list");
        let review_cell =
            &minimized_line[minimized_line.find("Reviewing").expect("review label")..];
        assert!(!review_cell.contains("Idle"), "{minimized_line}");
    }

    /// `projects` projects, `per_project` live sessions in each, laid out so
    /// the minimized list has headings and enough sessions to scroll. Project
    /// directories are zero-padded so they sort in the obvious order.
    fn minimized_sessions_dashboard(projects: usize, per_project: usize) -> DashboardState {
        let mut sessions = BTreeMap::new();
        let mut index = 0;
        for project in 0..projects {
            for _ in 0..per_project {
                let mut session = running_session();
                session.id = format!("session-{index:02}");
                session.created_at = format!("2026-08-{:02}T00:00:00Z", index + 1);
                session.project_directory = Some(format!("/projects/proj{project:02}").into());
                sessions.insert(session.id.clone(), session);
                index += 1;
            }
        }
        let mut dashboard = DashboardState::new(
            config(),
            State {
                version: STATE_VERSION,
                sessions,
                mount_history: BTreeMap::new(),
                container_sizes: BTreeMap::new(),
            },
            BTreeMap::new(),
        );
        // One turn of the dial reaches the minimized list.
        minimize_all_panes(&mut dashboard);
        dashboard
    }

    /// The content rows of the Sessions pane (inside its border) for a
    /// minimized list at the given terminal size.
    fn minimized_content_rows(
        dashboard: &mut DashboardState,
        width: u16,
        height: u16,
    ) -> Vec<String> {
        let lines = drawn(dashboard, width, height);
        let pane = dashboard.pane_areas.unwrap()[0];
        lines[usize::from(pane.y + 1)..usize::from(pane.bottom() - 1)]
            .iter()
            .map(|line| {
                line.chars()
                    .skip(usize::from(pane.x) + 1)
                    .take(usize::from(pane.width) - 2)
                    .collect()
            })
            .collect()
    }

    /// Minimized Sessions keeps the familiar vertical list but drops every
    /// transcript preview beneath the session's summary line.
    #[test]
    fn minimized_sessions_keep_summary_rows_without_message_previews() {
        let mut dashboard = minimized_sessions_dashboard(3, 2);
        let rendered = drawn(&mut dashboard, 120, 44).join("\n");
        assert!(!dashboard.session_row_areas.is_empty());
        assert!(rendered.contains("ACP pretty"), "{rendered}");
        assert!(!rendered.contains("You:"), "{rendered}");
        assert!(!rendered.contains("Agent:"), "{rendered}");
    }

    /// A busy minimized list keeps a bounded height and two-line hitboxes.
    #[test]
    fn minimized_sessions_bound_the_viewport_to_preserve_the_conversation() {
        let mut dashboard = minimized_sessions_dashboard(3, 3);
        let lines = drawn(&mut dashboard, 120, 20);
        assert_eq!(dashboard.pane_areas.expect("pane geometry")[0].height, 16);
        assert!(
            dashboard
                .session_row_areas
                .iter()
                .all(|(_, area)| area.height == 2)
        );
        assert!(!lines.iter().any(|line| line.contains("You:")), "{lines:?}");
    }

    /// A sparse minimized list uses one row for its heading and each session.
    #[test]
    fn a_sparse_minimized_list_shows_its_heading_and_sessions() {
        let mut dashboard = minimized_sessions_dashboard(1, 2);
        let lines = drawn(&mut dashboard, 120, 44);
        assert!(
            !lines.iter().any(|line| line.contains("more")),
            "no marker expected when all sessions fit: {lines:?}"
        );
        assert_eq!(dashboard.pane_areas.expect("pane geometry")[0].height, 40);
    }

    /// The minimized row keeps the session name and its actionable status.
    #[test]
    fn minimized_sessions_identify_the_session_by_name() {
        let mut dashboard = minimized_sessions_dashboard(1, 1);
        let rendered = drawn(&mut dashboard, 120, 44).join("\n");
        assert!(rendered.contains("ACP pretty"), "{rendered}");
        assert!(rendered.contains("Idle"), "{rendered}");
        let rows = minimized_content_rows(&mut dashboard, 120, 44);
        let status = rows.iter().find(|line| line.contains("Idle")).unwrap();
        assert!(status.starts_with("  Idle"), "{status:?}");
    }

    /// A session row is coloured by the same state rule the expanded rows
    /// use: idle is blue, active work yellow, and a failed session red.
    #[test]
    fn the_minimized_list_colours_a_session_row_by_state() {
        let colour_of = |mut dashboard: DashboardState| {
            let mut terminal = Terminal::new(TestBackend::new(120, 44)).expect("terminal");
            terminal
                .draw(|frame| render(frame, &mut dashboard))
                .expect("draw the minimized list");
            let buffer = terminal.backend().buffer();
            let lines = buffer_lines(buffer);
            let row = lines
                .iter()
                .position(|line| line.contains("ACP pretty"))
                .expect("a session row");
            buffer[(cell_column(&lines[row], "ACP pretty"), row as u16)].fg
        };

        let healthy = minimized_sessions_dashboard(1, 1);
        assert_eq!(colour_of(healthy), theme::palette().session_idle);

        let mut busy = minimized_sessions_dashboard(1, 1);
        busy.session_details
            .get_mut("session-00")
            .expect("the session detail")
            .current_turn_started_at = Some(1);
        assert_eq!(colour_of(busy), theme::palette().session_activity);

        let mut failed = minimized_sessions_dashboard(1, 1);
        {
            let session = failed
                .state
                .sessions
                .get_mut("session-00")
                .expect("the session");
            session.state = SessionState::Error;
        }
        assert_eq!(colour_of(failed), theme::palette().session_error);
    }

    /// A narrow minimized pane truncates the summary without bringing back
    /// the message previews.
    #[test]
    fn minimized_sessions_truncate_only_the_top_line() {
        let mut dashboard = minimized_sessions_dashboard(1, 1);
        dashboard
            .state
            .sessions
            .get_mut("session-00")
            .expect("the session")
            .target_template_id = "extremely-long-target-identifier".into();

        let rows = minimized_content_rows(&mut dashboard, 80, 22);
        assert!(
            rows.iter().any(|line| line.contains("ACP pretty")),
            "the narrow summary keeps the session name: {rows:?}"
        );
        assert!(!rows.iter().any(|line| line.contains("You:")), "{rows:?}");
        assert!(!rows.iter().any(|line| line.contains("Agent:")), "{rows:?}");
    }

    /// The minimized list is a viewport: selecting a session past the visible
    /// rows scrolls the window so it shows, and earlier rows leave view.
    #[test]
    fn the_minimized_list_scrolls_to_keep_the_selection_visible() {
        let mut dashboard = minimized_sessions_dashboard(12, 1);

        // Selecting the first session keeps the window at the start.
        dashboard.selected_session_id = Some("session-00".into());
        let rows = minimized_content_rows(&mut dashboard, 120, 20);
        assert!(
            rows.iter().any(|line| line.contains("proj00")),
            "first project visible: {rows:?}"
        );
        assert!(
            !rows.iter().any(|line| line.contains("proj11")),
            "last project not yet visible: {rows:?}"
        );

        // Selecting the last session scrolls it into view and the first out.
        dashboard.selected_session_id = Some("session-11".into());
        let rows = minimized_content_rows(&mut dashboard, 120, 20);
        assert!(
            rows.iter().any(|line| line.contains("proj11")),
            "last project scrolled into view: {rows:?}"
        );
        assert!(
            !rows.iter().any(|line| line.contains("proj00")),
            "first project scrolled out: {rows:?}"
        );
    }

    /// Clicking a minimized row selects that session and leaves the dial where
    /// the user set it; the list draws the selection itself.
    #[test]
    fn clicking_a_minimized_row_selects_it_and_keeps_the_list() {
        use crossterm::event::{MouseButton, MouseEventKind};

        let mut dashboard = minimized_sessions_dashboard(2, 2);
        drawn(&mut dashboard, 120, 44);

        let (index, rect) = *dashboard
            .session_row_areas
            .first()
            .expect("a minimized row hitbox");
        let expected = dashboard.ordered_sessions()[index].id.clone();

        dashboard.handle_mouse(mouse_at_row(
            MouseEventKind::Down(MouseButton::Left),
            rect,
            0,
        ));

        assert_eq!(dashboard.selected_session_id(), Some(expected.as_str()));
        assert!(
            dashboard.sessions_minimized(),
            "the click should leave the minimized list alone"
        );
        assert_eq!(dashboard.focus(), Focus::Sessions);
    }

    #[test]
    fn a_short_terminal_keeps_every_minimized_title_and_control() {
        let mut dashboard = minimized_sessions_dashboard(2, 2);
        dashboard.set_deployment_capacity_targets(vec![test_capacity_target()]);
        dashboard.apply_quota(weekly_quota("claude-1", 63));

        let lines = drawn(&mut dashboard, 120, 20);
        let pane = dashboard.pane_areas.expect("short minimized pane")[0];

        assert!(
            lines[usize::from(pane.y)].contains('╭')
                && lines[usize::from(pane.y)].contains("Sessi"),
            "the minimized list keeps its title and border: {lines:?}"
        );
        assert!(
            lines[usize::from(pane.y)].contains('▁')
                && lines[usize::from(pane.y)].contains('▪')
                && lines[usize::from(pane.y)].contains('□')
        );
        for visible in ["Targets", "Quota"] {
            assert!(
                lines.iter().any(|line| line.contains(visible)),
                "{visible} should remain visible: {lines:?}"
            );
        }
        let pane = dashboard.pane_areas.expect("pane geometry")[0];
        let selection = dashboard
            .frame_surfaces()
            .surface(SurfaceId::DashboardPane(0))
            .expect("tiny minimized selection surface");
        assert_eq!(selection.rect, pane.inner(Margin::new(1, 1)));
        // Targets and Quota fit beside the minimized sidebar at this width.
        assert_eq!(dashboard.pane_areas.unwrap()[1].x, pane.right());
        assert_eq!(selection.rect.height, 14);
    }

    #[test]
    fn the_minimized_list_follows_the_terminal_height_each_frame() {
        let mut dashboard = minimized_sessions_dashboard(3, 2);

        let tall = drawn(&mut dashboard, 120, 44);
        let tall_pane = dashboard.pane_areas.expect("tall panes")[0];
        assert!(
            tall[usize::from(tall_pane.y)].contains('╭')
                && tall[usize::from(tall_pane.y)].contains("Sessi")
        );
        assert_eq!(dashboard.pane_areas.expect("tall panes")[0].height, 40);

        let short = drawn(&mut dashboard, 120, 20);
        let short_pane = dashboard.pane_areas.expect("short panes")[0];
        assert!(
            short[usize::from(short_pane.y)].contains('╭')
                && short[usize::from(short_pane.y)].contains("Sessi")
        );
        assert_eq!(dashboard.pane_areas.expect("short panes")[0].height, 16);

        drawn(&mut dashboard, 120, 44);
        assert_eq!(
            dashboard.pane_areas.expect("tall panes again")[0].height,
            40
        );
    }

    /// Minimized Sessions draws a sparse list on a landscape terminal.
    #[test]
    fn minimized_on_a_landscape_terminal_draws_the_list() {
        let mut dashboard = dashboard_with_session(running_session());
        minimize_all_panes(&mut dashboard);

        let lines = drawn(&mut dashboard, 120, 40);

        assert!(dashboard.sessions_minimized());
        let sessions_height = lines
            .iter()
            .position(|line| line.contains("Conversation"))
            .expect("the conversation band");
        assert_eq!(sessions_height, 0, "{lines:#?}");
    }

    /// A brand-new workspace has no sessions at all, and minimizing Sessions
    /// there must still draw rather than fall over on an empty list.
    #[test]
    fn the_minimized_list_draws_with_no_sessions() {
        let mut dashboard = DashboardState::new(config(), State::default(), BTreeMap::new());
        minimize_all_panes(&mut dashboard);

        let lines = drawn(&mut dashboard, 200, 50);
        assert!(
            lines.iter().any(|line| line.contains("Conversation")),
            "{lines:#?}"
        );
    }

    #[test]
    fn minimized_on_a_portrait_terminal_still_draws_the_list() {
        let mut dashboard = dashboard_with_session(running_session());
        minimize_all_panes(&mut dashboard);

        let (width, height) = (80u16, 120u16);
        let lines = drawn(&mut dashboard, width, height);

        assert!(dashboard.sessions_minimized());
        let sessions_height = lines
            .iter()
            .position(|line| line.contains("Conversation"))
            .expect("the conversation band");
        assert_eq!(sessions_height, 0, "{lines:#?}");
    }

    #[test]
    fn a_short_portrait_terminal_keeps_the_list_and_support_summaries() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.set_deployment_capacity_targets(vec![test_capacity_target()]);
        dashboard.apply_quota(weekly_quota("claude-1", 63));
        minimize_all_panes(&mut dashboard);

        let lines = drawn(&mut dashboard, 80, 38);

        let pane = dashboard.pane_areas.expect("pane geometry")[0];
        assert!(
            lines[usize::from(pane.y)].contains('╭') && pane.width > 0,
            "the portrait list keeps its bordered Sessions title: {lines:?}"
        );
        let panes = dashboard.pane_areas.expect("pane geometry");
        assert_eq!(panes[0].bottom(), panes[2].bottom());
        assert_eq!(panes[1].x, panes[0].right());
        assert_eq!(panes[1].width, 60);
        for visible in ["Tar", "Quo"] {
            assert!(
                lines.iter().any(|line| line.contains(visible)),
                "{visible} should remain: {lines:?}"
            );
        }
    }

    #[test]
    fn the_minimized_rows_report_cpu_and_weekly_percent_used() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.set_deployment_capacity_targets(vec![test_capacity_target()]);
        dashboard.apply_deployment_capacity("local", Ok(Some(host_usage(42))), now_seconds());
        dashboard.apply_quota(weekly_quota("claude-1", 63));
        minimize_all_panes(&mut dashboard);

        let lines = drawn(&mut dashboard, 120, 44);
        let targets = lines
            .iter()
            .find(|line| line.contains("─ Targets ──"))
            .expect("the minimized Targets row");
        assert!(targets.contains("local 42%"), "{targets:?}");
        let quota = lines
            .iter()
            .find(|line| line.contains("─ Quota ──"))
            .expect("the minimized Quota row");
        // The open pane prints the remaining percentage; so does this row.
        assert!(quota.contains("claude-1 63%"), "{quota:?}");
        // Each minimized pane is exactly one row.
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.contains("─ Targets ──"))
                .count(),
            1
        );
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.contains("─ Quota ──"))
                .count(),
            1
        );
    }

    /// A fleet with `count` machines running, which is what its probe list
    /// records: one probe per live instance.
    fn fleet_target(count: usize) -> mj_core::targets::DeploymentCapacityTarget {
        mj_core::targets::DeploymentCapacityTarget {
            id: "aws:ec2".into(),
            host: "ec2".into(),
            target_ids: vec!["ec2".into()],
            kind: DeploymentCapacityKind::AwsFleet,
            local: false,
            probes: (0..count)
                .map(|index| {
                    mj_core::targets::CommandSpec::new("true", [format!("instance-{index}")])
                })
                .collect(),
            probe_error: None,
        }
    }

    /// A fleet has no CPU percentage of its own, so what it reports in use is
    /// how many machines it is running - including when that is none, which
    /// used to read "on demand" and said nothing about the fleet's state.
    #[test]
    fn a_fleet_reports_how_many_machines_it_is_running() {
        for (count, expected) in [(0, "0 VMs"), (1, "1 VM"), (3, "3 VMs")] {
            let mut dashboard = dashboard_with_session(running_session());
            dashboard.set_deployment_capacity_targets(vec![fleet_target(count)]);
            if count > 0 {
                dashboard.apply_deployment_capacity(
                    "aws:ec2",
                    Ok(Some(mj_core::targets::DeploymentCapacityUsage {
                        cpu_percent: None,
                        memory_used_bytes: 0,
                        memory_total_bytes: 8,
                        logical_cores: 4,
                        disk_total_bytes: Some(64),
                    })),
                    now_seconds(),
                );
            } else {
                dashboard.apply_deployment_capacity("aws:ec2", Ok(None), now_seconds());
            }

            let open = drawn(&mut dashboard, 140, 44).join("\n");
            assert!(open.contains(expected), "open pane, {count}: {open}");
            assert!(!open.contains("on demand"), "open pane, {count}: {open}");

            minimize_all_panes(&mut dashboard);
            let minimized = drawn(&mut dashboard, 140, 44)
                .into_iter()
                .find(|line| line.contains("─ Targets ──"))
                .expect("the minimized Targets row");
            assert!(
                minimized.contains(&format!("ec2 {expected}")),
                "minimized row, {count}: {minimized}"
            );
            assert!(!minimized.contains("no CPU"), "minimized row, {count}");
        }
    }

    /// An exhausted profile reads 0%, the same as the open pane's bar. Showing
    /// how much has been *used* would read 100% there, which looks like a
    /// profile in the best possible shape rather than one with nothing left.
    #[test]
    fn an_exhausted_quota_reads_zero_in_the_minimized_row() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.apply_quota(weekly_quota("claude-1", 0));
        minimize_all_panes(&mut dashboard);

        let quota = drawn(&mut dashboard, 120, 44)
            .into_iter()
            .find(|line| line.contains("─ Quota ──"))
            .expect("the minimized Quota row");
        assert!(quota.contains("claude-1 0%"), "{quota:?}");
        assert!(!quota.contains("claude-1 100%"), "{quota:?}");
    }

    /// A reading that cannot be trusted has to say so. A number that is
    /// actually missing, stale or inapplicable is worse than no number.
    #[test]
    fn the_minimized_rows_stay_explicit_about_readings_they_do_not_have() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.set_deployment_capacity_targets(vec![test_capacity_target()]);
        minimize_all_panes(&mut dashboard);

        // No sample at all.
        let lines = drawn(&mut dashboard, 120, 44);
        let targets = |lines: &[String]| {
            lines
                .iter()
                .find(|line| line.contains("─ Targets ──"))
                .expect("the minimized Targets row")
                .clone()
        };
        assert!(targets(&lines).contains("local unavailable"));

        // A probe in flight.
        dashboard.begin_capacity_refresh();
        assert!(targets(&drawn(&mut dashboard, 120, 44)).contains("local refreshing…"));

        // A sample too old to trust.
        dashboard.apply_deployment_capacity(
            "local",
            Ok(Some(host_usage(7))),
            now_seconds() - CAPACITY_SAMPLE_STALE_AFTER_SECONDS - 60,
        );
        assert!(
            targets(&drawn(&mut dashboard, 120, 44)).contains("local 7% (stale)"),
            "{:?}",
            targets(&drawn(&mut dashboard, 120, 44))
        );

        // A quota that failed to refresh.
        let quota = drawn(&mut dashboard, 120, 44)
            .into_iter()
            .find(|line| line.contains("─ Quota ──"))
            .expect("the minimized Quota row");
        assert!(quota.contains("claude-1 unavailable"), "{quota:?}");
    }

    /// A failed session used to render identically to a healthy one, so the
    /// only thing that told you it had failed was pressing Enter on it. Red
    /// is the same signal an unreachable relay carries: this row needs
    /// attention rather than reading.
    #[test]
    fn a_failed_session_draws_a_red_summary_at_both_pane_sizes() {
        for size in [PaneSize::Standard, PaneSize::Minimized] {
            let mut healthy = dashboard_with_session(running_session());
            healthy.set_pane_size(SupportPane::Sessions, size);
            let mut failed = {
                let mut session = running_session();
                session.state = SessionState::Error;
                session.last_error = Some("worker bootstrap failed".into());
                dashboard_with_session(session)
            };
            failed.set_pane_size(SupportPane::Sessions, size);

            let row_colour = |dashboard: &mut DashboardState| {
                let mut terminal = Terminal::new(TestBackend::new(120, 44)).expect("terminal");
                terminal
                    .draw(|frame| render(frame, dashboard))
                    .expect("draw the session list");
                let buffer = terminal.backend().buffer();
                let lines = buffer_lines(buffer);
                let status = if dashboard
                    .state
                    .sessions
                    .values()
                    .any(|session| session.state == SessionState::Error)
                {
                    "Error"
                } else {
                    "Idle"
                };
                let row = lines
                    .iter()
                    .position(|line| line.contains(status))
                    .unwrap_or_else(|| panic!("the session's row ({status}): {lines:?}"));
                let column = cell_column(&lines[row], status);
                buffer[(column, row as u16)].fg
            };

            assert_eq!(row_colour(&mut failed), theme::palette().error, "{size:?}");
            assert_ne!(
                row_colour(&mut healthy),
                theme::palette().error,
                "{size:?}: only a session that needs attention is red"
            );
        }
    }

    /// The minimized rows read as pane titles: the pane's rule, plain text,
    /// and readings separated by commas. Colour is the only thing carrying
    /// meaning, and it comes from the same scale the full quota bar uses.
    #[test]
    fn the_minimized_rows_keep_the_pane_rule_and_colour_by_headroom() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.set_deployment_capacity_targets(vec![
            test_capacity_target(),
            mj_core::targets::DeploymentCapacityTarget {
                id: "morannon".into(),
                host: "morannon".into(),
                ..test_capacity_target()
            },
        ]);
        // A quiet host has headroom; a busy one does not.
        dashboard.apply_deployment_capacity("local", Ok(Some(host_usage(3))), now_seconds());
        dashboard.apply_deployment_capacity("morannon", Ok(Some(host_usage(95))), now_seconds());
        // Plenty of the weekly window left.
        dashboard.apply_quota(weekly_quota("claude-1", 63));
        // Nearly none, and in trouble.
        dashboard.apply_quota(weekly_quota("codex-1", 10));
        minimize_all_panes(&mut dashboard);

        let mut terminal = Terminal::new(TestBackend::new(120, 44)).expect("terminal");
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw the minimized panes");
        let buffer = terminal.backend().buffer();
        let lines = buffer_lines(buffer);

        let targets_row = lines
            .iter()
            .position(|line| line.contains("─ Targets ──"))
            .expect("the minimized Targets row");
        let targets = &lines[targets_row];
        assert!(targets.contains("local 3%, morannon 95%"), "{targets:?}");
        assert!(targets.contains("─ ▁   ▪ "), "{targets:?}");

        let quota_row = lines
            .iter()
            .position(|line| line.contains("─ Quota ──"))
            .expect("the minimized Quota row");
        assert!(
            lines[quota_row].contains("claude-1 63%, codex-1 10%"),
            "the row reads the remaining percentage the open pane prints: {:?}",
            lines[quota_row]
        );

        // The colour of a value, by the column its first digit sits in.
        let colour_of = |row: usize, needle: &str| {
            let column = cell_column(&lines[row], needle);
            buffer[(column, row as u16)].fg
        };
        // A quiet host has headroom left, a busy one does not; a quota reads
        // the same scale on the headroom it reports.
        assert_eq!(colour_of(targets_row, "3%"), theme::palette().success);
        assert_eq!(colour_of(targets_row, "95%"), theme::palette().error);
        assert_eq!(colour_of(quota_row, "63%"), theme::palette().success);
        assert_eq!(colour_of(quota_row, "10%"), theme::palette().error);
        // The label and the names are ordinary text; only the values carry a
        // colour.
        assert_eq!(colour_of(targets_row, "Targets"), theme::palette().text);
        assert_eq!(colour_of(targets_row, "morannon"), theme::palette().text);
    }

    /// A usage-priced profile has no window to summarise, so it is left out of
    /// the minimized row rather than spending width on a placeholder - both
    /// once its own report says it is API-priced and before that report has
    /// arrived.
    #[test]
    fn a_usage_priced_profile_is_absent_from_the_minimized_row() {
        let mut dashboard = dashboard_with_session(running_session());
        add_deepseek_profile(&mut dashboard);
        minimize_all_panes(&mut dashboard);

        let row = |dashboard: &mut DashboardState| {
            drawn(dashboard, 160, 44)
                .into_iter()
                .find(|line| line.contains("─ Quota ──"))
                .expect("the minimized Quota row")
        };

        // Before any refresh, the profile's kind is the only signal there is.
        let before = row(&mut dashboard);
        assert!(!before.contains("deepseek"), "{before:?}");

        // And once the report arrives, the report itself says so.
        dashboard.apply_quota(api_quota("deepseek"));
        let after = row(&mut dashboard);
        assert!(!after.contains("deepseek"), "{after:?}");
        assert!(!after.contains("api"), "{after:?}");
        // The subscription profiles still read normally.
        assert!(after.contains("claude-1"), "{after:?}");
    }

    /// A week with headroom left is no comfort while the next five hours are
    /// spent, so a profile that has dipped into its week reports both figures.
    /// An untouched week says everything there is to say on its own, and a
    /// profile with no five-hour window has nothing more to add.
    #[test]
    fn the_minimized_row_pairs_the_weekly_and_five_hour_figures() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.apply_quota(weekly_and_five_hour_quota("claude-1", 96, 40));
        dashboard.apply_quota(weekly_and_five_hour_quota("codex-1", 100, 40));
        dashboard.apply_quota(weekly_quota("codex-2", 63));
        minimize_all_panes(&mut dashboard);

        let quota = drawn(&mut dashboard, 160, 44)
            .into_iter()
            .find(|line| line.contains("─ Quota ──"))
            .expect("the minimized Quota row");
        assert!(quota.contains("claude-1 96%/40%"), "{quota:?}");
        assert!(quota.contains("codex-1 100%,"), "{quota:?}");
        assert!(!quota.contains("100%/"), "{quota:?}");
        assert!(quota.contains("codex-2 63%"), "{quota:?}");
        assert!(!quota.contains("63%/"), "{quota:?}");
    }

    /// The reading's colour has to describe the window that is actually
    /// running out: a profile with most of its week left but no five-hour
    /// headroom is in trouble now.
    #[test]
    fn the_paired_reading_takes_the_colour_of_the_tighter_window() {
        let mut dashboard = dashboard_with_session(running_session());
        // Plenty of week, almost no five hours.
        dashboard.apply_quota(weekly_and_five_hour_quota("claude-1", 96, 5));
        // Almost no week, plenty of five hours.
        dashboard.apply_quota(weekly_and_five_hour_quota("codex-1", 8, 90));
        minimize_all_panes(&mut dashboard);

        let mut terminal = Terminal::new(TestBackend::new(160, 44)).expect("terminal");
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw the minimized panes");
        let buffer = terminal.backend().buffer();
        let lines = buffer_lines(buffer);
        let row = lines
            .iter()
            .position(|line| line.contains("─ Quota ──"))
            .expect("the minimized Quota row");
        let colour_of = |needle: &str| {
            let column = cell_column(&lines[row], needle);
            buffer[(column, row as u16)].fg
        };

        assert_eq!(colour_of("96%/5%"), theme::palette().error);
        assert_eq!(colour_of("8%/90%"), theme::palette().error);
    }

    /// A minimized pane is one row by definition, so more hosts than fit have
    /// to be cut rather than wrapped onto a second row.
    #[test]
    fn the_minimized_rows_truncate_rather_than_wrap() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.set_deployment_capacity_targets(
            (0..8)
                .map(|index| mj_core::targets::DeploymentCapacityTarget {
                    id: format!("host-{index}"),
                    host: format!("a-rather-long-host-name-{index}"),
                    ..test_capacity_target()
                })
                .collect(),
        );
        minimize_all_panes(&mut dashboard);

        let lines = drawn(&mut dashboard, 80, 44);
        let rows = lines
            .iter()
            .filter(|line| line.contains("─ Targets ──"))
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), 1, "{lines:#?}");
        assert!(rows[0].chars().count() <= 80);
        assert!(
            rows[0].contains('…'),
            "the readings are cut rather than wrapped: {:?}",
            rows[0]
        );
        assert!(rows[0].contains("─ ▁   ▪ "), "{:?}", rows[0]);
    }

    #[test]
    fn read_idle_session_stays_blue_in_expanded_and_collapsed_rows() {
        let mut session = stopped_session();
        session.state = SessionState::Running;
        // The detach cursor sits past the only agent message, so nothing is
        // unread; a truly idle live session is still blue.
        session.viewed_through_event_ordinal = 1;
        for collapsed in [false, true] {
            let mut dashboard = dashboard_with_session(session.clone());
            dashboard.focus = Focus::Quota;
            let mut materialized =
                materialized_session_for("session-1", vec![agent_message(1, "seen response")]);
            materialized.execution = MaterializedExecutionState::Idle;
            dashboard.apply_materialized_session(&materialized);
            if collapsed {
                dashboard.focus_sessions();
                dashboard.handle_key(crate::test_support::key(KeyCode::Char('1')));
                dashboard.focus = Focus::Quota;
            }

            let mut terminal = Terminal::new(TestBackend::new(140, 28)).expect("terminal");
            terminal
                .draw(|frame| render(frame, &mut dashboard))
                .expect("draw dashboard");
            let buffer = terminal.backend().buffer();
            let status_y = (buffer.area.y..buffer.area.bottom())
                .find(|y| {
                    let row = (buffer.area.x..buffer.area.right())
                        .map(|x| buffer[(x, *y)].symbol())
                        .collect::<String>();
                    row.contains("podman")
                })
                .expect("the session's summary row");
            let status = (buffer.area.x..buffer.area.right())
                .map(|x| buffer[(x, status_y)].symbol())
                .collect::<String>();
            assert!(!status.contains("unread"));
            let pane = dashboard.pane_areas.expect("pane areas")[0];
            assert!(
                (pane.x + 1..pane.right() - 1)
                    .filter(|x| summary_text_cell(&buffer[(*x, status_y)]))
                    .all(|x| buffer[(x, status_y)].fg == theme::palette().session_idle),
                "{collapsed}: {status}"
            );
        }
    }

    #[test]
    fn session_name_prefers_override_then_acp_title_then_hel_uuid() {
        let mut session = stopped_session();
        assert_eq!(session_name(&session), "ACP pretty name");

        session.acp_session_title = None;
        assert_eq!(session_name(&session), "session-1");

        session.session_title_override = Some("My name".into());
        assert_eq!(session_name(&session), "My name");

        session.session_title_override = None;
        session.native_session_id = None;
        assert_eq!(session_name(&session), "session-1");
        assert_ne!(session_name(&session), session.title);
    }

    /// A capacity sample the poller keeps refreshing carries no clock column
    /// and no staleness marker: the number on screen is the current one.
    #[test]
    fn capacity_pane_renders_grouped_host_load_without_sample_clock() {
        let mut dashboard = DashboardState::new(config(), State::default(), BTreeMap::new());
        let mut target = test_capacity_target();
        target.target_ids = vec!["podman".into(), "mac-container".into()];
        dashboard.set_deployment_capacity_targets(vec![target]);
        dashboard.apply_deployment_capacity(
            "local",
            Ok(Some(DeploymentCapacityUsage {
                cpu_percent: Some(37),
                memory_used_bytes: 3,
                memory_total_bytes: 4,
                logical_cores: 8,
                disk_total_bytes: None,
            })),
            now_epoch_seconds(),
        );
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).expect("terminal");

        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw dashboard");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(
            rendered.contains("podman, mac-container  37% CPU"),
            "{rendered}"
        );
        assert!(rendered.contains("37% CPU · 75% RAM"));
        assert!(!rendered.contains("Sample"));
        assert!(!rendered.contains("stale"));
        let buffer = terminal.backend().buffer();
        let header = (buffer.area.y..buffer.area.bottom())
            .map(|y| {
                (buffer.area.x..buffer.area.right())
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .find(|line| line.contains("Host / fleet") && line.contains("Targets"))
            .expect("capacity header");
        assert!(header.contains("In Use"));
    }

    #[test]
    fn dashboard_colors_named_host_permission_badges() {
        let mut config = config();
        let container = match config.targets["podman"].clone() {
            mj_core::config::TargetTemplate::LocalPodman { container } => container,
            _ => unreachable!(),
        };
        let ssh = |host: &str| mj_core::config::SshConnection {
            host: host.into(),
            user: None,
            identity_file: None,
            extra_args: Vec::new(),
        };
        config.targets.insert(
            "precision-3260".into(),
            mj_core::config::TargetTemplate::SshBare {
                ssh: ssh("precision-3260"),
                permissions: PermissionMode::Yolo,
                workspace_prefix: ".local/share/hel/workspaces".into(),
            },
        );
        config.targets.insert(
            "morannon-podman".into(),
            mj_core::config::TargetTemplate::SshPodman {
                ssh: ssh("morannon"),
                container,
            },
        );
        config.targets.insert(
            "morannon-raw".into(),
            mj_core::config::TargetTemplate::SshBare {
                ssh: ssh("morannon"),
                permissions: PermissionMode::Guardian,
                workspace_prefix: ".local/share/hel/workspaces".into(),
            },
        );
        let mut session = running_session();
        session.target_template_id = "precision-3260".into();
        session.project_directory = Some("/home/dev/hel".into());
        let state = State {
            version: STATE_VERSION,
            sessions: BTreeMap::from([(session.id.clone(), session)]),
            mount_history: BTreeMap::new(),
            container_sizes: BTreeMap::new(),
        };
        let mut dashboard = DashboardState::new(config, state, BTreeMap::new());
        let capacity_target =
            |host: &str, target_ids: &[&str]| mj_core::targets::DeploymentCapacityTarget {
                id: format!("ssh:{host}"),
                host: host.into(),
                target_ids: target_ids.iter().map(|id| (*id).into()).collect(),
                kind: DeploymentCapacityKind::Host,
                local: false,
                probes: Vec::new(),
                probe_error: None,
            };
        dashboard.set_deployment_capacity_targets(vec![
            capacity_target("precision-3260", &["precision-3260"]),
            capacity_target("morannon", &["morannon-podman", "morannon-raw"]),
        ]);
        let mut terminal = Terminal::new(TestBackend::new(160, 40)).expect("terminal");

        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw dashboard");

        let buffer = terminal.backend().buffer();
        let lines = buffer_lines(buffer);
        let badge_has_color = |needle: &str, color: Color| {
            lines.iter().enumerate().any(|(row, line)| {
                let Some(byte) = line.find(needle) else {
                    return false;
                };
                let x = buffer.area.x + line[..byte].chars().count() as u16;
                (x..x + 3).all(|x| buffer[(x, buffer.area.y + row as u16)].fg == color)
            })
        };
        let rendered = lines.join("\n");
        assert!(rendered.contains("precision-3260 [Y]"), "{rendered}");
        assert!(
            rendered.contains("morannon-podman, morannon-raw [G]"),
            "{rendered}"
        );
        assert!(!rendered.contains("morannon-podman [G]"), "{rendered}");
        assert!(badge_has_color("[Y]", theme::palette().error), "{rendered}");
        assert!(
            badge_has_color("[G]", theme::palette().success),
            "{rendered}"
        );
    }

    fn now_epoch_seconds() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    fn host_capacity_usage() -> DeploymentCapacityUsage {
        DeploymentCapacityUsage {
            cpu_percent: Some(37),
            memory_used_bytes: 3,
            memory_total_bytes: 4,
            logical_cores: 8,
            disk_total_bytes: None,
        }
    }

    fn drawn_dashboard(dashboard: &mut DashboardState, width: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, 40)).expect("terminal");
        terminal
            .draw(|frame| render(frame, dashboard))
            .expect("draw dashboard");
        buffer_lines(terminal.backend().buffer()).join("\n")
    }

    /// A probe that failed and a reading that stopped refreshing both keep the
    /// last numbers on screen and say why they cannot be trusted, instead of
    /// rendering exactly like a reading taken a moment ago.
    #[test]
    fn capacity_rows_mark_a_failed_probe_and_a_sample_that_stopped_refreshing() {
        let mut failed = DashboardState::new(config(), State::default(), BTreeMap::new());
        failed.set_deployment_capacity_targets(vec![test_capacity_target()]);
        failed.apply_deployment_capacity(
            "local",
            Ok(Some(host_capacity_usage())),
            now_epoch_seconds(),
        );
        failed.apply_deployment_capacity(
            "local",
            Err("probe timed out".into()),
            now_epoch_seconds(),
        );
        let rendered = drawn_dashboard(&mut failed, 200);
        assert!(rendered.contains("37% CPU · 75% RAM"), "{rendered}");
        assert!(rendered.contains("stale: probe timed out"), "{rendered}");

        let mut aged = DashboardState::new(config(), State::default(), BTreeMap::new());
        aged.set_deployment_capacity_targets(vec![test_capacity_target()]);
        aged.apply_deployment_capacity(
            "local",
            Ok(Some(host_capacity_usage())),
            now_epoch_seconds().saturating_sub(3_600),
        );
        let rendered = drawn_dashboard(&mut aged, 200);
        assert!(rendered.contains("stale: sampled 1h ago"), "{rendered}");
    }

    #[test]
    fn selected_transcript_tail_adapts_to_a_constrained_terminal() {
        let mut session = stopped_session();
        session.state = SessionState::Running;
        let mut dashboard = dashboard_with_session(session);
        let message = (1..=20)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        apply_materialized_transcript(&mut dashboard, vec![agent_message(1, message)]);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");

        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw constrained dashboard");

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Sessions"));
        assert!(rendered.contains("Targets"));
        assert!(rendered.contains("Quota"));
    }

    #[test]
    fn overflowing_session_pane_shows_a_scrollbar() {
        let mut sessions = BTreeMap::new();
        for index in 0..6 {
            let mut session = running_session();
            session.id = format!("active-{index:02}");
            session.state = SessionState::Running;
            sessions.insert(session.id.clone(), session);
        }
        for index in 0..20 {
            let mut session = stopped_session();
            session.id = format!("archived-{index:02}");
            sessions.insert(session.id.clone(), session);
        }
        let state = State {
            version: STATE_VERSION,
            sessions,
            mount_history: BTreeMap::new(),
            container_sizes: BTreeMap::new(),
        };
        let mut dashboard = DashboardState::new(config(), state, BTreeMap::new());
        for index in 0..6 {
            apply_materialized_transcript_for(
                &mut dashboard,
                &format!("active-{index:02}"),
                vec![agent_message(1, "one\ntwo\nthree\nfour")],
            );
        }
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw dashboard");
        let symbols = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>();

        let thumb = symbols.iter().filter(|symbol| **symbol == "▐").count();
        let track = symbols.iter().filter(|symbol| **symbol == "│").count();
        assert!(thumb >= 1, "expected a scrollbar thumb, rendered {thumb}");
        assert!(track >= 1, "expected a scrollbar track, rendered {track}");
    }

    #[test]
    fn fully_visible_tables_do_not_show_scrollbars() {
        let mut dashboard = dashboard_with_session(stopped_session());
        let mut terminal = Terminal::new(TestBackend::new(120, 36)).expect("test terminal");

        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw fully visible tables");
        let symbols = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>();

        assert!(!symbols.contains(&"▐"));
    }

    #[test]
    fn overflowing_quota_pane_uses_the_shared_scrollbar() {
        let mut config = config();
        let profile = config.profiles["codex-1"].clone();
        for index in 0..20 {
            config
                .profiles
                .insert(format!("profile-{index:02}"), profile.clone());
        }
        let mut dashboard = DashboardState::new(config, State::default(), BTreeMap::new());
        dashboard.focus = Focus::Quota;
        let mut terminal = Terminal::new(TestBackend::new(120, 24)).expect("test terminal");

        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw overflowing quotas");
        let symbols = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>();

        assert!(symbols.contains(&"▐"));
        assert!(symbols.contains(&"│"));
    }

    #[test]
    fn active_checkpoint_age_uses_compact_seconds_minutes_hours_and_days() {
        let checkpointed_at = "2026-08-09T01:00:00Z";
        let base = chrono::DateTime::parse_from_rfc3339(checkpointed_at)
            .unwrap()
            .timestamp() as u64;

        assert_eq!(checkpoint_age(base + 12, checkpointed_at), "12s");
        assert_eq!(checkpoint_age(base + 8 * 60, checkpointed_at), "8m");
        assert_eq!(checkpoint_age(base + 3 * 3_600, checkpointed_at), "3h");
        assert_eq!(checkpoint_age(base + 2 * 86_400, checkpointed_at), "2d");
    }

    #[test]
    fn recovery_state_is_hidden_until_a_failure_needs_attention() {
        let mut session = stopped_session();
        session.state = SessionState::Running;
        assert_eq!(
            recovery_warning_name(&session, "Build Hel".into(), 0),
            "Build Hel"
        );

        session.last_checkpoint_error = Some("copy failed".into());
        session.checkpoint = None;
        assert_eq!(
            recovery_warning_name(&session, "Build Hel".into(), 0),
            "Build Hel  ⚠ Recovery unavailable"
        );
    }

    #[test]
    fn active_session_with_no_turn_in_flight_reads_idle() {
        let mut session = stopped_session();
        session.state = SessionState::Running;
        let detail = SessionDetail {
            last_activity_at_ms: Some(1_000_000),
            ..SessionDetail::default()
        };

        let text = session_metadata_text(&session, Some(&detail), None, 1_480, &config());
        assert!(text.contains("Idle"), "{text}");
    }

    #[test]
    fn provisioning_clock_uses_elapsed_seconds_since_state_update() {
        let mut session = stopped_session();
        session.state = SessionState::Provisioning;
        session.updated_at = "1970-01-01T00:16:40Z".into();

        let text = session_metadata_text(&session, None, None, 1_012, &config());
        assert!(text.contains("Launch 12s"), "{text}");
    }

    #[test]
    fn transition_row_is_compact_and_contains_stage_identity_and_elapsed() {
        let session = stopped_session();
        let operation = operation(SessionOperationKind::Moving, Some(ProvisionStage::Cloning));
        let line = session_transition_line(
            "› ",
            &session,
            mj_core::state::SessionTransitionKind::Moving,
            Some(&operation),
            1_012,
            "podman",
            120,
            &config(),
            None,
        );
        let text = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(text.contains("Moving"), "{text}");
        assert!(text.contains("Clone"), "{text}");
        assert!(text.contains("12s"), "{text}");
        assert!(text.contains("ACP pretty name"), "{text}");
        assert!(!text.contains("No messages"), "{text}");
    }

    #[test]
    fn launch_clock_names_the_reported_stage() {
        let session = stopped_session();
        let operation = operation(
            SessionOperationKind::Launching,
            Some(ProvisionStage::Booting),
        );

        let text = session_metadata_text(&session, None, Some(&operation), 1_012, &config());
        assert!(text.contains("Boot 12s"), "{text}");
    }

    #[test]
    fn launch_clock_falls_back_to_the_kind_label_without_a_stage() {
        let session = stopped_session();
        let operation = operation(SessionOperationKind::Launching, None);

        let text = session_metadata_text(&session, None, Some(&operation), 1_012, &config());
        assert!(text.contains("Launch 12s"), "{text}");
    }

    #[test]
    fn a_stage_does_not_rename_a_non_launch_operation() {
        let session = stopped_session();
        let operation = operation(
            SessionOperationKind::Stopping,
            Some(ProvisionStage::Syncing),
        );

        let text = session_metadata_text(&session, None, Some(&operation), 1_012, &config());
        assert!(text.contains("Stopping 12s"), "{text}");
    }

    #[test]
    fn resuming_row_shows_the_destination_profile_and_target_not_the_stale_record() {
        // The controller updates the session's own last_profile/target as
        // soon as a resume starts, but the dashboard's local session
        // snapshot only refreshes once the operation finishes. The in-flight
        // row must show where the resume is going, not where it came from.
        let session = stopped_session();
        assert_eq!(session.last_profile, "codex-1");
        assert_eq!(session.target_template_id, "podman");
        let mut resuming = operation(SessionOperationKind::Resuming, None);
        resuming.resume_destination = Some(("grok-1".into(), "localhost".into()));

        let text = session_metadata_text(&session, None, Some(&resuming), 1_012, &config());

        assert!(text.contains("grok-1"), "{text}");
        assert!(text.contains("localhost"), "{text}");
    }

    #[test]
    fn without_a_resume_destination_the_row_falls_back_to_the_session_record() {
        let session = stopped_session();
        let resuming = operation(SessionOperationKind::Resuming, None);

        let text = session_metadata_text(&session, None, Some(&resuming), 1_012, &config());

        assert!(text.contains(&session.last_profile), "{text}");
        assert!(text.contains(&session.target_template_id), "{text}");
    }

    #[test]
    fn stage_clock_counts_from_when_the_stage_began_not_the_operation() {
        let session = stopped_session();
        let mut operation = operation(
            SessionOperationKind::Launching,
            Some(ProvisionStage::Booting),
        );
        // The operation started at 1_000 but the stage only began at 1_040;
        // the clock must count from the stage, not the whole operation.
        operation
            .active_stages
            .insert(ProvisionStage::Booting, 1_040);

        let text = session_metadata_text(&session, None, Some(&operation), 1_052, &config());
        assert!(text.contains("Boot 12s"), "{text}");
    }

    #[test]
    fn install_stage_names_the_harness_not_the_profile() {
        let session = stopped_session();
        assert_eq!(session.last_profile, "codex-1");
        let operation = operation(
            SessionOperationKind::Launching,
            Some(ProvisionStage::Installing(HarnessKind::Codex)),
        );

        let text = session_metadata_text(&session, None, Some(&operation), 1_012, &config());

        assert!(text.contains("Installing Codex 12s"), "{text}");
        assert!(text.contains("codex-1"), "{text}");
    }

    #[test]
    fn launch_clock_names_concurrent_stages_in_lifecycle_order() {
        let session = stopped_session();
        let mut operation = operation(SessionOperationKind::Launching, None);
        operation
            .active_stages
            .insert(ProvisionStage::Syncing, 1_003);
        operation
            .active_stages
            .insert(ProvisionStage::Cloning, 1_002);

        let text = session_metadata_text(&session, None, Some(&operation), 1_012, &config());
        assert!(text.contains("Clone, Sync 10s"), "{text}");
    }

    #[test]
    fn focused_panes_use_accented_rounded_borders_without_focus_title_text() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.set_deployment_capacity_targets(vec![test_capacity_target()]);
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw dashboard");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("╭Sessions"));
        assert!(rendered.contains("Targets"));
        assert!(!rendered.contains("[focused]"));

        for (focus, rounded) in [(Focus::Quota, "╭ Quota"), (Focus::Targets, "╭ Targets")] {
            dashboard.focus = focus;
            terminal
                .draw(|frame| render(frame, &mut dashboard))
                .expect("draw dashboard");
            let rendered = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(rendered.contains(rounded), "{focus:?}: {rendered:?}");
            assert!(!rendered.contains("[focused]"));
            let pane_index = if focus == Focus::Quota { 2 } else { 1 };
            let area = dashboard.pane_areas.expect("pane areas")[pane_index];
            let border = &terminal.backend().buffer()[(area.x, area.y)];
            assert_eq!(border.fg, theme::palette().accent);
            assert!(border.modifier.contains(Modifier::BOLD));
        }
    }

    #[test]
    fn only_focused_pane_draws_caret_without_shifting_table_columns() {
        let mut first = stopped_session();
        first.id = "session-0".into();
        first.state = SessionState::Running;
        let mut second = stopped_session();
        second.state = SessionState::Running;
        let mut dashboard = DashboardState::new(
            config(),
            State {
                version: STATE_VERSION,
                sessions: BTreeMap::from([(first.id.clone(), first), (second.id.clone(), second)]),
                mount_history: BTreeMap::new(),
                container_sizes: BTreeMap::new(),
            },
            BTreeMap::new(),
        );
        dashboard.set_deployment_capacity_targets(vec![test_capacity_target()]);
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut initial_name_columns = None;

        for expected_focus in [Focus::Sessions, Focus::Targets, Focus::Quota] {
            dashboard.focus = expected_focus;
            terminal
                .draw(|frame| render(frame, &mut dashboard))
                .expect("draw dashboard");
            let buffer = terminal.backend().buffer();
            let lines = (buffer.area.y..buffer.area.bottom())
                .map(|y| {
                    (buffer.area.x..buffer.area.right())
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>();
            // The Sessions pane always marks the conversation on screen, and
            // a focused table marks its own row. Nothing else draws a caret,
            // so a support pane that does not have focus adds none.
            let carets = lines
                .iter()
                .flat_map(|line| line.chars())
                .filter(|character| *character == '›')
                .count();
            let expected_carets = usize::from(expected_focus != Focus::Sessions) + 1;
            assert_eq!(carets, expected_carets, "{expected_focus:?}");
            if expected_focus == Focus::Sessions {
                // Both sessions draw their expanded form, and the caret on one
                // of them does not shift the other's columns.
                let name_columns = lines
                    .iter()
                    .filter_map(|line| {
                        line.find("ACP pretty name")
                            .map(|byte| line[..byte].chars().count())
                    })
                    .collect::<Vec<_>>();
                assert_eq!(name_columns.len(), 2);
                assert_eq!(name_columns[0], name_columns[1]);
                initial_name_columns = Some(name_columns);
            }
        }
        assert!(initial_name_columns.is_some());
    }

    #[test]
    fn existing_sessions_remain_visible_when_setup_has_no_accounts_or_targets() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.set_config(Config::default());
        let rendered = drawn(&mut dashboard, 120, 40).join("\n");
        assert!(rendered.contains("Sessions"), "{rendered}");
        assert!(rendered.contains("ACP pretty name"), "{rendered}");
        assert!(dashboard.pane_areas.is_some());
        dashboard.focus_sessions();
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::None
        );
        assert!(matches!(dashboard.mode, Mode::Confirm(_)));
    }

    #[test]
    fn empty_config_renders_onboarding_with_the_workspace_name() {
        let mut dashboard =
            DashboardState::new(Config::default(), State::default(), BTreeMap::new());
        dashboard.set_workspace_name("personal".into());
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw dashboard");
        let buffer = terminal.backend().buffer();
        let rendered = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Mjolnir needs a little fuel."));
        assert!(rendered.contains("personal"));
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Char('e'))),
            DashboardAction::None
        );
    }

    #[test]
    fn workspace_name_does_not_change_with_dashboard_updates() {
        let mut dashboard = DashboardState::new(config(), State::default(), BTreeMap::new());
        dashboard.set_workspace_name("acme-workspace".into());
        dashboard.set_state(State::default());
        dashboard.set_quotas(BTreeMap::new());

        let mut terminal = Terminal::new(TestBackend::new(100, 24)).expect("terminal");
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw dashboard");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Sessions"));
        assert_eq!(dashboard.workspace_name, "acme-workspace");
    }

    #[test]
    fn quota_render_includes_errors_and_refresh_age_in_title() {
        let mut dashboard = DashboardState::new(
            config(),
            State::default(),
            BTreeMap::from([(
                "codex-1".into(),
                ProfileQuota {
                    profile_id: "codex-1".into(),
                    harness: HarnessKind::Codex,
                    windows: vec![],
                    extra: None,
                    error: Some("offline".into()),
                    refreshed_at_epoch_seconds: 1,
                },
            )]),
        );
        let backend = TestBackend::new(120, 28);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw dashboard");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("unavailable"));
        assert!(!rendered.contains("offline"));
        assert!(rendered.contains("Quota (refreshed"));
        assert!(!rendered.contains("Refreshed"));
        assert!(!rendered.contains("Access"));
        assert!(!rendered.contains("agent-full-access"));
    }

    #[test]
    fn quota_render_shows_login_expired_without_unavailable_prefix() {
        let mut dashboard = DashboardState::new(
            config(),
            State::default(),
            BTreeMap::from([(
                "claude-1".into(),
                ProfileQuota {
                    profile_id: "claude-1".into(),
                    harness: HarnessKind::Claude,
                    windows: vec![],
                    extra: None,
                    error: Some("login expired".into()),
                    refreshed_at_epoch_seconds: 1,
                },
            )]),
        );
        let backend = TestBackend::new(120, 28);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw dashboard");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("login expired"));
        assert!(!rendered.contains("unavailable: login expired"));
    }

    #[test]
    fn deepseek_quota_row_shows_api_without_bars_or_reset_dates() {
        let mut config = config();
        config.profiles.get_mut("codex-1").unwrap().kind = HarnessKind::Deepseek;
        let mut dashboard = DashboardState::new(config, State::default(), BTreeMap::new());
        dashboard.quota_refreshing.insert("codex-1".into());
        let mut terminal = Terminal::new(TestBackend::new(120, 28)).expect("terminal");
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw dashboard");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(rendered.contains("API"));
        assert!(rendered.contains("▕   API    ▏"));
        assert!(!rendered.contains("API Pricing"));
        assert!(rendered.contains("DSH"));
        assert!(!rendered.contains("DeepSeek Harness"));
        assert!(!rendered.contains("unavailable"));
        assert!(!rendered.contains('%'));
    }

    #[test]
    fn quota_bars_show_fractional_remaining_capacity_and_blank_missing_windows() {
        let window = QuotaWindow {
            label: "Week".into(),
            remaining_percent: Some(73),
            used: None,
            limit: None,
            resets: None,
            resets_at_epoch_seconds: None,
        };

        let bar = quota_bar(Some(&window));
        assert_eq!(
            bar.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            "███████▎  ▏73%"
        );
        assert_eq!(bar.spans[0].style.fg, Some(theme::palette().success));
        assert_eq!(bar.spans[2].style.fg, None);
        assert!(
            bar.spans[..4]
                .iter()
                .all(|span| span.style.bg == Some(theme::palette().background))
        );
        assert_eq!(bar.spans[3].style.fg, Some(theme::palette().muted));
        let chart = quota_chart(bar, true);
        assert_eq!(chart.to_string(), "▕███████▎  ▏73%");
        assert_eq!(chart.spans[0].style, quota_chart_border_style());
        assert!(quota_bar(None).spans.is_empty());
    }

    #[test]
    fn api_quota_label_uses_the_black_bordered_chart_field() {
        let api = api_quota_bar();
        let rendered: String = api.spans.iter().map(|span| span.content.as_ref()).collect();
        assert_eq!(rendered, "   API    ▏");
        assert!(
            api.spans
                .iter()
                .all(|span| span.style.bg == Some(theme::palette().background))
        );
        assert_eq!(api.spans[3].style.fg, Some(theme::palette().muted));
    }

    #[test]
    fn quota_render_hides_five_hour_bar_and_reset_when_weekly_quota_is_exhausted() {
        let quota = ProfileQuota {
            profile_id: "codex-1".into(),
            harness: HarnessKind::Codex,
            windows: vec![
                QuotaWindow {
                    label: "Week".into(),
                    remaining_percent: Some(0),
                    used: None,
                    limit: None,
                    resets: Some("09:00 Aug 20".into()),
                    resets_at_epoch_seconds: Some(604_800),
                },
                QuotaWindow {
                    label: "5H".into(),
                    remaining_percent: Some(70),
                    used: None,
                    limit: None,
                    resets: Some("14:00 Aug 13".into()),
                    resets_at_epoch_seconds: Some(14_400),
                },
            ],
            extra: None,
            error: None,
            refreshed_at_epoch_seconds: 0,
        };
        let mut dashboard = DashboardState::new(
            config(),
            State::default(),
            BTreeMap::from([("codex-1".into(), quota)]),
        );
        let mut terminal = Terminal::new(TestBackend::new(140, 28)).expect("terminal");
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw dashboard");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(rendered.contains("0%"));
        assert!(!rendered.contains("70%"));
        assert!(!rendered.contains("4h"));
    }

    #[test]
    fn quota_reset_countdowns_use_a_second_unit_only_after_one_first_unit() {
        const MINUTE: u64 = 60;
        const HOUR: u64 = 60 * MINUTE;
        const DAY: u64 = 24 * HOUR;
        let now = 100;

        assert_eq!(
            quota_reset_countdown(now, (now + 2 * DAY + 5 * HOUR) as i64),
            "2d"
        );
        assert_eq!(
            quota_reset_countdown(now, (now + DAY + 5 * HOUR) as i64),
            "1d5h"
        );
        assert_eq!(
            quota_reset_countdown(now, (now + 2 * HOUR + 5 * MINUTE) as i64),
            "2h"
        );
        assert_eq!(
            quota_reset_countdown(now, (now + HOUR + 5 * MINUTE) as i64),
            "1h5m"
        );
        assert_eq!(
            quota_reset_countdown(now, (now + 35 * MINUTE) as i64),
            "35m"
        );
        assert_eq!(quota_reset_countdown(now, (now + 30) as i64), "<1m");
        assert_eq!(quota_reset_countdown(now, now as i64), "now");
    }

    #[test]
    fn weekly_and_five_hour_resets_are_independent() {
        let quota = ProfileQuota {
            profile_id: "codex-1".into(),
            harness: HarnessKind::Codex,
            windows: vec![
                QuotaWindow {
                    label: "Week".into(),
                    remaining_percent: Some(73),
                    used: None,
                    limit: None,
                    resets: Some("09:00 Aug 20".into()),
                    resets_at_epoch_seconds: Some(604_800),
                },
                QuotaWindow {
                    label: "5H".into(),
                    remaining_percent: Some(80),
                    used: None,
                    limit: None,
                    resets: Some("14:00 Aug 13".into()),
                    resets_at_epoch_seconds: Some(14_400),
                },
            ],
            extra: None,
            error: None,
            refreshed_at_epoch_seconds: 0,
        };

        assert_eq!(quota_reset_cells(&quota, 0), ("7d".into(), "4h0m".into()));
    }

    #[test]
    fn five_hour_reset_always_uses_minutes_above_one_hour() {
        const MINUTE: i64 = 60;
        const HOUR: i64 = 60 * MINUTE;

        assert_eq!(
            five_hour_quota_reset_countdown(100, 100 + 4 * HOUR + 50 * MINUTE),
            "4h50m"
        );
        assert_eq!(
            five_hour_quota_reset_countdown(100, 100 + 4 * HOUR + 5 * MINUTE),
            "4h5m"
        );
        assert_eq!(
            five_hour_quota_reset_countdown(100, 100 + HOUR + 5 * MINUTE),
            "1h5m"
        );
        assert_eq!(five_hour_quota_reset_countdown(100, 130), "<1m");
    }

    #[test]
    fn quota_render_uses_weekly_five_hour_and_reset_columns() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let now = i64::try_from(now).unwrap();
        let quota = ProfileQuota {
            profile_id: "codex-1".into(),
            harness: HarnessKind::Codex,
            windows: vec![
                QuotaWindow {
                    label: "Week".into(),
                    remaining_percent: Some(73),
                    used: None,
                    limit: None,
                    resets: Some("09:00 Aug 20".into()),
                    resets_at_epoch_seconds: Some(now + 2 * 24 * 60 * 60 + 30),
                },
                QuotaWindow {
                    label: "5H".into(),
                    remaining_percent: Some(70),
                    used: None,
                    limit: None,
                    resets: Some("14:00 Aug 13".into()),
                    resets_at_epoch_seconds: Some(now + 60 * 60 + 5 * 60 + 30),
                },
            ],
            extra: None,
            error: None,
            refreshed_at_epoch_seconds: 0,
        };
        let mut dashboard = DashboardState::new(
            config(),
            State::default(),
            BTreeMap::from([("codex-1".into(), quota)]),
        );
        let mut terminal = Terminal::new(TestBackend::new(140, 28)).expect("terminal");
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw dashboard");
        let lines = buffer_lines(terminal.backend().buffer());
        let rendered = lines.join("\n");

        assert!(rendered.contains("Weekly"));
        assert!(rendered.contains("5H"));
        assert_eq!(rendered.matches("Resets").count(), 2);
        assert!(rendered.contains("73%"));
        assert!(rendered.contains("70%"));
        assert!(rendered.contains("2d"));
        assert!(rendered.contains("1h5m"));
        assert!(!rendered.contains("09:00 Aug 20"));

        let row = lines
            .iter()
            .find(|line| line.contains("codex-1"))
            .expect("quota row");
        assert!(row.contains("▕███████▎  ▏73%"), "{row:?}");
        assert!(row.contains("▕███████   ▏70%"), "{row:?}");
        let weekly_percent = cell_column(row, "73%");
        let weekly_reset = cell_column(row, "2d");
        let five_hour_percent = cell_column(row, "70%");
        let five_hour_reset = cell_column(row, "1h5m");
        assert_eq!(weekly_reset, weekly_percent + 3 + 1);
        assert_eq!(five_hour_percent - 12, weekly_reset + 6 + 2);
        assert_eq!(five_hour_reset, five_hour_percent + 3 + 1);
    }

    #[test]
    fn quota_render_keeps_both_percentages_and_resets_at_eighty_columns() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let now = i64::try_from(now).unwrap();
        let quota = ProfileQuota {
            profile_id: "codex-1".into(),
            harness: HarnessKind::Codex,
            windows: vec![
                QuotaWindow {
                    label: "Week".into(),
                    remaining_percent: Some(73),
                    used: None,
                    limit: None,
                    resets: None,
                    resets_at_epoch_seconds: Some(now + 2 * 24 * 60 * 60 + 30),
                },
                QuotaWindow {
                    label: "5H".into(),
                    remaining_percent: Some(70),
                    used: None,
                    limit: None,
                    resets: None,
                    resets_at_epoch_seconds: Some(now + 60 * 60 + 5 * 60 + 30),
                },
            ],
            extra: None,
            error: None,
            refreshed_at_epoch_seconds: 0,
        };
        let mut dashboard = DashboardState::new(
            config(),
            State::default(),
            BTreeMap::from([("codex-1".into(), quota)]),
        );
        let mut terminal = Terminal::new(TestBackend::new(80, 28)).expect("terminal");
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw dashboard");
        let row = buffer_lines(terminal.backend().buffer())
            .into_iter()
            .find(|line| line.contains("codex-1"))
            .expect("quota row");
        assert!(row.contains("73%"), "{row:?}");
        assert!(row.contains("70%"), "{row:?}");
        assert!(row.contains("2d"), "{row:?}");
        assert!(row.contains("1h5m"), "{row:?}");
    }
}
