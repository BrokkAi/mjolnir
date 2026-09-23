use super::*;

/// One reading in a minimized pane: a name, its value, and how healthy the
/// value is. A reading with no health to report draws in the ordinary
/// foreground rather than claiming a colour it has not earned.
pub(crate) struct SummaryReading {
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
                    value: format!("refreshing{}", theme::glyphs().ellipsis),
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
                    value: format!("refreshing{}", theme::glyphs().ellipsis),
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
pub(crate) fn summary_row(
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
            let ellipsis = theme::glyphs().ellipsis;
            spans.push(Span::raw(if index == 0 {
                format!("{ellipsis} ")
            } else {
                format!(", {ellipsis} ")
            }));
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

pub(crate) fn quota_remaining_percent(window: &QuotaWindow) -> Option<u8> {
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

pub(crate) const EMPTY_QUOTA_CELL: &str = " ";

/// The chart's rails, in the symbol set in force.
fn quota_chart_left_border() -> &'static str {
    theme::glyphs().bar_left
}
fn quota_chart_right_border() -> &'static str {
    theme::glyphs().bar_right
}
// Both bar kinds occupy the same column, so they must agree on the cell count.
pub(crate) const QUOTA_BAR_CELLS: usize = 10;

pub(crate) fn quota_chart_border_style() -> Style {
    Style::default()
        .fg(theme::palette().muted)
        .bg(theme::palette().background)
}

pub(crate) fn quota_bar(window: Option<&QuotaWindow>) -> Line<'static> {
    pub(crate) const CELLS: usize = QUOTA_BAR_CELLS;
    pub(crate) const EIGHTHS_PER_CELL: usize = 8;
    let Some(remaining) = window.and_then(quota_remaining_percent) else {
        return Line::default();
    };
    let eighths = (usize::from(remaining) * CELLS * EIGHTHS_PER_CELL + 50) / 100;
    let full_cells = eighths / EIGHTHS_PER_CELL;
    let partial_eighths = eighths % EIGHTHS_PER_CELL;
    // ASCII has no partial cell, so the fraction rounds to a whole cell.
    let partial = if theme::ascii() {
        if partial_eighths >= 4 { "#" } else { "" }
    } else {
        ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉"][partial_eighths]
    };
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
        Span::styled(theme::glyphs().bar_full.repeat(full_cells), bar_style),
        Span::styled(partial.to_string(), bar_style),
        Span::styled(
            EMPTY_QUOTA_CELL.repeat(empty_cells),
            Style::default().bg(theme::palette().background),
        ),
        // The percentage follows the chart rail without an extra separator.
        Span::styled(quota_chart_right_border(), quota_chart_border_style()),
        Span::styled(
            format!("{remaining}%"),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
    ])
}

/// Renders the API label in the same black, bordered field as capacity charts.
pub(crate) fn api_quota_bar() -> Line<'static> {
    let label = API_LABEL;
    let label_cells = label.chars().count().min(QUOTA_BAR_CELLS);
    let left = (QUOTA_BAR_CELLS - label_cells) / 2;
    let right = QUOTA_BAR_CELLS - label_cells - left;
    let field = Style::default().bg(theme::palette().background);
    Line::from(vec![
        Span::styled(EMPTY_QUOTA_CELL.repeat(left), field),
        Span::styled(label, field.add_modifier(Modifier::BOLD)),
        Span::styled(EMPTY_QUOTA_CELL.repeat(right), field),
        Span::styled(quota_chart_right_border(), quota_chart_border_style()),
    ])
}

pub(crate) fn weekly_quota_exhausted(quota: &ProfileQuota) -> bool {
    quota
        .weekly_window()
        .and_then(quota_remaining_percent)
        .is_some_and(|remaining| remaining < 1)
}

pub(crate) fn five_hour_quota_bar(quota: &ProfileQuota) -> Line<'static> {
    let five_hour = if weekly_quota_exhausted(quota) {
        None
    } else {
        quota.five_hour_window()
    };
    quota_bar(five_hour)
}

pub(crate) fn quota_reset_countdown(now: u64, reset_at_epoch_seconds: i64) -> String {
    let Ok(reset) = u64::try_from(reset_at_epoch_seconds) else {
        return "now".into();
    };
    let remaining = reset.saturating_sub(now);
    if remaining == 0 {
        return "now".into();
    }

    pub(crate) const MINUTE: u64 = 60;
    pub(crate) const HOUR: u64 = 60 * MINUTE;
    pub(crate) const DAY: u64 = 24 * HOUR;
    if remaining >= DAY {
        let days = remaining / DAY;
        let hours = remaining % DAY / HOUR;
        format!("{days}d {hours}h")
    } else if remaining >= HOUR {
        let hours = remaining / HOUR;
        let minutes = remaining % HOUR / MINUTE;
        // Under ten hours the minutes decide whether to wait, so show them.
        if hours < 10 && minutes > 0 {
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

pub(crate) fn five_hour_quota_reset_countdown(now: u64, reset_at_epoch_seconds: i64) -> String {
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

pub(crate) struct QuotaTableRow {
    profile: String,
    harness: String,
    weekly: Line<'static>,
    weekly_reset: String,
    five_hour: Line<'static>,
    five_hour_reset: String,
}

impl QuotaTableRow {
    pub(crate) fn into_row(self) -> Row<'static> {
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

pub(crate) fn quota_chart(mut chart: Line<'static>, chart_present: bool) -> Line<'static> {
    let mut spans = Vec::new();
    if chart_present {
        spans.push(Span::styled(
            quota_chart_left_border(),
            quota_chart_border_style(),
        ));
    }
    spans.append(&mut chart.spans);
    Line::from(spans)
}

/// Trailing room after each content column. Quota-window resets sit one cell
/// after their percentage; the larger table groups retain two-cell gaps.
pub(crate) const QUOTA_COLUMN_GAPS: [u16; 6] = [2, 2, 1, 2, 1, 0];

pub(crate) fn quota_column_width(
    header: &str,
    content_widths: impl Iterator<Item = usize>,
    maximum: u16,
) -> u16 {
    let width = content_widths.fold(Line::raw(header).width(), usize::max);
    u16::try_from(width).unwrap_or(u16::MAX).min(maximum)
}

pub(crate) fn quota_table_rows(dashboard: &DashboardState, now: u64) -> Vec<QuotaTableRow> {
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
                        Line::raw(format!("refreshing{}", theme::glyphs().ellipsis)),
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
                            Line::raw(format!("refreshing{}", theme::glyphs().ellipsis)),
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

pub(crate) fn quota_table_column_widths(rows: &[QuotaTableRow]) -> [u16; 6] {
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
        format!("refreshing{}", theme::glyphs().ellipsis)
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
    let status = truncate_to_cells(
        &format!("({refresh_status}) "),
        status_budget,
        Truncate::SUMMARY,
    );
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
