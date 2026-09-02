//! Dashboard rendering: pane layout, session tables, capacity, quotas, footer.
use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Clear, HighlightSpacing, Paragraph, Row, Scrollbar,
    ScrollbarOrientation, ScrollbarState, Table, TableState, Wrap,
};

use hel::hel_chat::render_agent_message_head;
#[cfg(test)]
use hel::hel_chat::render_agent_message_tail;
use hel::hel_config::{HarnessKind, HelConfig, PermissionMode};
use hel::hel_quota::{ProfileQuota, QuotaWindow};
use hel::hel_state::{SessionRecord, SessionState};
use hel::hel_targets::DeploymentCapacityKind;

use crate::dialogs::{
    render_config_id_editor, render_confirmation, render_container_editor,
    render_import_bundle_confirmation, render_import_progress, render_rename_editor,
    render_repository_origin, render_session_edit, render_target_actions, render_web_dialog,
};
use crate::ingest::{CapacityDetail, SessionDetail, SessionOperationDisplay};
use crate::resume::render_resume_dialog;
use crate::widgets::{focus_border, format_resource_bytes};
use crate::wizards::{render_new_wizard, render_resume_wizard};
use crate::{DashboardState, Focus, Mode, SessionOperationKind, SessionsRow};

#[cfg(test)]
const ACTIVE_MESSAGE_LINES: usize = 4;

const SESSION_TABLE_CHROME_HEIGHT: u16 = 3;

#[cfg(test)]
pub(crate) const SUMMARY_RULE: &str = "─";

/// Draws the first-run screen: there is no conversation and no session list
/// yet, so the surface explains how to get one and shows the support panes
/// under it.
pub(crate) fn render_onboarding_surface(frame: &mut Frame, dashboard: &mut DashboardState) {
    let area = frame.area();
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(8),
            Constraint::Length(5),
            Constraint::Length(8),
            Constraint::Length(1),
        ])
        .split(area);
    render_dashboard_title(frame, layout[0], &dashboard.workspace_name);

    render_onboarding(frame, layout[1], dashboard);
    render_capacity(frame, layout[2], dashboard);
    render_quotas(frame, layout[3], dashboard);
    render_footer(frame, layout[4], dashboard);
    render_modal(frame, area, dashboard);
}

/// Draws the active modal over the dashboard already on the frame. Each modal
/// clears its own centered rect, so the panes stay visible around it.
///
/// The registry moves out for the call because the modal renderers read the
/// rest of the dashboard while they register their own surfaces.
pub(crate) fn render_modal(frame: &mut Frame, area: Rect, dashboard: &mut DashboardState) {
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
        Mode::SessionEdit(dialog) => render_session_edit(frame, area, dialog, &mut surfaces),
        Mode::ConfigId(editor) => render_config_id_editor(frame, area, editor, &mut surfaces),
        Mode::TargetActions(dialog) => {
            render_target_actions(frame, area, dashboard, dialog, &mut surfaces)
        }
        Mode::Web(dialog) => render_web_dialog(frame, area, dialog, &mut surfaces),
        Mode::Rename(editor) => render_rename_editor(frame, area, editor, &mut surfaces),
        Mode::EditContainer(editor) => render_container_editor(frame, area, editor, &mut surfaces),
        Mode::Importing(progress) => render_import_progress(frame, area, progress, &mut surfaces),
        Mode::ConfirmImportBundle(confirmation) => {
            render_import_bundle_confirmation(frame, area, confirmation, &mut surfaces)
        }
        Mode::Confirm(dialog) => render_confirmation(frame, area, dialog, &mut surfaces),
        Mode::Dashboard => {}
    }
    dashboard.frame_surfaces = surfaces;
}

fn render_dashboard_title(frame: &mut Frame, area: Rect, workspace_name: &str) {
    frame.render_widget(
        Paragraph::new(Span::styled(
            workspace_name,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ))
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

pub(crate) const MINIMUM_TERMINAL_WIDTH: u16 = 32;

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
        (dashboard.config.profiles.is_empty(), "a harness profile"),
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
            Line::raw("Press Ctrl+E to run setup, or edit Mjolnir's TOML configuration by hand."),
        ])
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: true })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Get started "),
        ),
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

/// The Sessions pane's title.
///
/// The Turn and Step legend explains the prefixes on the expanded rows' agent
/// lines, so it only appears while the pane is drawing those rows, and only
/// while the pane is wide enough to spell it out.
pub(crate) fn sessions_pane_title(width: u16, expanded: bool) -> &'static str {
    const FULL: &str = " Sessions · Turn=time since prompt · Step=time since agent activity ";
    const MEDIUM: &str = " Sessions · Turn=prompt age · Step=agent silence ";
    const COMPACT: &str = " Sessions Turn=prompt Step=silence ";

    if !expanded {
        return " Sessions ";
    }
    let available = usize::from(width.saturating_sub(2));
    if available >= FULL.chars().count() {
        FULL
    } else if available >= MEDIUM.chars().count() {
        MEDIUM
    } else if available >= COMPACT.chars().count() {
        COMPACT
    } else {
        // Narrower than the shortest legend. The rows label their own clocks,
        // so the pane keeps its name and drops the key.
        " Sessions "
    }
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

impl DrawnSessionRow {
    fn content_height(&self) -> u16 {
        u16::try_from(self.lines.len()).unwrap_or(u16::MAX)
    }
}

/// Lays the pane out without drawing it, so the layout can ask how tall it
/// wants to be before it has any rows to give it.
fn drawn_session_rows(dashboard: &DashboardState, width: u16) -> Vec<DrawnSessionRow> {
    let now_epoch_seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let sessions = dashboard.ordered_sessions();
    let targets = session_display_targets(dashboard, &sessions);
    let mut rows: Vec<DrawnSessionRow> = Vec::new();
    let mut pending_heading: Option<(String, Line<'static>)> = None;
    for row in dashboard.sessions_rows() {
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
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                ));
            }
            SessionsRow::Session { index, expanded } => {
                let Some(session) = sessions.get(index) else {
                    continue;
                };
                let detail = dashboard.session_details.get(&session.id);
                let unreachable = dashboard.unreachable_sessions.contains(&session.id);
                let facts = SessionRowFacts {
                    detail,
                    unreachable,
                    state: session.state,
                    now_epoch_seconds,
                };
                let operation = dashboard.session_operations.get(&session.id);
                let target = targets.get(index).cloned().unwrap_or_default();
                let permission = session_permission_badge(session, operation, &dashboard.config);
                // The selection drives which conversation is on screen, so
                // the caret marks it in both forms.
                let selected =
                    dashboard.selected_session_id.as_deref() == Some(session.id.as_str());
                let prefix = if selected { "› " } else { "  " };
                let (heading_key, heading_line) = match pending_heading.take() {
                    Some((key, line)) => (Some(key), Some(line)),
                    None => (None, None),
                };
                let mut lines = Vec::new();
                lines.extend(heading_line);
                // The minimized layout draws its own grid (see
                // `render_sessions_grid`) and never routes through here, so
                // this laydown only handles the expanded and support-collapsed
                // positions: four lines per session, or one.
                if expanded {
                    expanded_session_lines(
                        &mut lines,
                        dashboard,
                        session,
                        detail,
                        unreachable,
                        operation,
                        now_epoch_seconds,
                        &target,
                        permission,
                        width,
                    );
                } else {
                    lines.push(collapsed_session_line(
                        prefix,
                        &target,
                        facts,
                        usize::from(width.saturating_sub(4)),
                        permission,
                    ));
                }
                let spacing = u16::from(expanded);
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

/// The four rows an expanded session draws: status and identity, the user's
/// last message, and two rows of agent activity. The agent block is always
/// two rows, even with nothing to say, so every expanded session is the same
/// height and the layout can be computed from a count.
#[allow(clippy::too_many_arguments)]
fn expanded_session_lines(
    lines: &mut Vec<Line<'static>>,
    dashboard: &DashboardState,
    session: &SessionRecord,
    detail: Option<&SessionDetail>,
    unreachable: bool,
    operation: Option<&SessionOperationDisplay>,
    now_epoch_seconds: u64,
    target: &str,
    permission: Option<Span<'static>>,
    width: u16,
) {
    let selected = dashboard.selected_session_id.as_deref() == Some(session.id.as_str());
    let prefix = if selected { "› " } else { "  " };
    lines.push(session_top_line(
        prefix,
        session,
        detail,
        unreachable,
        operation,
        now_epoch_seconds,
        target,
        permission,
    ));
    lines.push(prefixed_summary_line(
        "  ",
        "You: ",
        detail.and_then(|detail| detail.last_user_message.as_deref()),
        usize::from(width.saturating_sub(4)),
        detail.is_some_and(|detail| detail.last_agent_message_follows_last_user),
    ));
    let agent_excerpt = detail.and_then(|detail| {
        if detail.last_user_message.is_none() || detail.last_agent_message_follows_last_user {
            detail.last_agent_message.as_deref()
        } else {
            detail.latest_agent_activity_after_last_user.as_deref()
        }
    });
    let prefixes = dashboard_agent_prefixes(now_epoch_seconds, detail);
    let prefix_width = prefixes.iter().map(String::len).max().unwrap_or_default();
    let mut agent = agent_excerpt
        .map(|message| {
            render_agent_message_head(
                message,
                usize::from(
                    width.saturating_sub(u16::try_from(prefix_width + 5).unwrap_or(u16::MAX)),
                ),
                2,
            )
        })
        .unwrap_or_default();
    if agent.is_empty() {
        agent.push(Line::raw("No messages yet"));
    }
    agent.resize(2, Line::default());
    for (agent_index, mut line) in agent.into_iter().take(2).enumerate() {
        let mut spans = vec![Span::raw("  ")];
        spans.push(Span::styled(
            format!("{} ", prefixes[agent_index]),
            Style::default().add_modifier(Modifier::BOLD),
        ));
        spans.append(&mut line.spans);
        lines.push(Line::from(spans));
    }
}

/// The one-line form the pane uses while the keyboard is elsewhere: which
/// project, which target, how long the turn has been running, and what the
/// agent last said.
/// What a one-line session row needs to know about the session behind it.
///
/// Both one-line forms derive the same three things from it - the turn clock,
/// the last thing the agent said, and the colour the row carries - so they
/// derive them in one place.
#[derive(Clone, Copy)]
struct SessionRowFacts<'a> {
    detail: Option<&'a SessionDetail>,
    unreachable: bool,
    state: SessionState,
    now_epoch_seconds: u64,
}

impl SessionRowFacts<'_> {
    fn style(&self) -> Style {
        Style::default().fg(session_band_color(
            self.detail,
            self.unreachable,
            self.state,
        ))
    }

    fn clock(&self) -> String {
        let activity = self.detail.map(|detail| &detail.activity);
        hel::usage_format::format_activity_clock(
            self.now_epoch_seconds,
            self.detail
                .and_then(|detail| detail.current_turn_started_at),
            activity.unwrap_or(&*EMPTY_ACTIVITY),
        )
    }

    /// The last non-empty line the agent said, or why there is none.
    fn last_agent_line(&self) -> &str {
        self.detail
            .and_then(|detail| detail.last_agent_message.as_deref())
            .and_then(|message| message.lines().rev().find(|line| !line.trim().is_empty()))
            .unwrap_or("No messages yet")
            .trim()
    }
}

/// The target label shown for each session, in `ordered_sessions()` order.
///
/// A target repeated inside one project is ambiguous on its own, so repeats
/// are numbered `[1]`, `[2]`, … in the order they appear. Both the row
/// laydown and the minimized grid read from this so they agree on labels.
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
}

/// The Sessions pane's bordered block: the legend as its left title and, at
/// the right of the bar, the workspace name in dim text — truncated to the
/// room the legend leaves, and omitted when it does not fit or is empty.
fn sessions_block<'a>(
    focused: bool,
    legend: &'a str,
    workspace_name: &str,
    width: u16,
) -> Block<'a> {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(focus_border(focused))
        .title(Line::from(Span::raw(legend)));
    let room = usize::from(width).saturating_sub(Span::raw(legend).width() + 4);
    if room > 8 && !workspace_name.is_empty() {
        block.title(
            Line::from(Span::styled(
                format!(" {} ", crate::widgets::truncate_text(workspace_name, room)),
                Style::default().fg(Color::DarkGray),
            ))
            .right_aligned(),
        )
    } else {
        block
    }
}

