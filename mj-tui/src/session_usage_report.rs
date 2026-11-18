//! Profiles' cancellable, read-only API cost comparison.
use crate::resume::DisplayZone;
use crate::{
    DashboardAction, DashboardState, Mode,
    widgets::{Truncate, centered_modal, dismissible_modal_title, truncate_to_cells},
};
use crossterm::event::Event;
use mj_chat::components::PopupSide;
use mj_chat::{
    components::{Checkbox, ChoiceList, ComboBox, ComboBoxState, ControlKind, Dialog, Interaction},
    selection::FrameSurfaces,
    theme,
};
use mj_core::usage::{
    CostUsdTicks, HOUR_MS, HourlyUsage, SessionUsageQuery, SessionUsageReport, UsageAgentRole,
};
use ratatui::{
    Frame,
    layout::{Margin, Rect},
    text::{Line, Span},
    widgets::Paragraph,
};
use std::cell::{Cell, RefCell};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UsageControl {
    Profile,
    Suspended,
    /// The hourly bar graph; its columns are the selectable hours.
    Hours,
    Rows,
    Refresh,
    Close,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum UsageRow {
    Role(UsageAgentRole),
    ModelTotal(UsageAgentRole, Option<String>),
    Session(String),
    Model(String, Option<String>),
    Coverage(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionUsageDialog {
    pub(crate) generation: u64,
    pub(crate) query: SessionUsageQuery,
    pub(crate) report: Option<Result<SessionUsageReport, String>>,
    pub(crate) profiles: Vec<String>,
    /// The latest bars, kept while a new read is in flight so the graph the
    /// user clicked stays on screen.
    hours: Vec<HourlyUsage>,
    /// How many of the oldest `hours` the last frame had no room to draw.
    hidden_hours: Cell<usize>,
    selected_key: RefCell<Option<UsageRow>>,
    row_keys: RefCell<Vec<UsageRow>>,
    selected_row: Cell<usize>,
    combo: ComboBoxState<UsageControl>,
    pub(crate) form: RefCell<Dialog<UsageControl>>,
}

impl DashboardState {
    pub(crate) fn begin_session_usage_report(&mut self, profile: Option<String>) {
        self.usage_request_generation = self.usage_request_generation.wrapping_add(1);
        let mut form = Dialog::default();
        form.focus(UsageControl::Profile);
        self.mode = Mode::SessionUsageReport(SessionUsageDialog {
            generation: self.usage_request_generation,
            query: SessionUsageQuery {
                profile,
                ..SessionUsageQuery::default()
            },
            report: None,
            profiles: self.config.profiles.keys().cloned().collect(),
            hours: Vec::new(),
            hidden_hours: Cell::new(0),
            selected_key: RefCell::new(None),
            row_keys: RefCell::new(Vec::new()),
            selected_row: Cell::new(0),
            combo: ComboBoxState::default(),
            form: RefCell::new(form),
        });
    }

    /// The terminal task owner uses this identity to cancel obsolete reads.
    pub fn session_usage_request(&self) -> Option<(u64, SessionUsageQuery)> {
        match &self.mode {
            Mode::SessionUsageReport(dialog) => Some((dialog.generation, dialog.query.clone())),
            _ => None,
        }
    }

    pub fn apply_session_usage_result(
        &mut self,
        generation: u64,
        result: Result<SessionUsageReport, String>,
    ) {
        if self.session_usage_request().map(|r| r.0) != Some(generation) {
            return;
        }
        if let Err(error) = &result {
            self.set_failure_notice(format!("Usage by session: {error}"));
        }
        if let Mode::SessionUsageReport(dialog) = &mut self.mode {
            if let Ok(report) = &result {
                dialog.hours = report.hours.clone();
                dialog.profiles = dialog
                    .profiles
                    .iter()
                    .chain(&report.profiles)
                    .cloned()
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .collect();
            }
            dialog.report = Some(result);
        }
    }

    pub(crate) fn handle_session_usage_report_event(
        &mut self,
        event: Event,
        mut dialog: SessionUsageDialog,
    ) -> DashboardAction {
        let result = dialog.form.get_mut().handle(&event);
        self.last_event_consumed.set(result.consumed);
        let action = dialog.combo.route(result.action);
        let mut reload = false;
        match action {
            Some(Interaction::Cancel | Interaction::Activate(UsageControl::Close)) => {
                self.cancel_modal();
                return DashboardAction::None;
            }
            Some(Interaction::Activate(UsageControl::Profile)) => {
                let index = dialog
                    .query
                    .profile
                    .as_ref()
                    .and_then(|p| dialog.profiles.iter().position(|v| v == p))
                    .map_or(0, |i| i + 1);
                dialog.combo.open(UsageControl::Profile, index);
            }
            Some(Interaction::ComboBoxCommit(UsageControl::Profile, index)) => {
                dialog.query.profile = index
                    .checked_sub(1)
                    .and_then(|i| dialog.profiles.get(i))
                    .cloned();
                reload = true;
            }
            // Choosing the hour that is already shown goes back to all hours.
            Some(Interaction::Select(UsageControl::Hours, index)) => {
                if let Some(bar) = dialog.hours.get(dialog.hidden_hours.get() + index) {
                    let hour = Some(bar.start_ms);
                    dialog.query.hour_start_ms = if dialog.query.hour_start_ms == hour {
                        None
                    } else {
                        hour
                    };
                    reload = true;
                }
            }
            Some(Interaction::Activate(UsageControl::Hours)) => {
                reload = dialog.query.hour_start_ms.take().is_some();
            }
            Some(Interaction::Toggle(UsageControl::Suspended)) => {
                dialog.query.include_suspended = !dialog.query.include_suspended;
                reload = true;
            }
            Some(Interaction::Activate(UsageControl::Refresh)) => reload = true,
            Some(Interaction::Select(UsageControl::Rows, index)) => {
                *dialog.selected_key.borrow_mut() = dialog.row_keys.borrow().get(index).cloned();
                dialog.selected_row.set(index);
            }
            _ => {}
        }
        if reload {
            self.usage_request_generation = self.usage_request_generation.wrapping_add(1);
            dialog.generation = self.usage_request_generation;
            dialog.report = None;
        }
        self.mode = Mode::SessionUsageReport(dialog);
        DashboardAction::None
    }
}

/// A cost, or `?` when it cannot be known. `+` marks a known cost that
/// running or unreported turns may add to.
fn dollars(cost: CostUsdTicks, partial: bool) -> String {
    let Ok(ticks) = cost else {
        return "?".into();
    };
    let plus = if partial { "+" } else { "" };
    if ticks > 0 && ticks < 10_000_000 {
        return format!("<$0.001{plus}");
    }
    format!("${:.3}{plus}", ticks as f64 / 10_000_000_000.0)
}

pub(crate) fn render(
    frame: &mut Frame,
    area: Rect,
    _dashboard: &DashboardState,
    dialog: &SessionUsageDialog,
    surfaces: &mut FrameSurfaces,
) {
    let popup = centered_modal(frame, surfaces, 92, 32, area);
    let inner = popup.inner(Margin {
        horizontal: 1,
        vertical: 1,
    });
    let mut form = dialog.form.borrow_mut();
    form.begin_frame();
    let title = dismissible_modal_title(
        &mut form,
        popup,
        "Usage by session",
        theme::title(true),
        true,
    );
    frame.render_widget(theme::modal().title(title), popup);
    if inner.height < 14 {
        form.end_frame(UsageControl::Close);
        return;
    }
    let selected_profile = dialog
        .query
        .profile
        .as_ref()
        .and_then(|p| dialog.profiles.iter().position(|v| v == p))
        .map_or(0, |i| i + 1);
    form.declare(
        UsageControl::Profile,
        ControlKind::ComboBox {
            len: dialog.profiles.len() + 1,
            selected: dialog
                .combo
                .selection(UsageControl::Profile, selected_profile),
            expanded: dialog.combo.is_open(UsageControl::Profile),
        },
    );
    frame.render_widget(
        Paragraph::new("Profile"),
        Rect::new(inner.x, inner.y, 8.min(inner.width), 1),
    );
    Checkbox::render(
        frame,
        Rect::new(inner.x, inner.y + 2, inner.width, 1),
        "Include suspended sessions",
        dialog.query.include_suspended,
        true,
        &mut form,
        UsageControl::Suspended,
    );
    let window = dialog.query.hour_start_ms.map_or_else(
        || "last 24 hours".to_owned(),
        |ms| {
            let zone = |ms| {
                chrono::DateTime::from_timestamp_millis(ms)
                    .unwrap_or_default()
                    .with_timezone(&DisplayZone::now().timezone())
            };
            format!(
                "{}–{}",
                zone(ms).format("%a %b %-d %H:%M"),
                zone(ms + HOUR_MS).format("%H:%M")
            )
        },
    );
    let bar_rows = if inner.height >= 24 { 5 } else { 2 };
    let chart_y = inner.y + 5;
    render_hour_chart(
        frame,
        Rect::new(inner.x, chart_y, inner.width, bar_rows + 1),
        dialog,
        bar_rows,
        &mut form,
    );
    let rows_y = chart_y + bar_rows + 2;
    let rows_area = Rect::new(
        inner.x,
        rows_y,
        inner.width,
        inner.bottom().saturating_sub(rows_y + 3),
    );
    match &dialog.report {
        None => frame.render_widget(
            Paragraph::new("Loading usage…").style(theme::muted()),
            rows_area,
        ),
        Some(Err(error)) => frame.render_widget(
            Paragraph::new(format!("Usage load failed: {error}\nUse Refresh to retry."))
                .wrap(ratatui::widgets::Wrap { trim: false })
                .style(theme::muted()),
            rows_area,
        ),
        Some(Ok(report)) => {
            let partial = report.sessions.iter().any(|s| s.is_partial());
            frame.render_widget(
                Paragraph::new(format!(
                    "Total {} · {window} · estimated API USD",
                    dollars(report.cost_usd_ticks, partial),
                )),
                Rect::new(inner.x, inner.y + 4, inner.width, 1),
            );
            let mut lines = Vec::new();
            let mut row_map = Vec::new();
            let mut row_keys = dialog.row_keys.borrow_mut();
            row_keys.clear();
            if !report.sessions.is_empty() {
                for agents in &report.agent_usage {
                    lines.push(Line::styled(
                        format!(
                            "{}  {}",
                            agents.role.label(),
                            dollars(agents.cost_usd_ticks, partial)
                        ),
                        theme::title(false),
                    ));
                    row_map.push(Some(row_keys.len()));
                    row_keys.push(UsageRow::Role(agents.role));
                    for model in &agents.models {
                        let cost = dollars(model.cost_usd_ticks, partial);
                        let name = truncate_to_cells(
                            model.model.as_deref().unwrap_or("Unknown model"),
                            usize::from(inner.width).saturating_sub(cost.len() + 4),
                            Truncate::PLAIN,
                        );
                        lines.push(Line::raw(format!("  {name}  {cost}")));
                        row_map.push(Some(row_keys.len()));
                        row_keys.push(UsageRow::ModelTotal(agents.role, model.model.clone()));
                    }
                    lines.push(Line::default());
                    row_map.push(None);
                }
            }
            for session in &report.sessions {
                let cost = format!(
                    "{}{}",
                    dollars(session.cost_usd_ticks, session.is_partial()),
                    if session.suspended {
                        " · suspended"
                    } else {
                        ""
                    }
                );
                let name = format!("{}  [{}]", session.title, session.profile);
                let name = truncate_to_cells(
                    &name,
                    usize::from(inner.width).saturating_sub(cost.len() + 2),
                    Truncate::SUMMARY,
                );
                lines.push(Line::from(vec![
                    Span::styled(name, theme::title(false)),
                    Span::raw(format!("  {cost}")),
                ]));
                row_map.push(Some(row_keys.len()));
                row_keys.push(UsageRow::Session(session.session_id.clone()));
                for model in &session.models {
                    let tokens = model.totals.get("total_tokens").map_or_else(
                        || "tokens unknown".to_owned(),
                        |v| format!("{} tokens", v.tokens),
                    );
                    let detail = format!("{}  {tokens}", dollars(model.cost_usd_ticks, false));
                    let name = truncate_to_cells(
                        model.model.as_deref().unwrap_or("Unknown model"),
                        usize::from(inner.width).saturating_sub(detail.len() + 6),
                        Truncate::PLAIN,
                    );
                    lines.push(Line::raw(format!("    {name}  {detail}",)));
                    row_map.push(Some(row_keys.len()));
                    row_keys.push(UsageRow::Model(
                        session.session_id.clone(),
                        model.model.clone(),
                    ));
                }
                let coverage = &session.coverage;
                lines.push(Line::styled(
                    format!(
                        "    {} sub-agents · {}/{} turns reported{}",
                        session.subagent_count,
                        coverage.full_turn_reports,
                        coverage.recorded_turns,
                        if coverage.unfinished_turns > 0 {
                            format!(" · {} unfinished", coverage.unfinished_turns)
                        } else {
                            String::new()
                        }
                    ),
                    theme::muted(),
                ));
                row_map.push(Some(row_keys.len()));
                row_keys.push(UsageRow::Coverage(session.session_id.clone()));
                lines.push(Line::default());
                row_map.push(None);
            }
            let selected = dialog
                .selected_key
                .borrow()
                .as_ref()
                .and_then(|key| row_keys.iter().position(|row| row == key))
                .unwrap_or_else(|| {
                    dialog
                        .selected_row
                        .get()
                        .min(row_keys.len().saturating_sub(1))
                });
            dialog.selected_row.set(selected);
            *dialog.selected_key.borrow_mut() = row_keys.get(selected).cloned();
            ChoiceList::render_with_rows(
                frame,
                rows_area,
                &lines,
                selected,
                &row_map,
                &[],
                &mut form,
                UsageControl::Rows,
            );
            if lines.is_empty() {
                frame.render_widget(
                    Paragraph::new("No sessions match these filters.").style(theme::muted()),
                    rows_area,
                );
            }
        }
    }
    for (row, hint) in [
        "Click a bar or ←/→ for an hour, Enter for all 24 · ░ unpriced models",
        "Sub-agents included · profile filters turns · + partial · base rates",
    ]
    .into_iter()
    .enumerate()
    {
        frame.render_widget(
            Paragraph::new(hint).style(theme::muted()),
            Rect::new(inner.x, inner.bottom() - 3 + row as u16, inner.width, 1),
        );
    }
    Dialog::render_actions(
        frame,
        Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
        &[
            (UsageControl::Refresh, "Refresh", true),
            (UsageControl::Close, "Close", true),
        ],
        &mut form,
    );
    // Dropdown popups draw last so they own the cells and pointer surface beneath them.
    let profile_options: Vec<Line<'_>> = std::iter::once(Line::raw("All"))
        .chain(dialog.profiles.iter().cloned().map(Line::raw))
        .collect();
    ComboBox::render(
        frame,
        area,
        Rect::new(
            inner.x + 8,
            inner.y,
            inner.width.saturating_sub(8).min(40),
            1,
        ),
        dialog.query.profile.as_deref().unwrap_or("All"),
        &profile_options,
        dialog
            .combo
            .selection(UsageControl::Profile, selected_profile),
        dialog.combo.is_open(UsageControl::Profile),
        true,
        "Profile",
        PopupSide::Below,
        ComboBox::DEFAULT_MAX_VISIBLE_ROWS,
        &mut form,
        UsageControl::Profile,
    );
    form.end_frame(UsageControl::Close);
}

/// The graph's top value: the next 1, 2, or 5 times a power of ten above the
/// tallest bar, in dollars, so the axis labels read cleanly.
fn axis_top_dollars(max_ticks: u64) -> f64 {
    let dollars = max_ticks as f64 / 10_000_000_000.0;
    if dollars <= 0.0 {
        return 1.0;
    }
    let magnitude = 10f64.powf(dollars.log10().floor());
    [1.0, 2.0, 5.0, 10.0]
        .into_iter()
        .map(|step| step * magnitude)
        .find(|top| *top >= dollars * 0.999_999)
        .unwrap_or(10.0 * magnitude)
}

fn axis_label(dollars: f64) -> String {
    if dollars <= 0.0 {
        return "$0".to_owned();
    }
    // Keep two significant digits below a cent so tiny costs do not read $0.
    let places = ((-dollars.log10()).ceil().max(0.0) as usize + 1).clamp(3, 12);
    let text = format!("{dollars:.places$}");
    format!("${}", text.trim_end_matches('0').trim_end_matches('.'))
}

/// One bar per hour in the report, tallest at the axis top. Each is a column
/// of the `Hours` control, so a click or arrow selects that hour.
fn render_hour_chart(
    frame: &mut Frame,
    area: Rect,
    dialog: &SessionUsageDialog,
    bar_rows: u16,
    form: &mut Dialog<UsageControl>,
) {
    const BLOCKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let max_ticks = dialog
        .hours
        .iter()
        .map(|hour| hour.known_cost_usd_ticks)
        .max()
        .unwrap_or(0);
    let top = axis_top_dollars(max_ticks);
    let labels = [axis_label(top), axis_label(top / 2.0), "$0".to_owned()];
    let gutter = labels.iter().map(String::len).max().unwrap_or(0) as u16 + 1;
    let available = area.width.saturating_sub(gutter);
    // Draw the newest hours that fit. Bars have a one-cell gap when there is
    // room; at 24 columns they touch, and only below that are hours dropped.
    let pitch = (available / dialog.hours.len().max(1) as u16).max(1);
    let shown = dialog.hours.len().min(usize::from(available / pitch));
    let hidden = dialog.hours.len() - shown;
    dialog.hidden_hours.set(hidden);
    let hours = &dialog.hours[hidden..];
    let selected = hours
        .iter()
        .position(|hour| Some(hour.start_ms) == dialog.query.hour_start_ms);
    let cursor = selected.unwrap_or(hours.len().saturating_sub(1));
    let focused = form.is_focused(UsageControl::Hours);
    let eighths = |hour: &HourlyUsage| {
        if hour.known_cost_usd_ticks == 0 {
            // Usage with no known price still marks the hour.
            return u16::from(hour.has_unknown_cost);
        }
        let full = f64::from(bar_rows) * 8.0;
        let scaled = hour.known_cost_usd_ticks as f64 / 10_000_000_000.0 / top * full;
        (scaled.round() as u16).clamp(1, bar_rows * 8)
    };
    let bar_width = usize::from(pitch.saturating_sub(1).max(1));
    let mut lines = Vec::new();
    for row in 0..bar_rows {
        let axis = match row {
            0 => 0,
            r if r == bar_rows - 1 => 2,
            r if r == bar_rows / 2 => 1,
            _ => usize::MAX,
        };
        let mut spans = vec![Span::styled(
            format!(
                "{:>width$} ",
                labels.get(axis).map_or("", String::as_str),
                width = usize::from(gutter) - 1
            ),
            theme::muted(),
        )];
        let level_below = (bar_rows - 1 - row) * 8;
        for (index, hour) in hours.iter().enumerate() {
            let level = eighths(hour).saturating_sub(level_below).min(8);
            let glyph = match (level, hour.has_unknown_cost) {
                (0, _) => ' ',
                (_, true) => '░',
                (level, false) => BLOCKS[usize::from(level) - 1],
            };
            let style = if selected == Some(index) {
                theme::active_control()
            } else {
                theme::actionable()
            };
            spans.push(Span::styled(glyph.to_string().repeat(bar_width), style));
            if pitch > 1 {
                spans.push(Span::raw(" "));
            }
        }
        lines.push(Line::from(spans));
    }
    // A two-digit hour needs two cells; with one-cell bars, label every other bar.
    let stride = if bar_width >= 2 {
        1
    } else {
        3usize.div_ceil(usize::from(pitch))
    };
    let label_width = usize::from(pitch) * stride;
    let mut label_row = vec![Span::raw(" ".repeat(usize::from(gutter)))];
    for index in (0..hours.len()).step_by(stride) {
        let local = chrono::DateTime::from_timestamp_millis(hours[index].start_ms)
            .unwrap_or_default()
            .with_timezone(&DisplayZone::now().timezone());
        let covers =
            |bar: Option<usize>| bar.is_some_and(|bar| (index..index + stride).contains(&bar));
        let mut style = if covers(selected) {
            theme::active_control()
        } else {
            theme::muted()
        };
        if focused && covers(Some(cursor)) {
            style = style.add_modifier(ratatui::style::Modifier::UNDERLINED);
        }
        label_row.push(Span::styled(
            format!("{:<label_width$}", local.format("%H")),
            style,
        ));
    }
    lines.push(Line::from(label_row));
    frame.render_widget(Paragraph::new(lines), area);
    let columns = (0..shown)
        .map(|index| {
            let start = area.x + gutter + index as u16 * pitch;
            (start, start + pitch)
        })
        .collect::<Vec<_>>();
    form.register_columns(
        UsageControl::Hours,
        Rect::new(area.x + gutter, area.y, shown as u16 * pitch, area.height),
        cursor,
        &columns,
        shown > 0,
    );
}
