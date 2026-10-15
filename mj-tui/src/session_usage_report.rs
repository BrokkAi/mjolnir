//! Profiles' cancellable, read-only API cost comparison.
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
use mj_core::usage::{SessionUsageQuery, SessionUsageReport, UsagePeriod};
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
    Period,
    Suspended,
    Rows,
    Refresh,
    Close,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum UsageRow {
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
    selected_key: RefCell<Option<UsageRow>>,
    row_keys: RefCell<Vec<UsageRow>>,
    selected_row: Cell<usize>,
    combo: ComboBoxState<UsageControl>,
    pub(crate) form: RefCell<Dialog<UsageControl>>,
}

impl DashboardState {
    pub(crate) fn begin_session_usage_report(&mut self) {
        self.usage_request_generation = self.usage_request_generation.wrapping_add(1);
        let mut form = Dialog::default();
        form.focus(UsageControl::Profile);
        self.mode = Mode::SessionUsageReport(SessionUsageDialog {
            generation: self.usage_request_generation,
            query: SessionUsageQuery::default(),
            report: None,
            profiles: self.config.profiles.keys().cloned().collect(),
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
            Some(Interaction::Activate(UsageControl::Period)) => dialog
                .combo
                .open(UsageControl::Period, dialog.query.period as usize),
            Some(Interaction::ComboBoxCommit(UsageControl::Profile, index)) => {
                dialog.query.profile = index
                    .checked_sub(1)
                    .and_then(|i| dialog.profiles.get(i))
                    .cloned();
                reload = true;
            }
            Some(Interaction::ComboBoxCommit(UsageControl::Period, index)) => {
                if let Some(period) = UsagePeriod::ALL.get(index) {
                    dialog.query.period = *period;
                    reload = true;
                }
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

fn dollars(ticks: u64) -> String {
    if ticks > 0 && ticks < 10_000_000 {
        return "<$0.001".into();
    }
    format!("${:.3}", ticks as f64 / 10_000_000_000.0)
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
    if inner.height < 9 {
        form.end_frame(UsageControl::Close);
        return;
    }
    let selected_profile = dialog
        .query
        .profile
        .as_ref()
        .and_then(|p| dialog.profiles.iter().position(|v| v == p))
        .map_or(0, |i| i + 1);
    // Declare in reading order; the expanded dropdowns paint last.
    for (control, len, selected) in [
        (
            UsageControl::Profile,
            dialog.profiles.len() + 1,
            selected_profile,
        ),
        (
            UsageControl::Period,
            UsagePeriod::ALL.len(),
            dialog.query.period as usize,
        ),
    ] {
        form.declare(
            control,
            ControlKind::ComboBox {
                len,
                selected: dialog.combo.selection(control, selected),
                expanded: dialog.combo.is_open(control),
            },
        );
    }
    let profile_width = inner.width.saturating_sub(22);
    frame.render_widget(
        Paragraph::new("Profile"),
        Rect::new(inner.x, inner.y, 8.min(inner.width), 1),
    );
    frame.render_widget(
        Paragraph::new("Period"),
        Rect::new(inner.x + profile_width, inner.y, 7, 1),
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
    let window = match dialog.query.period {
        UsagePeriod::Hourly => "last hour",
        UsagePeriod::Daily => "last 24 hours",
        UsagePeriod::Weekly => "last 7 days",
        UsagePeriod::Monthly => "last 30 days",
    };
    let total_area = Rect::new(inner.x, inner.y + 4, inner.width, 1);
    let rows_area = Rect::new(inner.x, inner.y + 6, inner.width, inner.height - 9);
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
                    "Total {}{} · {window} · estimated API USD",
                    dollars(report.cost_usd_ticks),
                    if partial { "+" } else { "" }
                )),
                total_area,
            );
            let mut lines = Vec::new();
            let mut row_map = Vec::new();
            let mut row_keys = dialog.row_keys.borrow_mut();
            row_keys.clear();
            for session in &report.sessions {
                let cost = format!(
                    "{}{}{}",
                    dollars(session.cost_usd_ticks),
                    if session.is_partial() { "+" } else { "" },
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
                    let detail = format!(
                        "{}  {tokens}",
                        model
                            .cost_usd_ticks
                            .map_or_else(|| "unpriced".into(), dollars)
                    );
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
    frame.render_widget(
        Paragraph::new("Parent profile · full sub-agent tree · + partial · standard base rates")
            .style(theme::muted()),
        Rect::new(inner.x, inner.bottom() - 2, inner.width, 1),
    );
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
        Rect::new(inner.x + 8, inner.y, profile_width.saturating_sub(10), 1),
        dialog.query.profile.as_deref().unwrap_or("All"),
        &profile_options,
        dialog
            .combo
            .selection(UsageControl::Profile, selected_profile),
        dialog.combo.is_open(UsageControl::Profile),
        true,
        "Profile",
        PopupSide::Below,
        &mut form,
        UsageControl::Profile,
    );
    let periods = UsagePeriod::ALL.map(|p| Line::raw(p.label()));
    ComboBox::render(
        frame,
        area,
        Rect::new(
            inner.x + profile_width + 7,
            inner.y,
            15.min(inner.width.saturating_sub(profile_width + 7)),
            1,
        ),
        dialog.query.period.label(),
        &periods,
        dialog
            .combo
            .selection(UsageControl::Period, dialog.query.period as usize),
        dialog.combo.is_open(UsageControl::Period),
        true,
        "Period",
        PopupSide::Below,
        &mut form,
        UsageControl::Period,
    );
    form.end_frame(UsageControl::Close);
}