/// Draws the Sessions pane and reports the per-row mouse hitboxes.
pub(crate) fn render_sessions(
    frame: &mut Frame,
    area: Rect,
    dashboard: &DashboardState,
) -> SessionRowsRendered {
    if dashboard.sessions_compact() {
        return render_sessions_grid(frame, area, dashboard);
    }
    let drawn = drawn_session_rows(dashboard, area.width);
    let focused = dashboard.focus() == Focus::Sessions;
    let expanded = !dashboard.sessions_compact();
    let legend = sessions_pane_title(area.width, expanded);
    let block = sessions_block(focused, legend, &dashboard.workspace_name, area.width);
    let table = Table::new(
        drawn.iter().map(|row| {
            Row::new([Cell::from(Text::from(row.lines.clone()))])
                .height(row.content_height())
                .bottom_margin(row.spacing)
        }),
        [Constraint::Min(1)],
    )
    .block(block);
    let mut state = TableState::default()
        .with_offset(dashboard.sessions_scroll.get())
        .with_selected(
            dashboard
                .selected_visible_index()
                .filter(|index| *index < drawn.len()),
        );
    frame.render_stateful_widget(table, area, &mut state);
    // The table scrolled only as far as it had to; remember where it settled
    // so the next frame does not scroll back to the top.
    dashboard.sessions_scroll.set(state.offset());

    let offset = state.offset();
    let mut row_y = area.y + 1;
    let mut visible = 0;
    let mut session_row_areas = Vec::new();
    let mut project_heading_areas = Vec::new();
    for row in drawn.iter().skip(offset) {
        if row_y >= area.bottom().saturating_sub(1) {
            break;
        }
        visible += 1;
        let heading_rows = u16::from(row.heading.is_some());
        if let Some(key) = row.heading.clone() {
            project_heading_areas.push((
                key,
                Rect::new(area.x + 1, row_y, area.width.saturating_sub(2), 1),
            ));
        }
        if let Some(index) = row.session {
            let session_y = row_y.saturating_add(heading_rows);
            let height = row.content_height().saturating_sub(heading_rows);
            session_row_areas.push((
                index,
                Rect::new(
                    area.x.saturating_add(1),
                    session_y,
                    area.width.saturating_sub(2),
                    height.min(area.bottom().saturating_sub(1).saturating_sub(session_y)),
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

/// One cell of the minimized Sessions grid: a project heading or a session.
enum GridCell {
    Heading(String),
    Session { index: usize },
}

/// The minimized Sessions pane: a compact grid that shows every session.
///
/// Three equal columns, filled column by column (top to bottom, then
/// rightward), with each project's heading appearing inline above its
/// sessions. The grid is a viewport over the whole session flow; when the
/// selection sits past the visible columns the window scrolls to keep it on
/// screen. Each session cell shows its target — coloured by the same
/// state-based rule the expanded rows use — and its turn clock, or `[idle]`,
/// with the target ellipsized so the clock always fits.
fn render_sessions_grid(
    frame: &mut Frame,
    area: Rect,
    dashboard: &DashboardState,
) -> SessionRowsRendered {
    const COLUMNS: usize = 3;
    const GAP: u16 = 2;

    let now_epoch_seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let sessions = dashboard.ordered_sessions();
    let targets = session_display_targets(dashboard, &sessions);
    let selected_index = dashboard
        .selected_session_id()
        .and_then(|id| sessions.iter().position(|session| session.id == id));

    // The flow of cells the columns are packed from, headings inline.
    let mut cells: Vec<GridCell> = Vec::new();
    let mut selected_flow_pos = None;
    for row in dashboard.sessions_rows() {
        match row {
            SessionsRow::ProjectHeading { label, .. } => cells.push(GridCell::Heading(label)),
            SessionsRow::Session { index, .. } => {
                if Some(index) == selected_index {
                    selected_flow_pos = Some(cells.len());
                }
                cells.push(GridCell::Session { index });
            }
        }
    }

    // A tiny terminal sheds the pane's title and border so every row goes to
    // sessions; a taller one keeps them.
    let block = if crate::combined::minimized_grid_bordered(frame.area().height) {
        let focused = dashboard.focus() == Focus::Sessions;
        let legend = sessions_pane_title(area.width, false);
        sessions_block(focused, legend, &dashboard.workspace_name, area.width)
    } else {
        Block::default().borders(Borders::NONE)
    };
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut session_row_areas = Vec::new();
    let grid_rows = inner.height as usize;
    if grid_rows == 0 || inner.width == 0 {
        return SessionRowsRendered {
            session_row_areas,
            project_heading_areas: Vec::new(),
        };
    }

    // Column widths: equal share of what is left after the gaps, remainder
    // handed to the leftmost columns so the row still spans the full width.
    let gaps = GAP * (COLUMNS as u16 - 1);
    let available = inner.width.saturating_sub(gaps);
    let base = available / COLUMNS as u16;
    let extra = available % COLUMNS as u16;
    let column_widths: Vec<u16> = (0..COLUMNS)
        .map(|column| base + u16::from((column as u16) < extra))
        .collect();

    // Viewport: scroll so the selected session's column stays visible. The
    // offset is derived from the selection alone, so it needs no stored state.
    let total_columns = cells.len().div_ceil(grid_rows).max(1);
    let max_offset = total_columns.saturating_sub(COLUMNS);
    let column_offset = selected_flow_pos
        .map(|position| (position / grid_rows).saturating_sub(COLUMNS - 1))
        .unwrap_or(0)
        .min(max_offset);

    // When the viewport does not show every session, the last visible cell
    // becomes a dim "+N more" so it is clear the grid scrolls to reach the
    // rest. It steps aside if the last cell holds the selection, so the marker
    // never buries the session the user is on.
    let total_sessions = cells
        .iter()
        .filter(|cell| matches!(cell, GridCell::Session { .. }))
        .count();
    let viewport_start = column_offset * grid_rows;
    let viewport_end = ((column_offset + COLUMNS) * grid_rows).min(cells.len());
    let sessions_shown = cells
        .get(viewport_start..viewport_end)
        .map(|slots| {
            slots
                .iter()
                .filter(|cell| matches!(cell, GridCell::Session { .. }))
                .count()
        })
        .unwrap_or(0);
    let more_marker = (sessions_shown < total_sessions && viewport_end > viewport_start)
        .then_some(viewport_end - 1)
        .filter(|&last| {
            !matches!(
                cells.get(last),
                Some(GridCell::Session { index }) if Some(*index) == selected_index
            )
        });

    let mut column_x = inner.x;
    for (visible_column, &column_width) in column_widths.iter().enumerate() {
        if column_width == 0 {
            continue;
        }
        let source_column = column_offset + visible_column;
        for grid_row in 0..grid_rows {
            let flow_position = source_column * grid_rows + grid_row;
            let Some(cell) = cells.get(flow_position) else {
                continue;
            };
            let y = inner.y + grid_row as u16;
            let rect = Rect::new(column_x, y, column_width, 1);
            if more_marker == Some(flow_position) {
                let hidden = total_sessions - sessions_shown
                    + usize::from(matches!(cell, GridCell::Session { .. }));
                frame.render_widget(
                    Paragraph::new(Line::styled(
                        crate::widgets::truncate_text(
                            &format!("+{hidden} more"),
                            column_width as usize,
                        ),
                        Style::default().fg(Color::DarkGray),
                    )),
                    rect,
                );
                continue;
            }
            let line = match cell {
                GridCell::Heading(label) => Line::styled(
                    crate::widgets::truncate_text(label, column_width as usize),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
                GridCell::Session { index } => {
                    let Some(session) = sessions.get(*index) else {
                        continue;
                    };
                    let detail = dashboard.session_details.get(&session.id);
                    let facts = SessionRowFacts {
                        detail,
                        unreachable: dashboard.unreachable_sessions.contains(&session.id),
                        state: session.state,
                        now_epoch_seconds,
                    };
                    let clock = facts.clock();
                    let prefix = if Some(*index) == selected_index {
                        "› "
                    } else {
                        "  "
                    };
                    // Reserve the prefix, a separating space, and the clock;
                    // the target takes whatever room is left and ellipsizes.
                    let reserved = prefix.chars().count() + 1 + clock.chars().count();
                    let target_room = (column_width as usize).saturating_sub(reserved);
                    let target = targets.get(*index).cloned().unwrap_or_default();
                    let target = crate::widgets::truncate_text(&target, target_room);
                    session_row_areas.push((*index, rect));
                    // Right-justify the clock at the column's edge: pad between
                    // the target and the clock so the clocks line up in a
                    // column instead of trailing each target.
                    let used =
                        prefix.chars().count() + target.chars().count() + clock.chars().count();
                    let gap = (column_width as usize).saturating_sub(used).max(1);
                    Line::styled(format!("{prefix}{target}{:gap$}{clock}", ""), facts.style())
                }
            };
            frame.render_widget(Paragraph::new(line), rect);
        }
        column_x = column_x.saturating_add(column_width).saturating_add(GAP);
    }

    SessionRowsRendered {
        session_row_areas,
        // The grid has no per-project collapse, so its headings are not
        // clickable and report no hitboxes.
        project_heading_areas: Vec::new(),
    }
}

fn collapsed_session_line(
    prefix: &str,
    target: &str,
    facts: SessionRowFacts<'_>,
    width: usize,
    permission: Option<Span<'static>>,
) -> Line<'static> {
    let clock = facts.clock();
    let fragment = facts.last_agent_line();
    let style = facts.style();
    let mut lead_width = prefix.chars().count() + target.chars().count() + 2;
    let mut spans = vec![Span::styled(format!("{prefix}{target}"), style)];
    if let Some(permission) = permission {
        spans.push(Span::styled("  ", style));
        lead_width += permission.width() + 2;
        spans.push(permission);
    }
    spans.push(Span::styled("  ", style));
    lead_width += clock.chars().count() + 1;
    spans.push(Span::styled(
        format!(
            "{clock} {}",
            crate::widgets::truncate_text(fragment, width.saturating_sub(lead_width))
        ),
        style,
    ));
    Line::from(spans).style(style)
}

#[allow(clippy::too_many_arguments)]
fn session_top_line(
    prefix: &str,
    session: &SessionRecord,
    detail: Option<&SessionDetail>,
    unreachable: bool,
    operation: Option<&SessionOperationDisplay>,
    now_epoch_seconds: u64,
    target: &str,
    permission: Option<Span<'static>>,
) -> Line<'static> {
    let (profile, _) = operation
        .and_then(|operation| operation.resume_destination.clone())
        .unwrap_or_else(|| {
            (
                session.last_profile.clone(),
                session.target_template_id.clone(),
            )
        });
    let status_columns = if let Some(operation) = operation {
        let (label, started_at) = operation_status(operation);
        Some(vec![format!(
            "{label} {}",
            hel::usage_format::format_clock(now_epoch_seconds.saturating_sub(started_at))
        )])
    } else if session.state == SessionState::Provisioning {
        let started_at = session_updated_at_epoch_seconds(session).unwrap_or(now_epoch_seconds);
        Some(vec![format!(
            "Launch {}",
            hel::usage_format::format_clock(now_epoch_seconds.saturating_sub(started_at))
        )])
    } else {
        None
    };
    let queued_prompts = detail.map_or(0, |detail| detail.queued_prompts.len());
    let mut columns = vec![target.to_owned()];
    if queued_prompts > 0 {
        columns.push(format!("[Q {queued_prompts}]"));
    }
    let summary = if let Some(status_columns) = status_columns {
        columns.extend(status_columns);
        columns.push(profile.clone());
        columns.join("  ")
    } else {
        columns.push(profile.clone());
        columns.join("  ")
    };
    let session_name =
        recovery_warning_name(session, session_name(session).to_owned(), now_epoch_seconds);
    let summary_tail = summary
        .strip_prefix(target)
        .expect("session summary starts with its target");
    let style = Style::default().fg(session_band_color(detail, unreachable, session.state));
    let mut spans = vec![Span::styled(format!("{prefix}{target}"), style)];
    if let Some(permission) = permission {
        spans.push(Span::styled("  ", style));
        spans.push(permission);
    }
    spans.push(Span::styled(
        format!("{summary_tail}  {session_name}"),
        style,
    ));
    Line::from(spans).style(style)
}

const DASHBOARD_CLOCK_WIDTH: usize = 6;

/// A session the dashboard has heard nothing operational about yet.
static EMPTY_ACTIVITY: std::sync::LazyLock<hel::usage_format::SessionActivity> =
    std::sync::LazyLock::new(hel::usage_format::SessionActivity::default);

/// The two column heads an expanded row puts in front of the agent's last
/// lines: the turn and its step while a turn runs, the background work the
/// agent left behind while it is idle, and otherwise the time it last spoke.
fn dashboard_agent_prefixes(now_epoch_seconds: u64, detail: Option<&SessionDetail>) -> [String; 2] {
    let last_spoke = || {
        let time = detail
            .and_then(|detail| detail.last_activity_at_ms)
            .and_then(|value| i64::try_from(value).ok())
            .and_then(|value| hel::hel_chat::format_event_time(Some(value)))
            .unwrap_or_default();
        format!("{time:<6}")
    };
    let columns = hel::usage_format::format_activity_columns(
        now_epoch_seconds,
        detail.and_then(|detail| detail.current_turn_started_at),
        detail.and_then(|detail| detail.last_acp_activity_at_ms),
        detail.map_or(&*EMPTY_ACTIVITY, |detail| &detail.activity),
    );
    match columns.as_slice() {
        [turn, step] => [pad_dashboard_column(turn), pad_dashboard_column(step)],
        // Background work takes the turn column and leaves the step column to
        // the time the agent last spoke; `[idle]` says nothing worth a column.
        [background] if background.trim() != "[idle]" => {
            [pad_dashboard_column(background), last_spoke()]
        }
        _ => ["Agent:".into(), last_spoke()],
    }
}

/// Right-align one clock column's value so the clocks line up between rows.
fn pad_dashboard_column(column: &str) -> String {
    match column.rsplit_once(' ') {
        Some((label, clock)) => format!("{label} {clock:>DASHBOARD_CLOCK_WIDTH$}"),
        None => column.to_owned(),
    }
}

fn session_target_label(
    session: &SessionRecord,
    operation: Option<&SessionOperationDisplay>,
    config: &HelConfig,
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
    config: &HelConfig,
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
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        PermissionMode::Yolo => Span::styled(
            "[Y]",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
    })
}

fn capacity_target_labels(target_ids: &[String], config: &HelConfig) -> Line<'static> {
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

fn prefixed_summary_line(
    prefix: &str,
    label: &str,
    message: Option<&str>,
    width: usize,
    muted: bool,
) -> Line<'static> {
    let message = message.unwrap_or("No messages yet");
    let flattened = message.split_whitespace().collect::<Vec<_>>().join(" ");
    let lead = format!("{prefix}{label}");
    let line = Line::from(vec![
        Span::raw(prefix.to_owned()),
        Span::styled(
            label.to_owned(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(crate::widgets::truncate_text(
            &flattened,
            width.saturating_sub(lead.chars().count()),
        )),
    ]);
    if muted {
        line.style(Style::default().fg(Color::DarkGray))
    } else {
        line
    }
}

pub(crate) fn render_session_scrollbar(
    frame: &mut Frame,
    area: Rect,
    content_length: usize,
    position: usize,
    viewport_content_length: usize,
) {
    if content_length <= viewport_content_length {
        return;
    }
    let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .thumb_style(Style::default().fg(Color::Gray))
        .track_style(Style::default().fg(Color::DarkGray));
    let mut state = ScrollbarState::new(content_length)
        .position(position)
        .viewport_content_length(viewport_content_length.max(1));
    frame.render_stateful_widget(
        scrollbar,
        area.inner(Margin {
            vertical: 1,
            horizontal: 0,
        }),
        &mut state,
    );
}

#[cfg(test)]
fn session_values(
    session: &SessionRecord,
    detail: Option<&SessionDetail>,
    operation: Option<&SessionOperationDisplay>,
    now_epoch_seconds: u64,
    config: &HelConfig,
) -> (String, String, String, String, String) {
    let clock = if let Some(operation) = operation {
        let (label, started_at) = operation_status(operation);
        let elapsed = now_epoch_seconds.saturating_sub(started_at);
        format!("{label} {elapsed}s")
    } else if session.state == SessionState::Provisioning {
        let started_at = session_updated_at_epoch_seconds(session).unwrap_or(now_epoch_seconds);
        format!("Launch {}s", now_epoch_seconds.saturating_sub(started_at))
    } else {
        hel::usage_format::format_activity_clock(
            now_epoch_seconds,
            detail.and_then(|detail| detail.current_turn_started_at),
            detail.map_or(&*EMPTY_ACTIVITY, |detail| &detail.activity),
        )
    };
    // An in-flight resume already told the controller its destination; show
    // that instead of the session record, which the dashboard won't refresh
    // until the operation finishes (see `SessionOperationDisplay::resume_destination`).
    let (profile_id, target_template_id) = operation
        .and_then(|operation| operation.resume_destination.clone())
        .unwrap_or_else(|| {
            (
                session.last_profile.clone(),
                session.target_template_id.clone(),
            )
        });
    (
        clock,
        profile_id,
        target_template_id,
        session.project_name(config),
        session_name(session).to_string(),
    )
}

fn operation_status(operation: &SessionOperationDisplay) -> (String, u64) {
    if matches!(
        operation.kind,
        SessionOperationKind::Launching | SessionOperationKind::Resuming
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

fn session_updated_at_epoch_seconds(session: &SessionRecord) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(&session.updated_at)
        .ok()?
        .timestamp()
        .try_into()
        .ok()
}

fn session_name(session: &SessionRecord) -> &str {
    session.display_title()
}

/// Color of an active session's summary band. An unreachable target is red so
/// it stands out; otherwise unread sessions are highlighted and the rest keep
/// the default. A session whose detail has not loaded yet keeps the default.
/// The colour a session's summary rows carry.
///
/// Red means the session needs attention rather than reading: its relay is
/// unreachable, or the session itself failed. Everything else distinguishes
/// unread work from work already seen.
fn session_band_color(
    detail: Option<&SessionDetail>,
    unreachable: bool,
    state: SessionState,
) -> Color {
    if unreachable || state == SessionState::Error {
        return Color::Red;
    }
    match detail {
        Some(detail) if detail.has_unread() && detail.current_turn_started_at.is_none() => {
            Color::LightBlue
        }
        Some(detail) if detail.has_unread() => Color::LightYellow,
        // ANSI yellow is the orange/amber ink in common terminal palettes;
        // bright yellow remains distinct for unread sessions.
        _ => Color::Yellow,
    }
}

#[cfg(test)]
fn active_message_tail(
    detail: Option<&SessionDetail>,
    width: usize,
    maximum_lines: usize,
) -> Vec<Line<'static>> {
    detail
        .and_then(|detail| detail.last_agent_message.as_deref())
        .map(|message| render_agent_message_tail(message, width, maximum_lines))
        .unwrap_or_default()
}

#[cfg(test)]
pub(crate) fn unread_line(unread_count: usize) -> Line<'static> {
    if unread_count > 0 {
        Line::from(Span::styled(
            format!("{unread_count} unread"),
            Style::default()
                .fg(Color::LightYellow)
                .add_modifier(Modifier::BOLD),
        ))
    } else {
        Line::default()
    }
}

fn checkpoint_age(now_epoch_seconds: u64, checkpointed_at: &str) -> String {
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
const CAPACITY_SAMPLE_STALE_AFTER_SECONDS: u64 = 90;

/// Why the row's reading cannot be trusted, if it cannot: a probe that failed,
/// or a sample that stopped refreshing. `None` means the reading is current.
fn capacity_staleness(detail: &CapacityDetail, now_epoch_seconds: u64) -> Option<String> {
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

pub(crate) fn render_capacity(frame: &mut Frame, area: Rect, dashboard: &mut DashboardState) {
    let now_epoch_seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let rows = dashboard.capacity_details.values().map(|detail| {
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
                // A fleet with nothing running has no capacity figures, and
                // the count is the whole answer.
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
                Style::default().fg(Color::DarkGray),
            ));
        }
        Row::new([
            Cell::from(detail.target.host.clone()),
            Cell::from(capacity_target_labels(
                &detail.target.target_ids,
                &dashboard.config,
            )),
            Cell::from(Line::from(in_use)),
        ])
    });
    let focused = dashboard.focus == Focus::Targets;
    let table = Table::new(
        rows,
        [
            Constraint::Percentage(22),
            Constraint::Percentage(36),
            Constraint::Percentage(42),
        ],
    )
    .header(
        Row::new(["Host / fleet", "Targets", "In Use"])
            .style(Style::default().add_modifier(Modifier::BOLD)),
    )
    .row_highlight_style(if focused {
        Style::default().bg(Color::DarkGray).fg(Color::White)
    } else {
        Style::default()
    })
    .highlight_symbol(if focused { "› " } else { "  " })
    .highlight_spacing(HighlightSpacing::Always)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(focus_border(focused))
            .title(" Targets "),
    );
    let mut state = TableState::default()
        .with_offset(dashboard.targets_scroll.get())
        .with_selected(
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
/// Both collapsed panes read the same scale, which is why it lives in one
/// place: a quota reports the headroom it has left directly, and a CPU reading
/// is the inverse — a busy host has little left.
fn headroom_color(headroom_percent: u8) -> Color {
    match headroom_percent {
        0..=20 => Color::Red,
        21..=50 => Color::Yellow,
        _ => Color::Green,
    }
}

/// One reading in a collapsed pane: a name, its value, and how healthy the
/// value is. A reading with no health to report draws in the ordinary
/// foreground rather than claiming a colour it has not earned.
struct SummaryReading {
    name: String,
    value: String,
    color: Option<Color>,
}

/// One row summarising every target host and its CPU load, for the collapsed
/// Targets pane.
///
/// A reading that cannot be trusted says so rather than showing a number: an
/// unavailable probe, a sample that stopped refreshing, and a fleet reading
/// that carries no CPU figure are all named explicitly.
pub(crate) fn minimized_targets_line(dashboard: &DashboardState, width: u16) -> Line<'static> {
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
    summary_row("Targets", &readings, width)
}

/// One row summarising every profile's quota, for the collapsed Quota pane.
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
pub(crate) fn minimized_quota_line(dashboard: &DashboardState, width: u16) -> Line<'static> {
    let readings = dashboard
        .config
        .profiles
        .iter()
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
                    name: id.clone(),
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
                    name: id.clone(),
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
                name: id.clone(),
                value,
                color: Some(headroom_color(
                    five_hour.map_or(weekly, |five_hour| weekly.min(five_hour)),
                )),
            })
        })
        .collect::<Vec<_>>();
    summary_row("Quota", &readings, width)
}

