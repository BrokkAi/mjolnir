//! Profiles' cancellable, read-only API cost comparison.
use crate::resume::DisplayZone;
use crate::{
    DashboardAction, DashboardState, Mode,
    widgets::{Truncate, centered_modal, dismissible_modal_title, truncate_to_cells},
};
use crossterm::event::{Event, KeyCode, KeyModifiers, MouseButton, MouseEventKind};
use mj_chat::components::PopupSide;
use mj_chat::{
    components::{Checkbox, ChoiceList, ComboBox, ComboBoxState, ControlKind, Dialog, Interaction},
    selection::FrameSurfaces,
    theme,
};
use mj_core::usage::{
    CostUsdTicks, HOUR_MS, HourlyUsage, SessionUsageQuery, SessionUsageReport, UsageAgentRole,
    hour_start_ms,
};
use ratatui::{
    Frame,
    layout::{Margin, Rect},
    text::{Line, Span},
    widgets::Paragraph,
};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UsageControl {
    Profile,
    Suspended,
    /// The hourly bar graph; its columns are the selectable hours.
    Hours,
    Changes,
    Providers,
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
    /// The latest bars, kept while a new read is in flight so the graph the
    /// user clicked stays on screen.
    hours: Vec<HourlyUsage>,
    /// How many of the oldest `hours` the last frame had no room to draw.
    hidden_hours: Cell<usize>,
    hour_anchor: Option<i64>,
    hour_cursor: Option<i64>,
    /// The range a left-button drag covers right now, drawn but never queried
    /// until the button release commits it.
    hour_drag_preview: Option<BTreeSet<i64>>,
    hour_geometry: Cell<(Rect, u16)>,
    hour_press: Option<(usize, KeyModifiers, bool)>,
    expanded_providers: BTreeSet<String>,
    provider_keys: RefCell<Vec<String>>,
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
            hour_anchor: None,
            hour_cursor: None,
            hour_drag_preview: None,
            hour_geometry: Cell::new((Rect::default(), 1)),
            hour_press: None,
            expanded_providers: BTreeSet::new(),
            provider_keys: RefCell::new(Vec::new()),
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
            self.set_failure_notice(format!("Usage Dashboard: {error}"));
        }
        if let Mode::SessionUsageReport(dialog) = &mut self.mode {
            if let Ok(report) = &result {
                dialog.hours = report.hours.clone();
                let graph_hours: BTreeSet<_> =
                    report.hours.iter().map(|hour| hour.start_ms).collect();
                if dialog
                    .hour_anchor
                    .is_some_and(|hour| !graph_hours.contains(&hour))
                {
                    dialog.hour_anchor = None;
                }
                if dialog
                    .hour_cursor
                    .is_some_and(|hour| !graph_hours.contains(&hour))
                {
                    dialog.hour_cursor = None;
                }
                let visible_hours =
                    &dialog.hours[dialog.hidden_hours.get().min(dialog.hours.len())..];
                let cursor = visible_hours
                    .iter()
                    .position(|hour| Some(hour.start_ms) == dialog.hour_cursor)
                    .unwrap_or(visible_hours.len().saturating_sub(1));
                dialog
                    .form
                    .get_mut()
                    .set_selected(UsageControl::Hours, cursor);
                dialog.profiles = dialog
                    .profiles
                    .iter()
                    .chain(&report.profiles)
                    .cloned()
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .collect();
                if let Some(selected) = &mut dialog.query.hour_starts_ms {
                    let before = selected.len();
                    selected.retain(|hour| graph_hours.contains(hour));
                    if selected.len() != before {
                        self.usage_request_generation =
                            self.usage_request_generation.wrapping_add(1);
                        dialog.generation = self.usage_request_generation;
                        dialog.report = None;
                        return;
                    }
                }
            }
            dialog.report = Some(result);
        }
    }

    pub(crate) fn handle_session_usage_report_event(
        &mut self,
        event: Event,
        mut dialog: SessionUsageDialog,
    ) -> DashboardAction {
        let mut modifiers = match event {
            Event::Mouse(mouse) => Some(mouse.modifiers),
            Event::Key(key) => Some(key.modifiers),
            _ => None,
        };
        let mut dragged = false;
        if let Event::Mouse(mouse) = event {
            let (rect, pitch) = dialog.hour_geometry.get();
            let index = rect
                .contains((mouse.column, mouse.row).into())
                .then(|| usize::from((mouse.column - rect.x) / pitch));
            match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) if index.is_some() => {
                    dialog.hour_press = Some((index.unwrap_or(0), mouse.modifiers, false));
                }
                MouseEventKind::Drag(MouseButton::Left) => {
                    if let (Some((start, _, moved)), Some(end)) = (&mut dialog.hour_press, index) {
                        *moved |= *start != end;
                        if *moved {
                            let hidden = dialog.hidden_hours.get();
                            let a = *start + hidden;
                            let b = end + hidden;
                            // A drag previews its range without touching the
                            // query: the report stays put until the release
                            // commits, and a release outside the chart commits
                            // nothing.
                            dialog.hour_drag_preview = Some(
                                dialog
                                    .hours
                                    .iter()
                                    .enumerate()
                                    .filter_map(|(i, h)| {
                                        (a.min(b)..=a.max(b)).contains(&i).then_some(h.start_ms)
                                    })
                                    .collect(),
                            );
                        }
                    }
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    dialog.hour_drag_preview = None;
                    if let Some((start, down_modifiers, moved)) = dialog.hour_press.take() {
                        modifiers = Some(mouse.modifiers | down_modifiers);
                        if moved && let Some(end) = index {
                            let hidden = dialog.hidden_hours.get();
                            let a = start + hidden;
                            let b = end + hidden;
                            dialog.query.hour_starts_ms = Some(
                                dialog
                                    .hours
                                    .iter()
                                    .enumerate()
                                    .filter_map(|(i, h)| {
                                        (a.min(b)..=a.max(b)).contains(&i).then_some(h.start_ms)
                                    })
                                    .collect(),
                            );
                            dialog.hour_anchor = dialog.hours.get(a).map(|h| h.start_ms);
                            dialog.hour_cursor = dialog.hours.get(b).map(|h| h.start_ms);
                            dialog.query.hour_start_ms = None;
                            dragged = true;
                        }
                    }
                }
                _ => {}
            }
        }
        let hours_focused = dialog.form.get_mut().is_focused(UsageControl::Hours);
        let providers_focused = dialog.form.get_mut().is_focused(UsageControl::Providers);
        let result = dialog.form.get_mut().handle(&event);
        self.last_event_consumed.set(result.consumed);
        let mut action = dialog.combo.route(result.action);
        if let Event::Key(key) = event
            && key.modifiers == KeyModifiers::SHIFT
            && matches!(key.code, KeyCode::Left | KeyCode::Right)
            && hours_focused
        {
            let count = dialog.hours.len().saturating_sub(dialog.hidden_hours.get());
            let current = dialog.hours[dialog.hidden_hours.get().min(dialog.hours.len())..]
                .iter()
                .position(|hour| Some(hour.start_ms) == dialog.hour_cursor)
                .unwrap_or(count.saturating_sub(1));
            if dialog.hour_anchor.is_none() {
                dialog.hour_anchor = dialog
                    .hours
                    .get(dialog.hidden_hours.get() + current)
                    .map(|hour| hour.start_ms);
            }
            let next = if key.code == KeyCode::Left {
                current.saturating_sub(1)
            } else {
                (current + 1).min(count.saturating_sub(1))
            };
            dialog
                .form
                .get_mut()
                .set_selected(UsageControl::Hours, next);
            action = Some(Interaction::Select(UsageControl::Hours, next));
        }
        if let Event::Key(key) = event
            && key.modifiers.is_empty()
            && matches!(
                key.code,
                KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')
            )
            && providers_focused
        {
            dialog.form.get_mut().focus(UsageControl::Providers);
            action = Some(Interaction::Activate(UsageControl::Providers));
        }
        if dragged {
            action = None;
        }
        let mut reload = dragged;
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
            Some(Interaction::Select(UsageControl::Hours, index)) => {
                if let Some(bar) = dialog.hours.get(dialog.hidden_hours.get() + index) {
                    let hour = bar.start_ms;
                    let modifiers = modifiers.unwrap_or(KeyModifiers::NONE);
                    let shift = modifiers.contains(KeyModifiers::SHIFT);
                    let ctrl = modifiers.contains(KeyModifiers::CONTROL);
                    let selected = if shift {
                        let anchor = *dialog.hour_anchor.get_or_insert(hour);
                        let range = dialog.hours.iter().filter_map(|bar| {
                            (anchor.min(hour)..=anchor.max(hour))
                                .contains(&bar.start_ms)
                                .then_some(bar.start_ms)
                        });
                        let mut selected = if ctrl {
                            dialog.query.hour_starts_ms.clone().unwrap_or_else(|| {
                                dialog.hours.iter().map(|bar| bar.start_ms).collect()
                            })
                        } else {
                            BTreeSet::new()
                        };
                        selected.extend(range);
                        selected
                    } else if ctrl {
                        dialog.hour_anchor = Some(hour);
                        let mut selected =
                            dialog.query.hour_starts_ms.clone().unwrap_or_else(|| {
                                dialog.hours.iter().map(|bar| bar.start_ms).collect()
                            });
                        if !selected.remove(&hour) {
                            selected.insert(hour);
                        }
                        selected
                    } else {
                        dialog.hour_anchor = Some(hour);
                        BTreeSet::from([hour])
                    };
                    dialog.hour_cursor = Some(hour);
                    dialog.query.hour_start_ms = None;
                    dialog.query.hour_starts_ms = Some(selected);
                    reload = true;
                }
            }
            Some(Interaction::Activate(UsageControl::Hours)) => {
                let had_selection = dialog.query.hour_starts_ms.take().is_some();
                let had_legacy_hour = dialog.query.hour_start_ms.take().is_some();
                reload = had_selection || had_legacy_hour;
                dialog.hour_anchor = None;
                dialog.hour_cursor = None;
                dialog.form.get_mut().set_selected(
                    UsageControl::Hours,
                    dialog
                        .hours
                        .len()
                        .saturating_sub(dialog.hidden_hours.get() + 1),
                );
            }
            Some(
                Interaction::Select(UsageControl::Changes, _)
                | Interaction::Activate(UsageControl::Changes),
            ) => {
                let index = dialog
                    .form
                    .get_mut()
                    .selected(UsageControl::Changes)
                    .unwrap_or(0);
                if let Some(change) = dialog
                    .report
                    .as_ref()
                    .and_then(|r| r.as_ref().ok())
                    .and_then(|r| r.role_changes.get(index))
                {
                    let start = hour_start_ms(change.changed_at_ms);
                    dialog.query.hour_start_ms = None;
                    dialog.query.hour_starts_ms = Some(
                        dialog
                            .hours
                            .iter()
                            .filter(|hour| hour.start_ms >= start)
                            .map(|hour| hour.start_ms)
                            .collect(),
                    );
                    dialog.hour_anchor = Some(start);
                    dialog.hour_cursor = dialog.hours.last().map(|hour| hour.start_ms);
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
            Some(Interaction::Select(UsageControl::Providers, index)) => {
                if matches!(event, Event::Mouse(_))
                    && let Some(key) = dialog.provider_keys.borrow().get(index).cloned()
                    && !dialog.expanded_providers.remove(&key)
                {
                    dialog.expanded_providers.insert(key);
                }
            }
            Some(Interaction::Activate(UsageControl::Providers)) => {
                if let Some(index) = dialog.form.get_mut().selected(UsageControl::Providers)
                    && let Some(key) = dialog.provider_keys.borrow().get(index).cloned()
                    && !dialog.expanded_providers.remove(&key)
                {
                    dialog.expanded_providers.insert(key);
                }
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

#[derive(Default)]
struct Provider {
    profiles: BTreeSet<String>,
    quota: Vec<mj_core::usage::ProfileQuotaUsage>,
    cost: u64,
    unknown_cost: bool,
    partial: bool,
    roles: BTreeMap<UsageAgentRole, Role>,
}
#[derive(Default)]
struct Role {
    cost: u64,
    unknown_cost: bool,
    tokens: u64,
    models: BTreeMap<String, u64>,
    unreported: u64,
}

fn allocations(values: &[u64], total: u64) -> Vec<u64> {
    let sum: u128 = values.iter().map(|v| u128::from(*v)).sum();
    if sum == 0 {
        return vec![0; values.len()];
    }
    let mut out: Vec<u64> = values
        .iter()
        .map(|v| (u128::from(*v) * u128::from(total) / sum) as u64)
        .collect();
    let mut order: Vec<_> = (0..values.len()).collect();
    order.sort_by_key(|&i| {
        (
            std::cmp::Reverse(u128::from(values[i]) * u128::from(total) % sum),
            i,
        )
    });
    for &i in order
        .iter()
        .take(total.saturating_sub(out.iter().sum()) as usize)
    {
        out[i] += 1;
    }
    out
}

fn pace(quota: &[mj_core::usage::ProfileQuotaUsage], duration: u64) -> (Option<u64>, bool) {
    let equal = quota.iter().any(|q| q.monthly_price_usd.unwrap_or(0) == 0);
    let mut weight = 0.0;
    let mut sum = 0.0;
    let mut partial = false;
    for q in quota {
        let elapsed = q.covered_seconds.unwrap_or(duration);
        let Some(used) = q.consumed_basis_points.filter(|_| elapsed >= 60) else {
            partial = true;
            continue;
        };
        let w = if equal {
            1.0
        } else {
            f64::from(q.monthly_price_usd.unwrap_or(0))
        };
        weight += w;
        sum += w * used as f64 / 10_000.0 * 604_800.0 / elapsed as f64;
        partial |= q.partial || elapsed < duration;
    }
    (
        (weight > 0.0).then(|| (100.0 * sum / weight).round() as u64),
        partial,
    )
}

fn forecast(pace: u64, projections: &[(f64, i64, f64)], now: i64) -> String {
    if pace == 0 {
        return "idle".into();
    }
    if !projections.is_empty() {
        let total_weight: f64 = projections.iter().map(|p| p.2).sum();
        if total_weight > 0.0 {
            let mut accounts: Vec<_> = projections
                .iter()
                .map(|&(left, reset, weight)| {
                    (left * weight / total_weight, reset, weight / total_weight)
                })
                .collect();
            let rate = pace as f64 / 168.0;
            let end = now + 604_800;
            let mut t = now as f64;
            while t < end as f64 {
                for account in &mut accounts {
                    while account.1 as f64 <= t {
                        account.0 = 100.0 * account.2;
                        account.1 += 604_800;
                    }
                }
                let next = accounts.iter().map(|a| a.1).min().unwrap_or(end).min(end) as f64;
                let mut hours = (next - t) / 3600.0;
                while hours > 0.0 {
                    let active_weight: f64 =
                        accounts.iter().filter(|a| a.0 > 1e-9).map(|a| a.2).sum();
                    if active_weight == 0.0 {
                        let minutes = ((t - now as f64) / 60.0).ceil() as u64;
                        return if minutes == 0 {
                            "dry now".into()
                        } else if minutes >= 1440 {
                            format!("dry in {}d {}h", minutes / 1440, minutes % 1440 / 60)
                        } else {
                            format!("dry in {}h {}m", minutes / 60, minutes % 60)
                        };
                    }
                    let depletion = accounts
                        .iter()
                        .filter(|a| a.0 > 1e-9)
                        .map(|a| a.0 * active_weight / (rate * a.2))
                        .fold(f64::INFINITY, f64::min);
                    let step = hours.min(depletion);
                    for account in &mut accounts {
                        if account.0 > 1e-9 {
                            account.0 =
                                (account.0 - rate * account.2 / active_weight * step).max(0.0);
                        }
                    }
                    t += step * 3600.0;
                    hours -= step;
                }
                t = next;
            }
        }
    }
    if pace < 80 {
        format!("slack · ~{}% unused", 100 - pace)
    } else if pace <= 105 {
        "on pace · safe".into()
    } else if projections.is_empty() {
        "over pace".into()
    } else {
        "tight · safe 7d".into()
    }
}

fn meridian(pct: Option<u64>, width: usize, thin: bool) -> String {
    if width == 0 {
        return String::new();
    }
    let Some(pct) = pct else {
        return "┄".repeat(width);
    };
    let mid = width / 2;
    let filled = ((pct.min(200) as f64 / 100.0 * mid as f64).round() as usize).min(width - 1);
    (0..width)
        .map(|i| {
            if i == mid {
                if thin {
                    '┆'
                } else if pct > 100 {
                    '┼'
                } else {
                    '│'
                }
            } else if i < filled + usize::from(filled >= mid) {
                if thin { '─' } else { '━' }
            } else {
                '·'
            }
        })
        .collect()
}

fn provider_rows(
    dashboard: &DashboardState,
    dialog: &SessionUsageDialog,
    report: &SessionUsageReport,
    width: u16,
) -> (Vec<Line<'static>>, Vec<Option<usize>>, Vec<String>) {
    let label = |id: &str| {
        dashboard.config.profiles.get(id).map_or_else(
            || format!("{id} (unconfigured)"),
            |p| {
                crate::render::quota_pools::profile_provider(id, p, &dashboard.quota_providers)
                    .display_name()
                    .to_owned()
            },
        )
    };
    let mut providers: BTreeMap<String, Provider> = BTreeMap::new();
    for q in &report.quota_usage {
        let p = providers.entry(label(&q.profile)).or_default();
        p.profiles.insert(q.profile.clone());
        p.quota.push(q.clone());
    }
    for p in report
        .quota_projections
        .iter()
        .filter(|p| !p.is_short_window && !p.window_label.to_ascii_lowercase().starts_with("5h"))
    {
        providers
            .entry(label(&p.profile_id))
            .or_default()
            .profiles
            .insert(p.profile_id.clone());
    }
    for u in &report.profile_usage {
        let key = u
            .profile
            .as_deref()
            .map_or_else(|| "Unattributed profile".to_owned(), label);
        let provider = providers.entry(key).or_default();
        provider.cost = provider.cost.saturating_add(u.cost_usd_ticks.unwrap_or(0));
        provider.unknown_cost |= u.cost_usd_ticks.is_err();
        provider.partial |= u.coverage.is_partial();
    }
    for u in &report.profile_role_usage {
        let key = u
            .profile
            .as_deref()
            .map_or_else(|| "Unattributed profile".to_owned(), label);
        let role = providers
            .entry(key)
            .or_default()
            .roles
            .entry(u.role)
            .or_default();
        for model in &u.models {
            role.cost = role.cost.saturating_add(model.cost_usd_ticks.unwrap_or(0));
            role.unknown_cost |= model.cost_usd_ticks.is_err();
            role.tokens = role
                .tokens
                .saturating_add(model.totals.get("total_tokens").map_or(0, |t| t.tokens));
            let name = model
                .model
                .clone()
                .unwrap_or_else(|| "Unknown model".into());
            let amount = model.cost_usd_ticks.unwrap_or(0);
            let total = role.models.entry(name).or_default();
            *total = total.saturating_add(amount);
            role.unreported += model.unreported_turns;
        }
    }
    let duration: u64 = dialog
        .query
        .selected_intervals(report.until_ms)
        .into_iter()
        .map(|(start, end)| (end - start).saturating_add(1).max(0) as u64 / 1000)
        .sum();
    let gauge_width = if width >= 95 {
        25
    } else if width >= 75 {
        17
    } else {
        0
    };
    let scale = match gauge_width {
        25 => "0%        100%      200%+",
        17 => "0%    100%  200%+",
        _ => "",
    };
    let mut lines = vec![Line::styled(
        format!(
            "{:<26}{scale}   Pace      Tank    Est $   Forecast / Models",
            "Provider Pace"
        ),
        theme::muted(),
    )];
    let mut map = vec![None];
    let mut keys = Vec::new();
    for (name, provider) in providers {
        let index = keys.len();
        keys.push(name.clone());
        let mut quota = provider.quota.clone();
        for profile in &provider.profiles {
            if !quota.iter().any(|q| &q.profile == profile) {
                quota.push(mj_core::usage::ProfileQuotaUsage {
                    profile: profile.clone(),
                    ..Default::default()
                });
            }
        }
        let (pct, partial) = pace(&quota, duration);
        let equal = provider.profiles.iter().any(|profile| {
            provider
                .quota
                .iter()
                .find(|q| &q.profile == profile)
                .and_then(|q| q.monthly_price_usd)
                .unwrap_or(0)
                == 0
        });
        let mut weekly = Vec::new();
        for profile in &provider.profiles {
            let projection = report
                .quota_projections
                .iter()
                .filter(|p| {
                    p.profile_id == *profile
                        && !p.is_short_window
                        && !p.window_label.to_ascii_lowercase().starts_with("5h")
                })
                .min_by_key(|p| p.window_label.len());
            if let Some(p) = projection {
                let weight = if equal {
                    1.0
                } else {
                    provider
                        .quota
                        .iter()
                        .find(|q| q.profile == *profile)
                        .and_then(|q| q.monthly_price_usd)
                        .filter(|p| *p > 0)
                        .map_or(1.0, f64::from)
                };
                weekly.push((p, weight));
            }
        }
        let weight: f64 = weekly.iter().map(|(_, w)| w).sum();
        let tank = if weight > 0.0 {
            format!(
                "{:.0}% left",
                weekly
                    .iter()
                    .map(|(p, w)| f64::from(p.remaining_percent) * w)
                    .sum::<f64>()
                    / weight
            )
        } else if provider.quota.is_empty() {
            "∞".into()
        } else {
            "?".into()
        };
        let now = mj_core::clock::epoch_seconds() as i64;
        let live: Vec<_> = weekly
            .iter()
            .filter_map(|(p, w)| {
                p.resets_at_epoch_seconds
                    .filter(|r| *r > now)
                    .map(|r| (f64::from(p.remaining_percent), r, *w))
            })
            .collect();
        let note = pct.map_or_else(
            || {
                if provider.quota.is_empty() && weekly.is_empty() {
                    "usage-billed".into()
                } else {
                    "unmeasured".into()
                }
            },
            |p| forecast(p, &live, now),
        );
        let gauge = meridian(pct, gauge_width, false);
        let arrow = if dialog.expanded_providers.contains(&name) {
            '▾'
        } else {
            '▸'
        };
        let amount = dollars(
            if provider.unknown_cost {
                Err(mj_core::usage::UnknownCost)
            } else {
                Ok(provider.cost)
            },
            provider.partial,
        );
        let pace_text = pct.map_or_else(
            || {
                if provider.quota.is_empty() && weekly.is_empty() {
                    "payg".into()
                } else {
                    "?".into()
                }
            },
            |p| format!("{}{p}%", if partial { "~" } else { "" }),
        );
        let color = match pct {
            None => theme::palette().secondary,
            Some(p) if p < 80 => theme::pin_color(0),
            Some(p) if p <= 105 => theme::palette().success,
            Some(p) if p <= 130 => theme::palette().warning,
            Some(_) => theme::palette().error,
        };
        // A dry forecast overrides the pace band's "safe" reading: exhaustion
        // is a warning below the error band and an error inside it.
        let note_style = if note.starts_with("dry") {
            ratatui::style::Style::default().fg(match pct {
                Some(p) if p > 130 => theme::palette().error,
                _ => theme::palette().warning,
            })
        } else {
            theme::actionable()
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!(
                    "{arrow} {:<23} ",
                    truncate_to_cells(&name, 23, Truncate::PLAIN)
                ),
                theme::actionable(),
            ),
            Span::styled(gauge, ratatui::style::Style::default().fg(color)),
            Span::styled(
                format!(" {pace_text:>6} {tank:>9} {amount:>8}  "),
                theme::actionable(),
            ),
            Span::styled(note, note_style),
        ]));
        map.push(Some(index));
        if dialog.expanded_providers.contains(&name) {
            let mut roles: Vec<_> = provider.roles.into_iter().collect();
            let cost_sum: u64 = roles.iter().map(|(_, r)| r.cost).sum();
            let values: Vec<_> = roles
                .iter()
                .map(|(_, r)| if cost_sum > 0 { r.cost } else { r.tokens })
                .collect();
            if values.iter().all(|v| *v == 0) && pct.unwrap_or(0) > 0 {
                lines.push(Line::styled(
                    format!(
                        "  └─ {:<20} {} {:>5}% 100% share",
                        "External / untracked",
                        meridian(pct, gauge_width, true),
                        pct.unwrap_or(0)
                    ),
                    theme::muted(),
                ));
                map.push(None);
            } else {
                let shares = allocations(&values, 100);
                let paces = allocations(&values, pct.unwrap_or(0));
                let count = roles.len();
                for (i, (role, data)) in roles.drain(..).enumerate() {
                    let mut models: Vec<_> = data.models.into_iter().collect();
                    models.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
                    let model = models
                        .into_iter()
                        .take(2)
                        .map(|(name, _)| name)
                        .collect::<Vec<_>>()
                        .join(", ");
                    let extra = if data.unreported > 0 {
                        format!(" · {} unreported", data.unreported)
                    } else {
                        String::new()
                    };
                    lines.push(Line::styled(
                        format!(
                            "  {} {:<20} {} {:>5}% {:>3}% share {:>8}  {model}{extra}",
                            if i + 1 == count { "└─" } else { "├─" },
                            truncate_to_cells(role.label(), 20, Truncate::PLAIN),
                            meridian(Some(paces[i]), gauge_width, true),
                            paces[i],
                            shares[i],
                            dollars(
                                if data.unknown_cost {
                                    Err(mj_core::usage::UnknownCost)
                                } else {
                                    Ok(data.cost)
                                },
                                data.unreported > 0
                            )
                        ),
                        theme::muted(),
                    ));
                    map.push(None);
                }
            }
        }
    }
    (lines, map, keys)
}