/// A collapsed pane's single row, drawn as the pane's own title so it keeps
/// the rule the full pane has. Readings are comma-separated and truncated
/// rather than wrapped, because the row is one row.
fn summary_row(label: &str, readings: &[SummaryReading], width: u16) -> Line<'static> {
    // A collapsed pane is still a pane, so its one row opens the way a
    // bordered one does and the rule carries on between the label and the
    // readings: `─ Quota ── claude-1 63% ────`.
    const OPENING: &str = "─ ";
    const DIVIDER: &str = " ── ";
    let mut spans = vec![Span::raw(format!("{OPENING}{label}{DIVIDER}"))];
    let mut used = OPENING.chars().count() + label.chars().count() + DIVIDER.chars().count();
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

const EMPTY_QUOTA_COLOR: Color = Color::DarkGray;
const EMPTY_QUOTA_CELL: &str = "░";
// Both bar kinds occupy the same column, so they must agree on the cell count.
const QUOTA_BAR_CELLS: usize = 10;

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
        0..=20 => Color::Red,
        21..=50 => Color::Yellow,
        _ => Color::Green,
    };
    let bar_style = Style::default().fg(color).add_modifier(Modifier::BOLD);
    Line::from(vec![
        Span::styled("█".repeat(full_cells), bar_style),
        Span::styled(partial.to_string(), bar_style),
        Span::styled(
            EMPTY_QUOTA_CELL.repeat(empty_cells),
            Style::default().fg(EMPTY_QUOTA_COLOR),
        ),
        Span::styled(
            format!(" {remaining:>3}%"),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
    ])
}

/// Renders the API label centered in a field of the same depleted-quota
/// shading the bars use, with the label cells left unshaded.
///
/// The depleted bar has no background color to copy: it is `EMPTY_QUOTA_COLOR`
/// seen through a glyph that covers about a quarter of each cell, so its
/// apparent shade exists only in the eye. Reusing the glyph reproduces that
/// shade exactly under any terminal theme or font, which a fixed color cannot.
fn api_quota_bar() -> Line<'static> {
    let label = hel::hel_quota::API_LABEL;
    let label_cells = label.chars().count().min(QUOTA_BAR_CELLS);
    let left = (QUOTA_BAR_CELLS - label_cells) / 2;
    let right = QUOTA_BAR_CELLS - label_cells - left;
    let shading = Style::default().fg(EMPTY_QUOTA_COLOR);
    Line::from(vec![
        Span::styled(EMPTY_QUOTA_CELL.repeat(left), shading),
        Span::styled(label, Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(EMPTY_QUOTA_CELL.repeat(right), shading),
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

fn quota_reset_cell(window: Option<&QuotaWindow>, now: u64) -> String {
    let Some(window) = window else {
        return String::new();
    };
    window
        .resets_at_epoch_seconds
        .map(|reset| quota_reset_countdown(now, reset))
        .or_else(|| window.resets.clone())
        .unwrap_or_default()
}

fn quota_reset_cells(quota: &ProfileQuota, now: u64) -> (String, String) {
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
        quota_reset_cell(quota.five_hour_window(), now)
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

fn quota_column_width(
    header: &str,
    content_widths: impl Iterator<Item = usize>,
    maximum: u16,
) -> u16 {
    let width = content_widths.fold(Line::raw(header).width(), usize::max);
    u16::try_from(width).unwrap_or(u16::MAX).min(maximum)
}

pub(crate) fn render_quotas(frame: &mut Frame, area: Rect, dashboard: &mut DashboardState) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let rows = dashboard
        .config
        .profiles
        .iter()
        .map(|(id, profile)| {
            let (weekly, weekly_reset, five_hour, five_hour_reset) =
                if profile.kind == HarnessKind::Deepseek {
                    (
                        api_quota_bar(),
                        String::new(),
                        Line::default(),
                        String::new(),
                    )
                } else if dashboard.quota_refreshing.contains(id) {
                    (
                        Line::raw("refreshing…"),
                        String::new(),
                        Line::default(),
                        String::new(),
                    )
                } else {
                    match dashboard.quotas.get(id) {
                        Some(quota) if quota.error.is_none() => {
                            let (weekly_reset, five_hour_reset) = quota_reset_cells(quota, now);
                            (
                                quota_bar(quota.weekly_window()),
                                weekly_reset,
                                five_hour_quota_bar(quota),
                                five_hour_reset,
                            )
                        }
                        Some(quota) => (
                            Line::raw(
                                quota
                                    .error_label()
                                    .unwrap_or_else(|| "unavailable: unknown error".into()),
                            ),
                            String::new(),
                            Line::default(),
                            String::new(),
                        ),
                        None => (
                            Line::raw("refreshing…"),
                            String::new(),
                            Line::default(),
                            String::new(),
                        ),
                    }
                };
            QuotaTableRow {
                profile: id.clone(),
                harness: profile.kind.display_name().into(),
                weekly,
                weekly_reset,
                five_hour,
                five_hour_reset,
            }
        })
        .collect::<Vec<_>>();
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
    let title = Line::from(vec![
        Span::raw(" Quota "),
        Span::styled(
            format!("({refresh_status}) "),
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    let quotas_focused = dashboard.focus == Focus::Quota;
    let border_type = if quotas_focused {
        BorderType::Double
    } else {
        BorderType::Plain
    };
    let widths = [
        quota_column_width(
            "Profile",
            rows.iter()
                .map(|row| Line::raw(row.profile.as_str()).width()),
            24,
        ),
        quota_column_width(
            "Harness",
            rows.iter()
                .map(|row| Line::raw(row.harness.as_str()).width()),
            12,
        ),
        quota_column_width("Weekly", rows.iter().map(|row| row.weekly.width()), 32),
        quota_column_width(
            "Resets",
            rows.iter()
                .map(|row| Line::raw(row.weekly_reset.as_str()).width()),
            24,
        ),
        quota_column_width("5H", rows.iter().map(|row| row.five_hour.width()), 15),
        quota_column_width(
            "Resets",
            rows.iter()
                .map(|row| Line::raw(row.five_hour_reset.as_str()).width()),
            24,
        ),
    ]
    .map(Constraint::Length);
    let table = Table::new(rows.into_iter().map(QuotaTableRow::into_row), widths)
        .column_spacing(2)
        .header(
            Row::new(["Profile", "Harness", "Weekly", "Resets", "5H", "Resets"])
                .style(Style::default().add_modifier(Modifier::BOLD)),
        )
        .row_highlight_style(if quotas_focused {
            Style::default().bg(Color::DarkGray).fg(Color::White)
        } else {
            Style::default()
        })
        .highlight_symbol(if quotas_focused { "› " } else { "  " })
        .highlight_spacing(HighlightSpacing::Always)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(border_type)
                .title(title),
        );
    let mut state = TableState::default()
        .with_offset(dashboard.quota_scroll.get())
        .with_selected((!dashboard.config.profiles.is_empty()).then_some(dashboard.quota_index));
    frame.render_stateful_widget(table, area, &mut state);
    dashboard.quota_scroll.set(state.offset());
    render_session_scrollbar(
        frame,
        area,
        dashboard.config.profiles.len(),
        state.offset(),
        usize::from(area.height.saturating_sub(SESSION_TABLE_CHROME_HEIGHT)),
    );
}

/// The hotkey hints for whichever pane owns the keyboard.
///
/// The composer's hints come from the chat itself, because they depend on
/// what it is doing (a queued prompt, dictation, a history search).
pub(crate) fn combined_footer_text(dashboard: &DashboardState) -> &'static str {
    match dashboard.focus {
        Focus::Sessions => {
            "Enter open · n new · s resume · e edit · a mark read · x cancel · Tab pane · Ctrl-G panes · F2 workspaces · F3 web · Ctrl-Q quit"
        }
        Focus::Targets => {
            "r refresh · Enter/e actions · Tab pane · Ctrl-G panes · F2 workspaces · F3 web · Ctrl-Q quit"
        }
        Focus::Quota => {
            "r refresh · Enter/e edit profile · Tab pane · Ctrl-G panes · F2 workspaces · F3 web · Ctrl-Q quit"
        }
        Focus::Prompt => "Tab pane · Ctrl-G panes · F2 workspaces · F3 web · Ctrl-Q quit",
    }
}

/// Draws the shared footer row.
///
/// A notice replaces the hints while one is showing, the same way the
/// composer's own footer works, so the two are interchangeable and the row
/// costs one line whichever surface drew it.
pub(crate) fn render_footer(frame: &mut Frame, area: Rect, dashboard: &DashboardState) {
    let notice = dashboard.notices.current();
    let (text, color) = match notice.as_deref() {
        Some(notice) => (notice.to_owned(), Color::Yellow),
        None => (combined_footer_text(dashboard).to_owned(), Color::DarkGray),
    };
    frame.render_widget(
        Paragraph::new(Line::styled(text, Style::default().fg(color))),
        area,
    );
}

fn refresh_age(now: u64, refreshed: u64) -> String {
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

    use crossterm::event::KeyCode;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::Color;

    use hel::hel_config::{HarnessKind, HelConfig, ProjectRepository};
    use hel::hel_quota::{ProfileQuota, QuotaWindow};
    use hel::hel_selection::SurfaceId;
    use hel::hel_state::{
        HelState, MaterializedExecutionState, STATE_VERSION, SessionState, TranscriptBody,
    };
    use hel::hel_targets::{DeploymentCapacityUsage, ProvisionStage};

    use super::*;
    use crate::test_support::*;

    use crate::ingest::SessionDetail;
    use crate::{DashboardAction, DashboardState, Focus, SessionOperationKind};

    #[test]
    fn grouped_dashboard_has_no_column_header_and_uses_fixed_session_summaries() {
        let mut dashboard = dashboard_with_session(running_session());
        apply_materialized_transcript(&mut dashboard, numbered_conversation(2));
        dashboard
            .session_details
            .get_mut("session-1")
            .unwrap()
            .queued_prompts
            .push(hel::hel_worker::QueuedPrompt {
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
        assert!(rendered.contains("podman  [Q 1]  codex-1  ACP pretty name"));
        assert!(
            rendered.contains("Sessions · Turn=time since prompt · Step=time since agent activity")
        );
        assert!(rendered.contains("  codex-1  ACP pretty name"));
        assert!(!rendered.contains("queued]"));
        assert!(rendered.contains("You: question 1"));
        assert!(rendered.contains("answer 1"));

        let buffer = terminal.backend().buffer();
        let lines = buffer_lines(buffer);
        let (user_row, user_line) = lines
            .iter()
            .enumerate()
            .find(|(_, line)| line.contains("You: question 1"))
            .expect("user transcript line");
        let user_column = cell_column(user_line, "You: question 1");
        assert!((user_column..user_column + 15).all(|column| {
            buffer[(buffer.area.x + column, buffer.area.y + user_row as u16)].fg == Color::DarkGray
        }));
    }

    #[test]
    fn a_modal_overlays_the_dashboard_instead_of_replacing_it() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.set_workspace_name("UNDERLYING DASHBOARD SENTINEL".into());
        assert_eq!(
            dashboard.handle_key(crate::test_support::key(KeyCode::Char('e'))),
            DashboardAction::None
        );
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
        assert!(row_of("UNDERLYING DASHBOARD SENTINEL") < popup_top);
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

        assert!(rendered.contains("You: unanswered follow-up"));
        assert!(rendered.contains(" Checking the workspace"), "{rendered}");
        assert!(!rendered.contains("Agent:"));
        assert!(!rendered.contains("answer 0"));
        let (user_row, user_line) = lines
            .iter()
            .enumerate()
            .find(|(_, line)| line.contains("You: unanswered follow-up"))
            .expect("user transcript line");
        let user_column = cell_column(user_line, "You: unanswered follow-up");
        assert_ne!(
            buffer[(buffer.area.x + user_column, buffer.area.y + user_row as u16)].fg,
            Color::DarkGray
        );
    }

    #[test]
    fn the_sessions_title_keeps_its_clock_key_only_while_the_key_fits() {
        // Every tier names both clocks the rows name, and each one fits the
        // width that selects it.
        for width in [120, 60, 40, 32] {
            let title = sessions_pane_title(width, true);
            assert!(title.chars().count() <= usize::from(width) - 2, "{title:?}");
            if width > 32 {
                assert!(title.contains("Turn"), "{title:?}");
                assert!(title.contains("Step"), "{title:?}");
            }
        }
        assert_eq!(sessions_pane_title(32, true), " Sessions ");
        assert_eq!(sessions_pane_title(120, false), " Sessions ");
    }

    #[test]
    fn dashboard_agent_prefixes_show_active_clocks_and_idle_activity_time() {
        let detail = SessionDetail {
            current_turn_started_at: Some(1_000),
            last_acp_activity_at_ms: Some(1_297_000),
            ..SessionDetail::default()
        };

        assert_eq!(
            dashboard_agent_prefixes(1_330, Some(&detail)),
            ["Turn  5m30s", "Step    33s"]
        );

        let idle = SessionDetail {
            last_activity_at_ms: Some(1_297_000),
            ..SessionDetail::default()
        };
        let activity_time = hel::hel_chat::format_event_time(Some(1_297_000)).unwrap();
        assert_eq!(
            dashboard_agent_prefixes(1_330, Some(&idle)),
            ["Agent:".to_owned(), format!("{activity_time:<6}")]
        );

        // An idle agent with a command still running says so in the turn
        // column and keeps the time it last spoke beside it.
        let background = SessionDetail {
            last_activity_at_ms: Some(1_297_000),
            activity: hel::usage_format::SessionActivity {
                harness_turn_started_at_ms: None,
                background_commands: vec![hel::hel_worker::BackgroundCommand {
                    started_at_ms: 1_000_000,
                    command: "cargo test".into(),
                }],
            },
            ..SessionDetail::default()
        };
        assert_eq!(
            dashboard_agent_prefixes(1_330, Some(&background)),
            ["  BG  5m30s".to_owned(), format!("{activity_time:<6}")]
        );
    }

    #[test]
    fn idle_dashboard_moves_state_and_activity_time_beside_the_agent_excerpt() {
        let mut dashboard = dashboard_with_session(running_session());
        let mut materialized =
            materialized_session_for("session-1", vec![agent_message(2, "Finished work")]);
        materialized.execution = MaterializedExecutionState::Idle;
        dashboard.apply_materialized_session(&materialized);
        let activity_time = hel::hel_chat::format_event_time(Some(2_000)).unwrap();
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("terminal");

        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw dashboard");
        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");

        assert!(!rendered.contains("[idle]"), "{rendered}");
        assert!(rendered.contains("Agent: Finished work"), "{rendered}");
        assert!(
            rendered.contains(&format!("{activity_time}  ")),
            "{rendered}"
        );
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
        let state = HelState {
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
            .position(|line| line.contains("podman [1]"))
            .expect("first session row") as u16;
        let second_y = lines
            .iter()
            .position(|line| line.contains("podman [2]"))
            .expect("second session row") as u16;
        assert!(
            (first_y..first_y + 4).all(|y| {
                (buffer.area.x + 1..buffer.area.right() - 1)
                    .all(|x| buffer[(x, y)].bg != Color::DarkGray)
            }),
            "selection must not paint a background"
        );
        assert!(lines[first_y as usize].contains("› podman [1]"));
        assert_eq!(
            second_y,
            first_y + 5,
            "sessions in an expanded project have one blank row between them"
        );
        assert!(
            (buffer.area.x + 1..buffer.area.right() - 1)
                .all(|x| buffer[(x, first_y + 4)].symbol().trim().is_empty())
        );
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
        let state = HelState {
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
            .position(|line| line.contains("› podman"))
            .expect("first session row") as u16;
        let second_heading_y = lines
            .iter()
            .position(|line| line.contains("beta"))
            .expect("second project heading") as u16;
        let first_bottom = first_y + 4;
        assert_eq!(second_heading_y, first_bottom + 1);
        assert!(
            (buffer.area.x + 1..buffer.area.right() - 1)
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
        let state = HelState {
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
        assert!(first_draw.contains("You: question 0"));
        assert!(first_draw.contains("You: beta question"));

        // The numbered hotkey collapses only its own project.
        assert_eq!(
            dashboard.handle_key(crate::test_support::key(KeyCode::Char('2'))),
            DashboardAction::None
        );
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw with beta collapsed");
        let second_draw = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(second_draw.contains("You: question 0"), "{second_draw}");
        assert!(!second_draw.contains("You: beta question"), "{second_draw}");
        assert!(
            second_draw
                .lines()
                .any(|line| line.contains("podman  ") && line.contains("beta answer")),
            "the collapsed group keeps a one-line row per session: {second_draw}"
        );

        // Collapsing alpha too leaves both groups collapsed at once.
        dashboard.handle_key(crate::test_support::key(KeyCode::Char('1')));
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw with both collapsed");
        let third_draw = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(!third_draw.contains("You: question 0"), "{third_draw}");
        assert!(!third_draw.contains("You: beta question"), "{third_draw}");

        // And the hotkey is a toggle, so pressing it again brings beta back.
        dashboard.handle_key(crate::test_support::key(KeyCode::Char('2')));
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw with beta expanded again");
        let fourth_draw = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(fourth_draw.contains("You: beta question"), "{fourth_draw}");
        assert!(!fourth_draw.contains("You: question 0"), "{fourth_draw}");
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
        let state = HelState {
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
        let mut terminal = Terminal::new(TestBackend::new(120, 44)).expect("terminal");
        // Collapse the beta project so its sessions draw their one-line form,
        // which is where duplicate targets need their numbering.
        dashboard.handle_key(crate::test_support::key(KeyCode::Char('2')));

        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw collapsed duplicate targets");
        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");

        assert!(
            rendered
                .lines()
                .any(|line| line.contains("podman [1]  ") && line.contains("first tail")),
            "{rendered}"
        );
        assert!(
            rendered
                .lines()
                .any(|line| line.contains("podman [2]  ") && line.contains("second tail")),
            "{rendered}"
        );
    }

    #[test]
    fn summary_band_colors_distinguish_normal_unread_and_unread_idle() {
        let normal = SessionDetail {
            current_turn_started_at: Some(1),
            ..SessionDetail::default()
        };
        assert_eq!(
            session_band_color(Some(&normal), false, SessionState::Running),
            Color::Yellow
        );

        let unread = SessionDetail {
            current_turn_started_at: Some(1),
            unread_agent_messages: 1,
            ..SessionDetail::default()
        };
        assert_eq!(
            session_band_color(Some(&unread), false, SessionState::Running),
            Color::LightYellow
        );

        let unread_idle = SessionDetail {
            unread_agent_messages: 1,
            ..SessionDetail::default()
        };
        assert_eq!(
            session_band_color(Some(&unread_idle), false, SessionState::Running),
            Color::LightBlue
        );

        let collapsed = collapsed_session_line(
            "› ",
            "podman",
            SessionRowFacts {
                detail: Some(&unread_idle),
                unreachable: false,
                state: SessionState::Running,
                now_epoch_seconds: 1,
            },
            80,
            None,
        );
        assert_eq!(collapsed.style.fg, Some(Color::LightBlue));

        let restarted_idle = SessionDetail {
            unread_session_restarts: 1,
            ..SessionDetail::default()
        };
        assert_eq!(
            session_band_color(Some(&restarted_idle), false, SessionState::Running),
            Color::LightBlue
        );

        let restarted_running = SessionDetail {
            current_turn_started_at: Some(1),
            unread_session_restarts: 1,
            ..SessionDetail::default()
        };
        assert_eq!(
            session_band_color(Some(&restarted_running), false, SessionState::Running),
            Color::LightYellow
        );

        // An unreachable target is red, overriding every other state.
        assert_eq!(
            session_band_color(Some(&unread), true, SessionState::Running),
            Color::Red
        );
        assert_eq!(
            session_band_color(None, true, SessionState::Running),
            Color::Red
        );
        let unreachable_line = collapsed_session_line(
            "› ",
            "podman",
            SessionRowFacts {
                detail: Some(&unread_idle),
                unreachable: true,
                state: SessionState::Running,
                now_epoch_seconds: 1,
            },
            80,
            None,
        );
        assert_eq!(unreachable_line.style.fg, Some(Color::Red));
    }

    #[test]
    fn dashboard_replaces_too_short_layout_with_required_height() {
        let mut dashboard = DashboardState::new(config(), HelState::default(), BTreeMap::new());
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
            rendered.contains("at least 16 rows (currently 10)"),
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
        assert!(
            rendered.contains("Sessions · Turn=time since prompt · Step=time since agent activity")
        );
        assert!(rendered.contains("Quota"));
    }

    #[test]
    fn dashboard_replaces_layouts_narrower_than_32_columns() {
        let mut dashboard = DashboardState::new(config(), HelState::default(), BTreeMap::new());
        let mut terminal = Terminal::new(TestBackend::new(31, 24)).expect("terminal");
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
        assert!(rendered.contains("Need at least 32 columns"));
        assert!(rendered.contains("Current width: 31"));

        let mut terminal = Terminal::new(TestBackend::new(32, 24)).expect("terminal");
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
        assert!(rendered.contains("Sessions"));
    }

    #[test]
    fn new_session_picker_keeps_choices_and_controls_visible_at_minimum_width() {
        let mut dashboard = DashboardState::new(config(), HelState::default(), BTreeMap::new());
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Char('n'))),
            DashboardAction::None
        );
        let mut terminal = Terminal::new(TestBackend::new(32, 24)).expect("terminal");
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
        let mut dashboard = DashboardState::new(config(), HelState::default(), BTreeMap::new());
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

        // The workspace name rides at the right of the Sessions title rather
        // than taking a full row of its own, so the transcript keeps that row.
        assert!(
            line(buffer.area.y).contains("Sessions"),
            "{:?}",
            line(buffer.area.y)
        );
        assert!(line(buffer.area.y).contains("personal"));
        assert!(!line(buffer.area.y).contains("ACP sessions"));
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
        assert!(hotkeys.contains("n new"), "{hotkeys:?}");
        assert!(hotkeys.contains("a mark read"), "{hotkeys:?}");
        assert!(!hotkeys.contains("[S]ort"));
    }

    /// The expanded row draws its own `Agent:`/clock prefix, so the excerpt
    /// beside it must not arrive carrying the transcript's rail as well.
    #[test]
    fn an_expanded_agent_excerpt_carries_no_transcript_gutter() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        apply_materialized_transcript(
            &mut dashboard,
            vec![agent_message(1, "reliability reply: summarize the README")],
        );

        let lines = drawn(&mut dashboard, 120, 44);
        let agent = lines
            .iter()
            .find(|line| line.contains("reliability reply"))
            .expect("the agent excerpt row");
        // The pane has focus, so its own border is the doubled glyph: any
        // light vertical left on this row came from the transcript's rail.
        assert!(
            !agent.contains('\u{2502}'),
            "the excerpt carries no transcript rail: {agent:?}"
        );
    }

    /// The empty band has two causes and they need different advice. Telling
    /// someone there is no live session while the pane above lists one is a
    /// plain lie.
    #[test]
    fn the_empty_prompt_distinguishes_no_session_from_no_conversation() {
        let mut empty = DashboardState::new(config(), HelState::default(), BTreeMap::new());
        let lines = drawn(&mut empty, 120, 44).join("\n");
        assert!(lines.contains("Prompt (no live session)"), "{lines}");
        assert!(lines.contains("n to create or s to resume"), "{lines}");

        // A live session that simply is not open says so instead.
        let mut live = dashboard_with_session(running_session());
        let lines = drawn(&mut live, 120, 44).join("\n");
        assert!(lines.contains("Prompt (no conversation open)"), "{lines}");
        assert!(lines.contains("Enter on the one to open"), "{lines}");
        assert!(!lines.contains("No live session"), "{lines}");
    }

    /// The band order is the whole point of the surface: everything is on one
    /// screen, in one arrangement, at every size it draws at.
    #[test]
    fn the_combined_surface_keeps_its_band_order_at_every_size() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.set_deployment_capacity_targets(vec![test_capacity_target()]);

        for (width, height) in [(140, 32), (60, 20), (32, 16)] {
            let lines = drawn(&mut dashboard, width, height);
            let row_of = |needle: &str| {
                lines
                    .iter()
                    .position(|line| line.contains(needle))
                    .unwrap_or_else(|| panic!("missing {needle} at {width}x{height}: {lines:#?}"))
            };
            let sessions = row_of("Sessions");
            let conversation = row_of("Conversation");
            let prompt = row_of("Prompt");
            let targets = row_of("Targets");
            let quota = row_of("Quota");
            assert!(
                sessions < conversation
                    && conversation < prompt
                    && prompt < targets
                    && targets < quota,
                "band order at {width}x{height}: {lines:#?}"
            );
            // The footer is the last row and always says something.
            assert!(
                !lines[lines.len() - 1].trim().is_empty(),
                "footer at {width}x{height}: {lines:#?}"
            );
        }
    }

    /// Collapsing the support panes hands their freed rows to the transcript,
    /// and the transcript also absorbs whatever the Sessions pane gives up (or
    /// gives back) as it moves to its fixed mode-2 third — nothing appears or
    /// vanishes, so the gesture is measurable rather than merely visible.
    #[test]
    fn collapsing_the_support_panes_gives_their_rows_to_the_transcript() {
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
        dashboard.cycle_pane_layout();
        let after = drawn(&mut dashboard, 140, 44);

        // Every row the tables and the Sessions pane give up lands in the
        // transcript; the composer and footer are untouched.
        let tables_freed = (band(&before, "Targets", "Quota") - band(&after, "Targets", "Quota"))
            + (band(&before, "Quota", "Ctrl-Q quit") - band(&after, "Quota", "Ctrl-Q quit"));
        let sessions_freed =
            band(&before, "Sessions", "Conversation") - band(&after, "Sessions", "Conversation");
        let transcript_gain =
            band(&after, "Conversation", "Prompt") - band(&before, "Conversation", "Prompt");
        assert!(tables_freed > 0, "the tables gave up nothing");
        assert_eq!(transcript_gain, tables_freed + sessions_freed);
        // Each collapsed pane really is one row.
        assert_eq!(band(&after, "Targets", "Quota"), 1);
        assert_eq!(band(&after, "Quota", "Ctrl-Q quit"), 1);
    }

    fn now_seconds() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    fn host_usage(cpu_percent: u8) -> hel::hel_targets::DeploymentCapacityUsage {
        hel::hel_targets::DeploymentCapacityUsage {
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
            extra: Some(hel::hel_quota::API_LABEL.into()),
            error: None,
            refreshed_at_epoch_seconds: now_seconds(),
        }
    }

    /// Adds a usage-priced profile to the dashboard's configuration, since the
    /// shared fixture only carries subscription profiles.
    fn add_deepseek_profile(dashboard: &mut DashboardState) {
        dashboard.config.profiles.insert(
            "deepseek".into(),
            hel::hel_config::HarnessProfile {
                context_window_bytes: None,
                kind: HarnessKind::Deepseek,
                home: std::path::PathBuf::from("/profiles/deepseek"),
                executable: None,
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
        let started_at_ms = i64::try_from(hel::clock::epoch_seconds()).unwrap() * 1_000 - 2_616_000;
        let activity = hel::usage_format::SessionActivity {
            harness_turn_started_at_ms: None,
            background_commands: vec![hel::hel_worker::BackgroundCommand {
                started_at_ms,
                command: "cargo test".into(),
            }],
        };

        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        assert!(
            drawn(&mut dashboard, 120, 44)
                .iter()
                .any(|line| line.contains("Agent:")),
            "an idle session with nothing running shows when it last spoke"
        );

        dashboard.set_session_activity("session-1", activity.clone());
        let expanded = drawn(&mut dashboard, 120, 44);
        assert!(
            expanded.iter().any(|line| line.contains("  BG 43m3")),
            "the expanded row: {expanded:?}"
        );

        let mut grid = dashboard_with_session(running_session());
        grid.set_session_activity("session-1", activity);
        grid.cycle_pane_layout();
        let cells = drawn(&mut grid, 120, 44);
        assert!(
            cells.iter().any(|line| line.contains("[BG 43m3")),
            "the grid cell: {cells:?}"
        );
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
            .position(|line| line.contains("podman"))
            .expect("the session's identity row");
        assert!(lines[first].contains("ACP pretty name"));
        assert!(lines[first + 1].contains("You:"), "{:?}", lines[first + 1]);
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

    /// `projects` projects, `per_project` live sessions in each, laid out so
    /// the minimized grid has real columns and headings to pack. Project
    /// directories are zero-padded so they sort in the obvious order.
    fn minimized_grid_dashboard(projects: usize, per_project: usize) -> DashboardState {
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
            HelState {
                version: STATE_VERSION,
                sessions,
                mount_history: BTreeMap::new(),
                container_sizes: BTreeMap::new(),
            },
            BTreeMap::new(),
        );
        // One turn of the dial reaches the minimized grid.
        dashboard.cycle_pane_layout();
        dashboard
    }

    /// The content rows of the Sessions pane (inside its border) for a
    /// minimized grid of the given terminal size.
    fn grid_content_rows(dashboard: &mut DashboardState, width: u16, height: u16) -> Vec<String> {
        let rows = crate::combined::minimized_grid_rows(height) as usize;
        let lines = drawn(dashboard, width, height);
        lines[1..=rows].to_vec()
    }

    /// The grid packs every project's sessions under a white heading, filling
    /// column by column, with the turn clock (here `[idle]`) beside each
    /// target. Two sessions sharing a buffer row proves there is more than one
    /// column.
    #[test]
    fn the_minimized_grid_columns_carry_white_headers_targets_and_clocks() {
        let mut dashboard = minimized_grid_dashboard(3, 2);
        let mut terminal = Terminal::new(TestBackend::new(120, 44)).expect("terminal");
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw the grid");
        let buffer = terminal.backend().buffer();
        let lines = buffer_lines(buffer);

        // Every project heading shows, in white and bold.
        for project in ["proj00", "proj01", "proj02"] {
            let row = lines
                .iter()
                .position(|line| line.contains(project))
                .unwrap_or_else(|| panic!("heading {project} missing: {lines:?}"));
            let column = cell_column(&lines[row], project);
            let cell = &buffer[(column, row as u16)];
            assert_eq!(cell.fg, Color::White, "{project} colour");
            assert!(
                cell.modifier.contains(Modifier::BOLD),
                "{project} should be bold"
            );
        }

        // Each session shows its target and its idle clock.
        assert!(
            lines.iter().any(|line| line.contains("podman")),
            "a target: {lines:?}"
        );
        assert!(
            lines.iter().any(|line| line.contains("[idle]")),
            "an idle clock: {lines:?}"
        );

        // Column-major fill means some buffer row carries two sessions side by
        // side.
        assert!(
            lines.iter().any(|line| line.matches("podman").count() >= 2),
            "two columns on one row: {lines:?}"
        );
    }

    /// When more sessions exist than the viewport shows, the last cell reads
    /// "+N more" so it is clear the grid scrolls to reach the rest.
    #[test]
    fn the_minimized_grid_marks_how_many_sessions_it_is_not_showing() {
        // 3 projects x 3 sessions = 12 cells; a tiny 2-row grid shows only 6,
        // hiding 5 sessions plus the one its marker cell covers.
        let mut dashboard = minimized_grid_dashboard(3, 3);
        let lines = drawn(&mut dashboard, 120, 20);
        assert!(
            lines.iter().any(|line| line.contains("+6 more")),
            "expected a +6 more marker: {lines:?}"
        );
    }

    /// A grid that shows every session has nothing to mark.
    #[test]
    fn the_minimized_grid_omits_the_marker_when_everything_fits() {
        let mut dashboard = minimized_grid_dashboard(1, 1);
        let lines = drawn(&mut dashboard, 120, 44);
        assert!(
            !lines.iter().any(|line| line.contains("more")),
            "no marker expected when all sessions fit: {lines:?}"
        );
    }

    /// The clock is right-justified at each column's edge, so the clocks line
    /// up in a column rather than trailing immediately after each target.
    #[test]
    fn the_minimized_grid_right_justifies_the_clock() {
        let mut dashboard = minimized_grid_dashboard(1, 1);
        let rows = grid_content_rows(&mut dashboard, 120, 44);
        let session = rows
            .iter()
            .find(|line| line.contains("[idle]"))
            .expect("the session row");

        // Column 0 spans the inner width less two 2-space gaps, split three
        // ways; the clock ends flush against that column's right edge.
        let inner = 120u16 - 2;
        let column0 = (inner - 4) / 3 + u16::from(!(inner - 4).is_multiple_of(3));
        let expected = 1 + column0 - "[idle]".len() as u16;
        assert_eq!(cell_column(session, "[idle]"), expected, "{session:?}");
    }

    /// A session cell is coloured by the same state rule the expanded rows
    /// use: a healthy running session is yellow, a failed one red.
    #[test]
    fn the_minimized_grid_colours_a_session_cell_by_state() {
        let colour_of = |mut dashboard: DashboardState| {
            let mut terminal = Terminal::new(TestBackend::new(120, 44)).expect("terminal");
            terminal
                .draw(|frame| render(frame, &mut dashboard))
                .expect("draw the grid");
            let buffer = terminal.backend().buffer();
            let lines = buffer_lines(buffer);
            let row = lines
                .iter()
                .position(|line| line.contains("podman"))
                .expect("a session cell");
            buffer[(cell_column(&lines[row], "podman"), row as u16)].fg
        };

        let healthy = minimized_grid_dashboard(1, 1);
        assert_eq!(colour_of(healthy), Color::Yellow);

        let mut failed = minimized_grid_dashboard(1, 1);
        {
            let session = failed
                .state
                .sessions
                .get_mut("session-00")
                .expect("the session");
            session.state = SessionState::Error;
        }
        assert_eq!(colour_of(failed), Color::Red);
    }

    /// The target ellipsizes so the clock always survives, even in a narrow
    /// column.
    #[test]
    fn the_minimized_grid_ellipsizes_the_target_to_keep_the_clock() {
        let mut dashboard = minimized_grid_dashboard(1, 1);
        dashboard
            .state
            .sessions
            .get_mut("session-00")
            .expect("the session")
            .target_template_id = "extremely-long-target-identifier".into();

        let rows = grid_content_rows(&mut dashboard, 44, 22);
        assert!(
            rows.iter().any(|line| line.contains('…')),
            "the target should ellipsize: {rows:?}"
        );
        assert!(
            rows.iter().any(|line| line.contains("[idle]")),
            "the clock should survive: {rows:?}"
        );
    }

    /// The grid is a viewport: selecting a session past the visible columns
    /// scrolls the window so it shows, and earlier columns leave view.
    #[test]
    fn the_minimized_grid_scrolls_to_keep_the_selection_visible() {
        let mut dashboard = minimized_grid_dashboard(12, 1);

        // Selecting the first session keeps the window at the start.
        dashboard.selected_session_id = Some("session-00".into());
        let rows = grid_content_rows(&mut dashboard, 120, 44);
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
        let rows = grid_content_rows(&mut dashboard, 120, 44);
        assert!(
            rows.iter().any(|line| line.contains("proj11")),
            "last project scrolled into view: {rows:?}"
        );
        assert!(
            !rows.iter().any(|line| line.contains("proj00")),
            "first project scrolled out: {rows:?}"
        );
    }

    /// Clicking a grid cell selects that session and leaves the dial where
    /// the user set it; the grid draws the selection itself.
    #[test]
    fn clicking_a_minimized_grid_cell_selects_it_and_keeps_the_grid() {
        use crossterm::event::{MouseButton, MouseEventKind};

        let mut dashboard = minimized_grid_dashboard(2, 2);
        drawn(&mut dashboard, 120, 44);

        let (index, rect) = *dashboard
            .session_row_areas
            .first()
            .expect("a grid cell hitbox");
        let expected = dashboard.ordered_sessions()[index].id.clone();

        dashboard.handle_mouse(mouse_at_row(
            MouseEventKind::Down(MouseButton::Left),
            rect,
            0,
        ));

        assert_eq!(dashboard.selected_session_id(), Some(expected.as_str()));
        assert!(
            dashboard.sessions_compact(),
            "the click should leave the grid alone"
        );
        assert_eq!(dashboard.focus(), Focus::Sessions);
    }

    /// A tiny terminal (under 40 rows) strips the minimized grid down to bare
    /// sessions: no title, no border, and the Targets and Quota panes gone
    /// entirely, so every row it has goes to sessions and the conversation.
    #[test]
    fn a_tiny_terminal_grid_drops_its_border_and_the_support_panes() {
        let mut dashboard = minimized_grid_dashboard(2, 2);
        dashboard.set_deployment_capacity_targets(vec![test_capacity_target()]);
        dashboard.apply_quota(weekly_quota("claude-1", 63));

        let lines = drawn(&mut dashboard, 120, 20);

        // The grid has no border or title, so its very first row is already a
        // session rather than a pane rule — and the sessions still draw.
        assert!(
            lines[0].contains("[idle]"),
            "the grid should start at row 0 with no border/title: {lines:?}"
        );
        assert!(
            !lines[0].contains('┌') && !lines[0].contains("Sessions"),
            "the title and border should be dropped: {lines:?}"
        );
        // Both support panes are gone entirely.
        for gone in ["Targets", "Quota"] {
            assert!(
                !lines.iter().any(|line| line.contains(gone)),
                "{gone} should be dropped: {lines:?}"
            );
        }
        let pane = dashboard.pane_areas.expect("pane geometry")[0];
        let selection = dashboard
            .frame_surfaces()
            .surface(SurfaceId::DashboardPane(0))
            .expect("tiny grid selection surface");
        assert_eq!(selection.rect, pane);
        assert_eq!(selection.rect.height, 2);
    }

    /// The grid reads the height threshold from the live frame every render,
    /// so the same dashboard drawn tall then short switches from the bordered
    /// five-row form to the bare two-row one — the decision is never cached.
    /// (Checked on row 0 of the Sessions band; "Sessions" also appears in the
    /// empty-conversation prompt lower down, so the whole buffer is no test.)
    #[test]
    fn the_minimized_grid_reevaluates_the_height_threshold_each_frame() {
        let mut dashboard = minimized_grid_dashboard(2, 2);

        // Tall: the sessions band is bordered and titled on its top row.
        let tall = drawn(&mut dashboard, 120, 44);
        assert!(
            tall[0].contains('┌') && tall[0].contains("Sessions"),
            "tall keeps the bordered title: {:?}",
            tall[0]
        );

        // Same dashboard, now short: no state carried over, so row 0 is a
        // borderless, title-less session row.
        let short = drawn(&mut dashboard, 120, 20);
        assert!(
            !short[0].contains('┌') && !short[0].contains("Sessions"),
            "short drops the title/border: {:?}",
            short[0]
        );

        // And back to tall restores the bordered form.
        let tall_again = drawn(&mut dashboard, 120, 44);
        assert!(
            tall_again[0].contains('┌') && tall_again[0].contains("Sessions"),
            "tall again restores the border: {:?}",
            tall_again[0]
        );
    }

    /// The collapsed dial draws the fixed grid on a landscape terminal: the
    /// Sessions pane is exactly the grid's five bordered rows.
    #[test]
    fn collapsed_on_a_landscape_terminal_draws_the_grid() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.cycle_pane_layout();

        let lines = drawn(&mut dashboard, 120, 40);

        assert!(dashboard.sessions_compact());
        let sessions_height = lines
            .iter()
            .position(|line| line.contains("Conversation"))
            .expect("the conversation band");
        assert_eq!(
            sessions_height,
            usize::from(crate::combined::minimized_grid_rows(40)) + 2,
            "{lines:#?}"
        );
    }

    /// The same dial position on a portrait terminal keeps the scrolling list,
    /// with the 1:2 Sessions/conversation split mode 2 has always had.
    #[test]
    fn collapsed_on_a_portrait_terminal_draws_the_list_with_the_one_third_split() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.cycle_pane_layout();

        let (width, height) = (40u16, 120u16);
        let lines = drawn(&mut dashboard, width, height);

        assert!(!dashboard.sessions_compact());
        let sessions_height = lines
            .iter()
            .position(|line| line.contains("Conversation"))
            .expect("the conversation band");
        // 120 tall, the four-row empty-prompt band, two collapsed support
        // rows, one footer row: shared = 113, and Sessions takes a third.
        assert_eq!(sessions_height, (120 - 4 - 2 - 1) / 3, "{lines:#?}");
    }

    /// A tiny portrait terminal keeps the session list rather than the grid,
    /// and still drops the collapsed Targets and Quota rows.
    #[test]
    fn a_tiny_portrait_terminal_drops_the_support_panes_but_keeps_the_list() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.set_deployment_capacity_targets(vec![test_capacity_target()]);
        dashboard.apply_quota(weekly_quota("claude-1", 63));
        dashboard.cycle_pane_layout();

        let lines = drawn(&mut dashboard, 34, 38);

        assert!(
            lines[0].contains('┌') && lines[0].contains("Sessions"),
            "the portrait list keeps its bordered Sessions title: {lines:?}"
        );
        for gone in ["Targets", "Quota"] {
            assert!(
                !lines.iter().any(|line| line.contains(gone)),
                "{gone} should be dropped: {lines:?}"
            );
        }
    }

    /// `count` live one-line sessions in a single project, so the
    /// support-collapsed pane has more rows than fit and has to scroll.
    fn scrollable_sessions_dashboard(count: usize) -> DashboardState {
        let mut sessions = BTreeMap::new();
        for index in 0..count {
            let mut session = running_session();
            session.id = format!("session-{index:02}");
            session.created_at = format!("2026-08-{:02}T00:00:00Z", index + 1);
            sessions.insert(session.id.clone(), session);
        }
        let mut dashboard = DashboardState::new(
            config(),
            HelState {
                version: STATE_VERSION,
                sessions,
                mount_history: BTreeMap::new(),
                container_sizes: BTreeMap::new(),
            },
            BTreeMap::new(),
        );
        // One turn of the dial: support panes collapsed, sessions one line each.
        dashboard.cycle_pane_layout();
        dashboard.focus_sessions();
        dashboard
    }

    /// Navigating the Sessions pane scrolls only as far as it must to keep the
    /// selection on screen. Drawn in a portrait terminal, where the collapsed
    /// dial keeps the scrolling list rather than the fixed grid. Once a down-arrow has scrolled to reveal a row, an
    /// up-arrow that lands on a still-visible row must not scroll back — the
    /// pane only scrolls up again when the selection would leave the top.
    #[test]
    fn the_sessions_pane_scrolls_only_to_keep_the_selection_visible() {
        let mut dashboard = scrollable_sessions_dashboard(12);
        // Collapse the project so each session is one line and several show at
        // once — the case where over-scrolling would be visible.
        for key in dashboard.project_keys() {
            dashboard.collapsed_project_keys.insert(key);
        }
        dashboard.selected_session_id = Some("session-00".into());

        // At the top, no scroll.
        drawn(&mut dashboard, 36, 42);
        assert_eq!(dashboard.sessions_scroll.get(), 0);

        // Arrow down far enough that the pane has to scroll.
        for _ in 0..9 {
            dashboard.handle_key(key(KeyCode::Down));
            drawn(&mut dashboard, 36, 42);
        }
        let scrolled = dashboard.sessions_scroll.get();
        assert!(scrolled > 0, "the pane should have scrolled down");

        // One arrow up lands on a row that is still visible, so the pane holds
        // its position rather than scrolling back toward the top.
        dashboard.handle_key(key(KeyCode::Up));
        drawn(&mut dashboard, 36, 42);
        assert_eq!(
            dashboard.sessions_scroll.get(),
            scrolled,
            "an up-arrow onto a visible row must not scroll"
        );

        // Walking all the way back to the first session does scroll up.
        for _ in 0..11 {
            dashboard.handle_key(key(KeyCode::Up));
            drawn(&mut dashboard, 36, 42);
        }
        assert_eq!(dashboard.sessions_scroll.get(), 0);
    }

    #[test]
    fn the_minimized_rows_report_cpu_and_weekly_percent_used() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.set_deployment_capacity_targets(vec![test_capacity_target()]);
        dashboard.apply_deployment_capacity("local", Ok(Some(host_usage(42))), now_seconds());
        dashboard.apply_quota(weekly_quota("claude-1", 63));
        dashboard.cycle_pane_layout();

        let lines = drawn(&mut dashboard, 120, 44);
        let targets = lines
            .iter()
            .find(|line| line.contains("─ Targets ──"))
            .expect("the collapsed Targets row");
        assert!(targets.contains("local 42%"), "{targets:?}");
        let quota = lines
            .iter()
            .find(|line| line.contains("─ Quota ──"))
            .expect("the collapsed Quota row");
        // The open pane prints the remaining percentage; so does this row.
        assert!(quota.contains("claude-1 63%"), "{quota:?}");
        // Each collapsed pane is exactly one row.
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
    fn fleet_target(count: usize) -> hel::hel_targets::DeploymentCapacityTarget {
        hel::hel_targets::DeploymentCapacityTarget {
            id: "aws:ec2".into(),
            host: "ec2".into(),
            target_ids: vec!["ec2".into()],
            kind: DeploymentCapacityKind::AwsFleet,
            local: false,
            probes: (0..count)
                .map(|index| {
                    hel::hel_targets::CommandSpec::new("true", [format!("instance-{index}")])
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
                    Ok(Some(hel::hel_targets::DeploymentCapacityUsage {
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

            dashboard.cycle_pane_layout();
            let collapsed = drawn(&mut dashboard, 140, 44)
                .into_iter()
                .find(|line| line.contains("─ Targets ──"))
                .expect("the collapsed Targets row");
            assert!(
                collapsed.contains(&format!("ec2 {expected}")),
                "collapsed row, {count}: {collapsed}"
            );
            assert!(!collapsed.contains("no CPU"), "collapsed row, {count}");
        }
    }

    /// An exhausted profile reads 0%, the same as the open pane's bar. Showing
    /// how much has been *used* would read 100% there, which looks like a
    /// profile in the best possible shape rather than one with nothing left.
    #[test]
    fn an_exhausted_quota_reads_zero_in_the_collapsed_row() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.apply_quota(weekly_quota("claude-1", 0));
        dashboard.cycle_pane_layout();

        let quota = drawn(&mut dashboard, 120, 44)
            .into_iter()
            .find(|line| line.contains("─ Quota ──"))
            .expect("the collapsed Quota row");
        assert!(quota.contains("claude-1 0%"), "{quota:?}");
        assert!(!quota.contains("claude-1 100%"), "{quota:?}");
    }

    /// A reading that cannot be trusted has to say so. A number that is
    /// actually missing, stale or inapplicable is worse than no number.
    #[test]
    fn the_minimized_rows_stay_explicit_about_readings_they_do_not_have() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.set_deployment_capacity_targets(vec![test_capacity_target()]);
        dashboard.cycle_pane_layout();

        // No sample at all.
        let lines = drawn(&mut dashboard, 120, 44);
        let targets = |lines: &[String]| {
            lines
                .iter()
                .find(|line| line.contains("─ Targets ──"))
                .expect("the collapsed Targets row")
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
            .expect("the collapsed Quota row");
        assert!(quota.contains("claude-1 unavailable"), "{quota:?}");
    }

    /// A failed session used to render identically to a healthy one, so the
    /// only thing that told you it had failed was pressing Enter on it. Red
    /// is the same signal an unreachable relay carries: this row needs
    /// attention rather than reading.
    #[test]
    fn a_failed_session_draws_a_red_summary_in_both_pane_modes() {
        for focus in [Focus::Sessions, Focus::Prompt] {
            let mut healthy = dashboard_with_session(running_session());
            healthy.focus = focus;
            let mut failed = {
                let mut session = running_session();
                session.state = SessionState::Error;
                session.last_error = Some("worker bootstrap failed".into());
                dashboard_with_session(session)
            };
            failed.focus = focus;

            let row_colour = |dashboard: &mut DashboardState| {
                let mut terminal = Terminal::new(TestBackend::new(120, 44)).expect("terminal");
                terminal
                    .draw(|frame| render(frame, dashboard))
                    .expect("draw the session list");
                let buffer = terminal.backend().buffer();
                let lines = buffer_lines(buffer);
                let row = lines
                    .iter()
                    .position(|line| line.contains("podman"))
                    .expect("the session's row");
                let column = cell_column(&lines[row], "podman");
                buffer[(column, row as u16)].fg
            };

            assert_eq!(row_colour(&mut failed), Color::Red, "{focus:?}");
            assert_ne!(
                row_colour(&mut healthy),
                Color::Red,
                "{focus:?}: only a session that needs attention is red"
            );
        }
    }

    /// The collapsed rows read as pane titles: the pane's rule, plain text,
    /// and readings separated by commas. Colour is the only thing carrying
    /// meaning, and it comes from the same scale the full quota bar uses.
    #[test]
    fn the_collapsed_rows_keep_the_pane_rule_and_colour_by_headroom() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.set_deployment_capacity_targets(vec![
            test_capacity_target(),
            hel::hel_targets::DeploymentCapacityTarget {
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
        dashboard.cycle_pane_layout();

        let mut terminal = Terminal::new(TestBackend::new(120, 44)).expect("terminal");
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw the collapsed panes");
        let buffer = terminal.backend().buffer();
        let lines = buffer_lines(buffer);

        let targets_row = lines
            .iter()
            .position(|line| line.contains("─ Targets ──"))
            .expect("the collapsed Targets row");
        let targets = &lines[targets_row];
        assert!(targets.contains("local 3%, morannon 95%"), "{targets:?}");
        assert!(
            targets.ends_with('─'),
            "the collapsed pane keeps its rule: {targets:?}"
        );

        let quota_row = lines
            .iter()
            .position(|line| line.contains("─ Quota ──"))
            .expect("the collapsed Quota row");
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
        assert_eq!(colour_of(targets_row, "3%"), Color::Green);
        assert_eq!(colour_of(targets_row, "95%"), Color::Red);
        assert_eq!(colour_of(quota_row, "63%"), Color::Green);
        assert_eq!(colour_of(quota_row, "10%"), Color::Red);
        // The label and the names are ordinary text; only the values carry a
        // colour.
        assert_eq!(colour_of(targets_row, "Targets"), Color::Reset);
        assert_eq!(colour_of(targets_row, "morannon"), Color::Reset);
    }

    /// A usage-priced profile has no window to summarise, so it is left out of
    /// the collapsed row rather than spending width on a placeholder - both
    /// once its own report says it is API-priced and before that report has
    /// arrived.
    #[test]
    fn a_usage_priced_profile_is_absent_from_the_collapsed_row() {
        let mut dashboard = dashboard_with_session(running_session());
        add_deepseek_profile(&mut dashboard);
        dashboard.cycle_pane_layout();

        let row = |dashboard: &mut DashboardState| {
            drawn(dashboard, 160, 44)
                .into_iter()
                .find(|line| line.contains("─ Quota ──"))
                .expect("the collapsed Quota row")
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
    fn the_collapsed_row_pairs_the_weekly_and_five_hour_figures() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.apply_quota(weekly_and_five_hour_quota("claude-1", 96, 40));
        dashboard.apply_quota(weekly_and_five_hour_quota("codex-1", 100, 40));
        dashboard.apply_quota(weekly_quota("codex-2", 63));
        dashboard.cycle_pane_layout();

        let quota = drawn(&mut dashboard, 160, 44)
            .into_iter()
            .find(|line| line.contains("─ Quota ──"))
            .expect("the collapsed Quota row");
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
        dashboard.cycle_pane_layout();

        let mut terminal = Terminal::new(TestBackend::new(160, 44)).expect("terminal");
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw the collapsed panes");
        let buffer = terminal.backend().buffer();
        let lines = buffer_lines(buffer);
        let row = lines
            .iter()
            .position(|line| line.contains("─ Quota ──"))
            .expect("the collapsed Quota row");
        let colour_of = |needle: &str| {
            let column = cell_column(&lines[row], needle);
            buffer[(column, row as u16)].fg
        };

        assert_eq!(colour_of("96%/5%"), Color::Red);
        assert_eq!(colour_of("8%/90%"), Color::Red);
    }

    /// A collapsed pane is one row by definition, so more hosts than fit have
    /// to be cut rather than wrapped onto a second row.
    #[test]
    fn the_minimized_rows_truncate_rather_than_wrap() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.set_deployment_capacity_targets(
            (0..8)
                .map(|index| hel::hel_targets::DeploymentCapacityTarget {
                    id: format!("host-{index}"),
                    host: format!("a-rather-long-host-name-{index}"),
                    ..test_capacity_target()
                })
                .collect(),
        );
        dashboard.cycle_pane_layout();

        let lines = drawn(&mut dashboard, 60, 44);
        let rows = lines
            .iter()
            .filter(|line| line.contains("─ Targets ──"))
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), 1, "{lines:#?}");
        assert!(rows[0].chars().count() <= 60);
        assert!(
            rows[0].contains('…'),
            "the readings are cut rather than wrapped: {:?}",
            rows[0]
        );
        assert!(
            rows[0].ends_with('─'),
            "the collapsed pane keeps its rule: {:?}",
            rows[0]
        );
    }

    #[test]
    fn read_idle_session_uses_the_normal_summary_color() {
        let mut session = stopped_session();
        session.state = SessionState::Running;
        // The detach cursor sits past the only agent message, so nothing is
        // unread; the band still brightens because no turn is in flight.
        session.viewed_through_event_ordinal = 1;
        let mut dashboard = dashboard_with_session(session);
        dashboard.focus = Focus::Quota;
        let mut materialized =
            materialized_session_for("session-1", vec![agent_message(1, "seen response")]);
        materialized.execution = MaterializedExecutionState::Idle;
        dashboard.apply_materialized_session(&materialized);
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
        assert!(
            (buffer.area.x + 1..buffer.area.right() - 1)
                .filter(|x| summary_text_cell(&buffer[(*x, status_y)]))
                .all(|x| buffer[(x, status_y)].fg == Color::Yellow)
        );
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
        let mut dashboard = DashboardState::new(config(), HelState::default(), BTreeMap::new());
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

        assert!(rendered.contains("podman, mac-container"));
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
            hel::hel_config::TargetTemplate::LocalPodman { container } => container,
            _ => unreachable!(),
        };
        let ssh = |host: &str| hel::hel_config::SshConnection {
            host: host.into(),
            user: None,
            identity_file: None,
            extra_args: Vec::new(),
        };
        config.targets.insert(
            "precision-3260".into(),
            hel::hel_config::TargetTemplate::SshBare {
                ssh: ssh("precision-3260"),
                permissions: PermissionMode::Yolo,
                workspace_prefix: ".local/share/hel/workspaces".into(),
            },
        );
        config.targets.insert(
            "morannon-podman".into(),
            hel::hel_config::TargetTemplate::SshPodman {
                ssh: ssh("morannon"),
                container,
            },
        );
        config.targets.insert(
            "morannon-raw".into(),
            hel::hel_config::TargetTemplate::SshBare {
                ssh: ssh("morannon"),
                permissions: PermissionMode::Guardian,
                workspace_prefix: ".local/share/hel/workspaces".into(),
            },
        );
        let mut session = running_session();
        session.target_template_id = "precision-3260".into();
        session.project_directory = Some("/home/dev/hel".into());
        let state = HelState {
            version: STATE_VERSION,
            sessions: BTreeMap::from([(session.id.clone(), session)]),
            mount_history: BTreeMap::new(),
            container_sizes: BTreeMap::new(),
        };
        let mut dashboard = DashboardState::new(config, state, BTreeMap::new());
        let capacity_target =
            |host: &str, target_ids: &[&str]| hel::hel_targets::DeploymentCapacityTarget {
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
        let mut terminal = Terminal::new(TestBackend::new(140, 40)).expect("terminal");

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
        assert!(badge_has_color("[Y]", Color::Red), "{rendered}");
        assert!(badge_has_color("[G]", Color::Green), "{rendered}");
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
        let mut failed = DashboardState::new(config(), HelState::default(), BTreeMap::new());
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

        let mut aged = DashboardState::new(config(), HelState::default(), BTreeMap::new());
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
            let mut session = stopped_session();
            session.id = format!("active-{index:02}");
            session.state = SessionState::Running;
            sessions.insert(session.id.clone(), session);
        }
        for index in 0..20 {
            let mut session = stopped_session();
            session.id = format!("archived-{index:02}");
            sessions.insert(session.id.clone(), session);
        }
        let state = HelState {
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
        let backend = TestBackend::new(120, 36);
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

        let up = symbols.iter().filter(|symbol| **symbol == "▲").count();
        let down = symbols.iter().filter(|symbol| **symbol == "▼").count();
        assert!(up >= 1, "expected an upper arrow, rendered {up}");
        assert!(down >= 1, "expected a lower arrow, rendered {down}");
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

        assert!(!symbols.iter().any(|symbol| matches!(*symbol, "▲" | "▼")));
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
        let mut dashboard = DashboardState::new(config, HelState::default(), BTreeMap::new());
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

        assert!(symbols.contains(&"▲"));
        assert!(symbols.contains(&"▼"));
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

        let (clock, _, _, _, _) = session_values(&session, Some(&detail), None, 1_480, &config());
        assert_eq!(clock, "[idle]");
    }

    #[test]
    fn active_message_tail_uses_the_last_four_nonempty_lines() {
        let short = SessionDetail {
            last_agent_message: Some("one line".into()),
            ..SessionDetail::default()
        };
        assert_eq!(
            active_message_tail(Some(&short), 80, ACTIVE_MESSAGE_LINES).len(),
            1
        );

        let long = SessionDetail {
            last_agent_message: Some("one\ntwo\nthree\nfour\nfive".into()),
            ..SessionDetail::default()
        };
        let lines = active_message_tail(Some(&long), 80, ACTIVE_MESSAGE_LINES)
            .into_iter()
            .map(|line| {
                line.spans
                    .into_iter()
                    .map(|span| span.content.into_owned())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert_eq!(lines, ["two", "three", "four", "five"]);
        assert!(active_message_tail(None, 80, ACTIVE_MESSAGE_LINES).is_empty());
    }

    #[test]
    fn active_message_tail_removes_blank_lines_before_capping() {
        let detail = SessionDetail {
            last_agent_message: Some(
                "Fixed and pushed.\n\nDuplicate LinkedIn URLs now use last-write-wins behavior.\n\nCommit: b6cb3e8 Keep the last duplicate connection record".into(),
            ),
            ..SessionDetail::default()
        };

        let rendered = active_message_tail(Some(&detail), 80, ACTIVE_MESSAGE_LINES)
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(!rendered.contains("more]"));
        assert!(rendered.contains("Fixed and pushed."));
        assert!(rendered.contains("Commit: b6cb3e8"));
    }

    #[test]
    fn provisioning_clock_uses_elapsed_seconds_since_state_update() {
        let mut session = stopped_session();
        session.state = SessionState::Provisioning;
        session.updated_at = "1970-01-01T00:16:40Z".into();

        let (clock, _, _, _, _) = session_values(&session, None, None, 1_012, &config());
        assert_eq!(clock, "Launch 12s");
    }

    #[test]
    fn launch_clock_names_the_reported_stage() {
        let session = stopped_session();
        let operation = operation(
            SessionOperationKind::Launching,
            Some(ProvisionStage::Booting),
        );

        let (clock, _, _, _, _) =
            session_values(&session, None, Some(&operation), 1_012, &config());
        assert_eq!(clock, "Boot 12s");
    }

    #[test]
    fn launch_clock_falls_back_to_the_kind_label_without_a_stage() {
        let session = stopped_session();
        let operation = operation(SessionOperationKind::Launching, None);

        let (clock, _, _, _, _) =
            session_values(&session, None, Some(&operation), 1_012, &config());
        assert_eq!(clock, "Launch 12s");
    }

    #[test]
    fn a_stage_does_not_rename_a_non_launch_operation() {
        let session = stopped_session();
        let operation = operation(
            SessionOperationKind::Stopping,
            Some(ProvisionStage::Syncing),
        );

        let (clock, _, _, _, _) =
            session_values(&session, None, Some(&operation), 1_012, &config());
        assert_eq!(clock, "Stopping 12s");
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

        let (_, profile_id, target_template_id, _, _) =
            session_values(&session, None, Some(&resuming), 1_012, &config());

        assert_eq!(profile_id, "grok-1");
        assert_eq!(target_template_id, "localhost");
    }

    #[test]
    fn without_a_resume_destination_the_row_falls_back_to_the_session_record() {
        let session = stopped_session();
        let resuming = operation(SessionOperationKind::Resuming, None);

        let (_, profile_id, target_template_id, _, _) =
            session_values(&session, None, Some(&resuming), 1_012, &config());

        assert_eq!(profile_id, session.last_profile);
        assert_eq!(target_template_id, session.target_template_id);
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

        let (clock, _, _, _, _) =
            session_values(&session, None, Some(&operation), 1_052, &config());
        assert_eq!(clock, "Boot 12s");
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

        let (clock, _, _, _, _) =
            session_values(&session, None, Some(&operation), 1_012, &config());
        assert_eq!(clock, "Clone, Sync 10s");
    }

    #[test]
    fn active_target_and_project_are_separate_cells() {
        let mut config = config();
        config
            .bundles
            .get_mut("hel")
            .unwrap()
            .repositories
            .push(ProjectRepository {
                id: "anvil".into(),
                github: Some("BrokkAi/anvil".into()),
                local: None,
                destination: "anvil".into(),
                git_ref: None,
            });

        let (_, _, target, project, _) = session_values(&stopped_session(), None, None, 0, &config);
        assert_eq!(target, "podman");
        assert_eq!(project, "hel");
    }

    #[test]
    fn focused_panes_use_double_borders_without_focus_title_text() {
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
        assert!(rendered.contains("╔ Sessions"));
        assert!(rendered.contains("┌ Targets"));
        assert!(!rendered.contains("[focused]"));

        for (focus, doubled) in [(Focus::Quota, "╔ Quota"), (Focus::Targets, "╔ Targets")] {
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
            assert!(rendered.contains(doubled), "{focus:?}: {rendered:?}");
            assert!(!rendered.contains("[focused]"));
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
            HelState {
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
    fn empty_config_renders_onboarding_with_the_workspace_name() {
        let mut dashboard =
            DashboardState::new(HelConfig::default(), HelState::default(), BTreeMap::new());
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
            DashboardAction::OpenConfig
        );
    }

    #[test]
    fn workspace_name_does_not_change_with_dashboard_updates() {
        let mut dashboard = DashboardState::new(config(), HelState::default(), BTreeMap::new());
        dashboard.set_workspace_name("acme-workspace".into());
        dashboard.set_state(HelState::default());
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
        assert!(rendered.contains("acme-workspace"));
    }

    #[test]
    fn quota_render_includes_errors_and_refresh_age_in_title() {
        let mut dashboard = DashboardState::new(
            config(),
            HelState::default(),
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
        assert!(rendered.contains("unavailable: offline"));
        assert!(rendered.contains("Quota (refreshed"));
        assert!(!rendered.contains("Refreshed"));
        assert!(!rendered.contains("Access"));
        assert!(!rendered.contains("agent-full-access"));
    }

    #[test]
    fn quota_render_shows_login_expired_without_unavailable_prefix() {
        let mut dashboard = DashboardState::new(
            config(),
            HelState::default(),
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
        let mut dashboard = DashboardState::new(config, HelState::default(), BTreeMap::new());
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
            "███████▎░░  73%"
        );
        assert_eq!(bar.spans[0].style.fg, Some(Color::Green));
        assert_eq!(bar.spans[2].style.fg, Some(Color::DarkGray));
        assert_eq!(bar.spans[2].style.bg, None);
        assert!(quota_bar(None).spans.is_empty());
    }

    #[test]
    fn api_quota_label_is_punched_into_the_depleted_bar_shading() {
        let api = api_quota_bar();
        let exhausted = quota_bar(Some(&QuotaWindow {
            label: "Week".into(),
            remaining_percent: Some(0),
            used: None,
            limit: None,
            resets: None,
            resets_at_epoch_seconds: None,
        }));
        let shading = &exhausted.spans[2];
        assert_eq!(shading.content, "░░░░░░░░░░");

        let rendered: String = api.spans.iter().map(|span| span.content.as_ref()).collect();
        assert_eq!(rendered, "░░░API░░░░");
        // The padding must be the bar's own glyph and color, so the two match
        // whatever the terminal maps them to.
        for padding in [&api.spans[0], &api.spans[2]] {
            assert_eq!(padding.style.fg, shading.style.fg);
            assert_eq!(padding.style.bg, shading.style.bg);
        }
        // A painted background would not match a glyph-shaded cell.
        assert!(api.spans.iter().all(|span| span.style.bg.is_none()));
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
            HelState::default(),
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

        assert_eq!(quota_reset_cells(&quota, 0), ("7d".into(), "4h".into()));
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
            HelState::default(),
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
        let weekly_percent = cell_column(row, "73%");
        let weekly_reset = cell_column(row, "2d");
        let five_hour_percent = cell_column(row, "70%");
        let five_hour_reset = cell_column(row, "1h5m");
        assert_eq!(weekly_reset, weekly_percent + 3 + 2);
        assert_eq!(five_hour_percent - 12, weekly_reset + 6 + 2);
        assert_eq!(five_hour_reset, five_hour_percent + 3 + 2);
    }
}