pub(crate) fn render(
    frame: &mut Frame,
    area: Rect,
    dashboard: &DashboardState,
    dialog: &SessionUsageDialog,
    surfaces: &mut FrameSurfaces,
) {
    let popup = centered_modal(frame, surfaces, 110, 34, area);
    let inner = popup.inner(Margin {
        horizontal: 1,
        vertical: 1,
    });
    let mut form = dialog.form.borrow_mut();
    form.begin_frame();
    let title = dismissible_modal_title(
        &mut form,
        popup,
        "Usage Dashboard",
        theme::title(true),
        true,
    );
    frame.render_widget(theme::modal().title(title), popup);
    if inner.height < 9 {
        form.end_frame(UsageControl::Close);
        return;
    }
    // A short terminal drops the blank lines between the controls.
    let compact = inner.height < 14;
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
        Rect::new(
            inner.x,
            inner.y + if compact { 1 } else { 2 },
            inner.width,
            1,
        ),
        "Include suspended sessions",
        dialog.query.include_suspended,
        true,
        &mut form,
        UsageControl::Suspended,
    );
    let window = match dialog.query.hour_starts_ms.as_ref() {
        None => "last 24 clock hours".to_owned(),
        Some(hours) if hours.is_empty() => "no hours selected".to_owned(),
        Some(hours) if hours.len() > 1 => format!("{} hours selected", hours.len()),
        Some(hours) => {
            let ms = *hours.first().expect("one selected hour");
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
        }
    };
    // A short terminal keeps the rows and drops the graph.
    let bar_rows = if inner.height >= 24 {
        5
    } else if compact {
        1
    } else {
        2
    };
    let header_height = if compact { 3 } else { 5 };
    // The chart label row occupies one of these rows; normal mode adds a gap.
    let changes = dialog
        .report
        .as_ref()
        .and_then(|r| r.as_ref().ok())
        .map(|r| r.role_changes.as_slice())
        .unwrap_or_default();
    let changes_height = if compact || changes.is_empty() {
        0
    } else {
        (changes.len() as u16 + 1).min(4)
    };
    let row_gap = if compact { 2 } else { 3 };
    let hints = [
        "Click/←→ hour · Ctrl toggle · Shift range · Ctrl+Shift add · Enter all",
        "Sub-agents included · profile filters turns · + partial · ░ unpriced · base rates",
    ];
    let visible_hints = &hints[usize::from(compact)..];
    let footer_height = visible_hints.len() as u16 + 1;
    let (provider_lines, provider_map, provider_keys) = match &dialog.report {
        Some(Ok(report)) => provider_rows(dashboard, dialog, report, inner.width),
        _ => (Vec::new(), Vec::new(), Vec::new()),
    };
    let summary_budget = inner
        .height
        .saturating_sub(header_height + bar_rows + row_gap + changes_height + footer_height + 1);
    let provider_height = (provider_lines.len() as u16).min(summary_budget);
    let chart_y = inner.y + header_height + provider_height;
    *dialog.provider_keys.borrow_mut() = provider_keys;
    if provider_height > 0 {
        let selected = form.selected(UsageControl::Providers).unwrap_or(0);
        ChoiceList::render_with_rows(
            frame,
            Rect::new(
                inner.x,
                chart_y - provider_height,
                inner.width,
                provider_height,
            ),
            &provider_lines,
            selected,
            &provider_map,
            &[],
            &mut form,
            UsageControl::Providers,
        );
    }
    render_hour_chart(
        frame,
        Rect::new(inner.x, chart_y, inner.width, bar_rows + 2),
        dialog,
        bar_rows,
        &mut form,
    );
    let changes_y = chart_y + bar_rows + row_gap;
    if changes_height > 0 {
        frame.render_widget(
            Paragraph::new("Role changes on timeline").style(theme::title(false)),
            Rect::new(inner.x, changes_y, inner.width, 1),
        );
        let lines: Vec<_> = changes
            .iter()
            .enumerate()
            .map(|(index, change)| {
                let local = chrono::DateTime::from_timestamp_millis(change.changed_at_ms)
                    .unwrap_or_default()
                    .with_timezone(&DisplayZone::now().timezone());
                Line::raw(format!(
                    "{} {}  {}",
                    change_badge(index),
                    local.format("%H:%M"),
                    change.summary
                ))
            })
            .collect();
        let selected = form
            .selected(UsageControl::Changes)
            .unwrap_or(0)
            .min(lines.len().saturating_sub(1));
        let row_map: Vec<_> = (0..lines.len()).map(Some).collect();
        ChoiceList::render_with_rows(
            frame,
            Rect::new(inner.x, changes_y + 1, inner.width, changes_height - 1),
            &lines,
            selected,
            &row_map,
            &[],
            &mut form,
            UsageControl::Changes,
        );
    }
    let rows_y = changes_y + changes_height;
    let rows_area = Rect::new(
        inner.x,
        rows_y,
        inner.width,
        inner.bottom().saturating_sub(rows_y + footer_height),
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
                Rect::new(inner.x, inner.y + header_height - 1, inner.width, 1),
            );
            let mut lines = Vec::new();
            let mut row_map = Vec::new();
            let mut row_keys = dialog.row_keys.borrow_mut();
            row_keys.clear();
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
                    let unreported = model.unreported_turns;
                    let tokens = match (model.totals.get("total_tokens"), unreported) {
                        (Some(v), 0) => format!("{} tokens", v.tokens),
                        (Some(v), n) => format!(
                            "{} tokens · {n} {} unreported",
                            v.tokens,
                            if n == 1 { "turn" } else { "turns" }
                        ),
                        (None, 0) => "tokens unknown".to_owned(),
                        (None, n) => {
                            format!("{n} {} unreported", if n == 1 { "turn" } else { "turns" })
                        }
                    };
                    let detail = format!(
                        "{}  {tokens}",
                        dollars(model.cost_usd_ticks, unreported > 0)
                    );
                    let name = truncate_to_cells(
                        model.model.as_deref().unwrap_or("Unknown model"),
                        usize::from(inner.width)
                            .saturating_sub(Line::raw(detail.as_str()).width() + 6),
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
    for (row, hint) in visible_hints.iter().rev().enumerate() {
        frame.render_widget(
            Paragraph::new(*hint).style(theme::muted()),
            Rect::new(inner.x, inner.bottom() - 2 - row as u16, inner.width, 1),
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

fn change_badge(index: usize) -> char {
    ['①', '②', '③', '④', '⑤', '⑥', '⑦', '⑧', '⑨']
        .get(index)
        .copied()
        .unwrap_or('*')
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
    let highlighted = dialog
        .hour_drag_preview
        .as_ref()
        .or(dialog.query.hour_starts_ms.as_ref());
    let selected =
        |hour: &HourlyUsage| highlighted.is_some_and(|selected| selected.contains(&hour.start_ms));
    let cursor = hours
        .iter()
        .position(|hour| Some(hour.start_ms) == dialog.hour_cursor)
        .unwrap_or(hours.len().saturating_sub(1));
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
        for hour in hours {
            let level = eighths(hour).saturating_sub(level_below).min(8);
            let glyph = match (level, hour.has_unknown_cost) {
                (0, _) => ' ',
                (_, true) => '░',
                (level, false) => BLOCKS[usize::from(level) - 1],
            };
            let style = if selected(hour) {
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
    let changes = dialog
        .report
        .as_ref()
        .and_then(|r| r.as_ref().ok())
        .map(|r| r.role_changes.as_slice())
        .unwrap_or_default();
    let mut axis = vec![Span::raw(" ".repeat(usize::from(gutter)))];
    for hour in hours {
        let marker = changes
            .iter()
            .enumerate()
            .filter(|(_, change)| hour_start_ms(change.changed_at_ms) == hour.start_ms)
            .map(|(index, _)| format!("▲{}", change_badge(index)))
            .collect::<String>();
        let marker = truncate_to_cells(&marker, usize::from(pitch), Truncate::PLAIN);
        let remaining = usize::from(pitch).saturating_sub(Line::raw(marker.as_str()).width());
        axis.push(Span::styled(
            marker,
            ratatui::style::Style::default().fg(theme::pin_color(5)),
        ));
        axis.push(Span::styled(
            if selected(hour) { "━" } else { "─" }.repeat(remaining),
            if selected(hour) {
                theme::active_control()
            } else {
                theme::muted()
            },
        ));
    }
    lines.push(Line::from(axis));
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
        let mut style = if hours[index..(index + stride).min(hours.len())]
            .iter()
            .any(selected)
        {
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
    dialog.hour_geometry.set((
        Rect::new(area.x + gutter, area.y, shown as u16 * pitch, bar_rows),
        pitch,
    ));
    form.register_columns(
        UsageControl::Hours,
        Rect::new(area.x + gutter, area.y, shown as u16 * pitch, area.height),
        cursor,
        &columns,
        shown > 0,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_core::usage::ProfileQuotaUsage;

    #[test]
    fn usage_weighted_weekly_pace_and_partial_coverage() {
        let a = ProfileQuotaUsage {
            profile: "a".into(),
            consumed_basis_points: Some(10_000),
            covered_seconds: Some(604_800),
            partial: false,
            monthly_price_usd: Some(100),
        };
        let mut b = ProfileQuotaUsage {
            profile: "b".into(),
            consumed_basis_points: Some(10_000),
            covered_seconds: Some(302_400),
            partial: false,
            monthly_price_usd: Some(300),
        };
        assert_eq!(pace(&[a.clone(), b.clone()], 604_800), (Some(175), true));
        b.monthly_price_usd = None;
        assert_eq!(pace(&[a.clone(), b.clone()], 604_800), (Some(150), true));
        b.covered_seconds = Some(59);
        assert_eq!(pace(&[a, b], 604_800), (Some(100), true));
    }

    #[test]
    fn usage_staggered_resets_change_pooled_forecast() {
        assert_eq!(
            forecast(200, &[(30.0, 200_000, 1.0), (30.0, 200_000, 1.0)], 0),
            "dry in 1d 1h"
        );
        assert_eq!(
            forecast(110, &[(90.0, 86_400, 1.0), (90.0, 345_600, 1.0)], 0),
            "tight · safe 7d"
        );
        assert_eq!(forecast(120, &[], 0), "over pace");
    }

    /// A weekly pace inside the "safe" band still exhausts a nearly-empty tank
    /// long before its reset; the forecast must say so.
    // Hard-won: review-group-e0d8539b: pace ≤ 105% short-circuited to "on pace · safe" without simulating exhaustion.
    #[test]
    fn usage_forecast_reports_exhaustion_at_a_safe_pace() {
        let now = 1_700_000_000;
        let note = forecast(100, &[(10.0, now + 3 * 86_400, 1.0)], now);
        assert!(
            note.starts_with("dry in"),
            "pace 100% with 10% left and a reset 3d away runs dry: {note}"
        );
        let note = forecast(100, &[(0.0, now + 3 * 86_400, 1.0)], now);
        assert_eq!(note, "dry now");
    }

    #[test]
    fn usage_largest_remainder_keeps_role_totals_and_meridian_scale() {
        assert_eq!(allocations(&[1, 1, 1], 100), vec![34, 33, 33]);
        assert_eq!(allocations(&[1, 2, 3], 145).iter().sum::<u64>(), 145);
        assert_eq!(meridian(Some(100), 25, false).chars().nth(12), Some('│'));
        assert_eq!(meridian(Some(100), 25, true).chars().nth(12), Some('┆'));
        assert_eq!(meridian(Some(10), 0, false), "");
    }
}
