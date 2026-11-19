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
            // Same rule as the expanded table: a host whose listed targets
            // are all unavailable shows no reading.
            if super::capacity::all_listed_targets_unavailable(&detail.target.target_ids, dashboard)
            {
                return SummaryReading {
                    name: detail.target.host.clone(),
                    value: "unavailable".to_string(),
                    color: None,
                };
            }
            if detail.refreshing {
                return SummaryReading {
                    name: detail.target.host.clone(),
                    value: format!("refreshing{}", theme::glyphs().ellipsis),
                    color: None,
                };
            }
            let (value, cpu_percent) = match (&detail.target.kind, &detail.state.usage) {
                (DeploymentCapacityKind::Host, Some(usage)) => match usage.cpu_percent {
                    Some(cpu) => (format!("{cpu}%"), Some(cpu)),
                    None => ("no CPU".to_string(), None),
                },
                // A fleet has no CPU percentage of its own, so what it reports
                // is how many machines it is running.
                (DeploymentCapacityKind::AwsFleet, Some(_)) => (fleet_vm_label(detail), None),
                (DeploymentCapacityKind::AwsFleet, None) if detail.state.on_demand => {
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

/// A weighted reading for each independently metered provider pool.
pub(crate) fn minimized_quota_line(
    dashboard: &DashboardState,
    width: u16,
    focused: bool,
    now: u64,
) -> Line<'static> {
    let mut readings = Vec::new();
    let mut statuses = Vec::new();
    for provider in super::quota_pools::provider_pools(
        &dashboard.config,
        &dashboard.quotas,
        &dashboard.quota_providers,
        now,
    ) {
        for pool in provider.pools {
            readings.push(SummaryReading {
                name: if pool.label.is_empty() {
                    provider.provider.display_name().into()
                } else {
                    format!("{} {}", provider.provider.display_name(), pool.label)
                },
                value: format!(
                    "{}{}",
                    pool.percentage(),
                    if pool
                        .members
                        .iter()
                        .any(|member| dashboard.quota_refreshing.contains(&member.id))
                    {
                        " (refreshing)"
                    } else {
                        ""
                    }
                ),
                color: Some(provider_accent(&provider.provider)),
            });
        }
        for (id, status) in provider.text {
            let status = if dashboard.quota_refreshing.contains(&id) {
                format!("refreshing{}", theme::glyphs().ellipsis)
            } else if status.starts_with("quota check refused") {
                "check refused".into()
            } else {
                status
            };
            statuses.push(SummaryReading {
                name: id,
                value: status,
                color: None,
            });
        }
    }
    readings.extend(statuses);
    summary_row("Profiles", &readings, width, focused)
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
    // readings: `─ Profiles ── claude-1 63% ────`.
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

pub(crate) fn weekly_quota_exhausted(quota: &ProfileQuota) -> bool {
    quota
        .weekly_window()
        .and_then(quota_remaining_percent)
        .is_some_and(|remaining| remaining < 1)
}

pub(crate) fn quota_column_width(
    header: &str,
    content_widths: impl Iterator<Item = usize>,
    maximum: u16,
) -> u16 {
    u16::try_from(content_widths.fold(Line::raw(header).width(), usize::max))
        .unwrap_or(u16::MAX)
        .min(maximum)
}

/// Enough space for seven readable profile segments, without forcing short
/// dashboards to reserve the width of expanded notes.
pub(crate) fn quota_pane_width(_dashboard: &DashboardState) -> u16 {
    76
}

pub(crate) fn quota_content_height(dashboard: &DashboardState) -> usize {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    quota_lines(
        dashboard,
        usize::from(quota_pane_width(dashboard).saturating_sub(2)),
        now,
        false,
        clock_label,
    )
    .0
    .len()
    .max(1)
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
    render_quotas_at(frame, area, dashboard, size, now, clock_label);
}

pub(crate) fn render_quotas_at(
    frame: &mut Frame,
    area: Rect,
    dashboard: &mut DashboardState,
    size: Option<PaneSize>,
    now: u64,
    clock: fn(i64, &str) -> String,
) {
    // Provider discovery is polled once per frame by the combined renderer,
    // before pane sizing, so sizing and drawing use the same snapshot.
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
    // On the combined surface (`size` is set) the title opens the pane's
    // menu, and says so with a dropdown mark.
    let label = crate::surface_controls::pane_title_label("Profiles", size.is_some());
    let label_width = Line::raw(label.as_str()).width();
    let title_budget = size.map_or_else(
        || area.width.saturating_sub(2),
        |_| {
            pane_title_content_width(
                area.width,
                dashboard.pane_maximize_enabled(SupportPane::Quota),
            )
        },
    );
    let status_budget = usize::from(title_budget).saturating_sub(label_width);
    let status = truncate_to_cells(
        &format!("({refresh_status}) "),
        status_budget,
        Truncate::SUMMARY,
    );
    let pressed = size.is_some()
        && crate::surface_controls::register_pane_title_menu(
            dashboard,
            SupportPane::Quota,
            Rect::new(
                area.x.saturating_add(1),
                area.y,
                u16::try_from(label_width)
                    .unwrap_or(u16::MAX)
                    .min(title_budget),
                area.height.min(1),
            ),
        );
    let title = Line::from(vec![
        if pressed {
            Span::styled(label, theme::selection(true))
        } else {
            Span::raw(label)
        },
        Span::styled(status, Style::default().fg(theme::palette().muted)),
    ]);

    let focused = dashboard.focus == Focus::Quota;
    let block = theme::panel(focused).title(title);
    let block = size.map_or(block.clone(), |size| {
        block.title(pane_size_controls(
            size,
            dashboard.pane_maximize_enabled(SupportPane::Quota),
        ))
    });
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let (lines, pool_rows, profile_rows) =
        quota_lines(dashboard, usize::from(inner.width), now, focused, clock);
    let viewport = usize::from(inner.height);
    dashboard.quota_viewport.set(viewport);
    let mut offset = dashboard
        .quota_scroll
        .get()
        .min(lines.len().saturating_sub(viewport));
    let selected = dashboard.selected_quota_row();
    let mut selected_lines = pool_rows
        .iter()
        .filter(|(row, _)| Some(row) == selected.as_ref())
        .map(|(_, line)| *line);
    let rendered_selection = selected_lines.next().and_then(|first| {
        selected
            .clone()
            .map(|row| (row, first, selected_lines.next_back().unwrap_or(first)))
    });
    let moved = *dashboard.quota_rendered_selection.borrow() != rendered_selection;
    *dashboard.quota_rendered_selection.borrow_mut() = rendered_selection.clone();
    if (take_selection_recenter(dashboard, Focus::Quota) || (focused && moved))
        && let Some((_, first, last)) = rendered_selection
    {
        if first < offset {
            offset = first;
        }
        if last >= offset + viewport {
            offset = (last + 1).saturating_sub(viewport).min(first);
        }
    }
    frame.render_widget(
        Paragraph::new(lines.clone()).scroll((u16::try_from(offset).unwrap_or(u16::MAX), 0)),
        inner,
    );
    dashboard.quota_scroll.set(offset);
    *dashboard.quota_pool_areas.borrow_mut() = pool_rows
        .iter()
        .filter(|(_, row)| *row >= offset && *row < offset + viewport)
        .map(|(identity, row)| {
            (
                identity.clone(),
                Rect::new(inner.x, inner.y + (row - offset) as u16, inner.width, 1),
            )
        })
        .collect();
    *dashboard.quota_row_areas.borrow_mut() = profile_rows
        .into_iter()
        .filter(|(_, row)| *row >= offset && *row < offset + viewport)
        .map(|(id, row)| {
            (
                id,
                Rect::new(inner.x, inner.y + (row - offset) as u16, inner.width, 1),
            )
        })
        .collect();
    render_session_scrollbar(frame, area, lines.len(), offset, viewport);
}

use super::quota_pools::{PoolMember, PoolPeriod, QuotaPool, QuotaRow, refill_widths};
use mj_core::config::QuotaProvider;

#[allow(
    clippy::disallowed_methods,
    reason = "Provider colours paint both foreground and background against the active theme; monochrome uses the theme palette."
)]
fn provider_accent(provider: &QuotaProvider) -> Color {
    if theme::current() == theme::UiTheme::Mono {
        return theme::palette().text;
    }
    match provider {
        QuotaProvider::Anthropic => Color::Rgb(217, 119, 87),
        QuotaProvider::OpenAI => Color::Rgb(16, 163, 127),
        QuotaProvider::Google => Color::Rgb(66, 133, 244),
        QuotaProvider::XAI => Color::Rgb(200, 200, 210),
        QuotaProvider::Moonshot => Color::Rgb(150, 110, 230),
        QuotaProvider::DeepSeek => Color::Rgb(77, 107, 254),
        QuotaProvider::Zai => Color::Rgb(60, 170, 200),
        QuotaProvider::Meta => Color::Rgb(0, 129, 251),
        QuotaProvider::Other(_) => theme::palette().muted,
    }
}

#[allow(
    clippy::disallowed_methods,
    reason = "Blend provider backgrounds against the active theme background, never an unknown terminal background."
)]
fn tint(accent: Color, strength: f64) -> Color {
    let Color::Rgb(r, g, b) = accent else {
        return accent;
    };
    let (br, bg, bb) = match theme::palette().background {
        Color::Rgb(r, g, b) => (r, g, b),
        _ => (30, 30, 30),
    };
    let mix =
        |a: u8, b: u8| (f64::from(b) + (f64::from(a) - f64::from(b)) * strength).round() as u8;
    Color::Rgb(mix(r, br), mix(g, bg), mix(b, bb))
}

/// Label colour with legible contrast on a provider fill: dark text on light
/// backgrounds, light text on dark ones. Non-RGB fills and monochrome keep the theme text.
#[allow(
    clippy::disallowed_methods,
    reason = "Contrast is computed from the fill's own RGB value, not from a theme colour."
)]
fn readable_on(background: Color) -> Color {
    let Color::Rgb(r, g, b) = background else {
        return theme::palette().text;
    };
    // ASCII still paints RGB provider fills, so it needs the same contrast.
    if theme::current() == theme::UiTheme::Mono {
        return theme::palette().text;
    }
    let linear = |c: u8| {
        let c = f64::from(c) / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    let luminance = 0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b);
    // Black text beats white above this luminance (equal contrast ratios).
    if luminance > 0.179 {
        Color::Rgb(16, 16, 16)
    } else {
        Color::Rgb(245, 245, 245)
    }
}

/// Countdown to a reset, falling back to the provider's reset text when the
/// report carries no timestamp.
pub(crate) fn reset_label(reset: Option<i64>, text: Option<&str>, now: u64) -> String {
    match (reset, text) {
        (None, Some(text)) => text.to_owned(),
        (reset, _) => reset_duration(reset, now),
    }
}

pub(crate) fn reset_duration(reset: Option<i64>, now: u64) -> String {
    let Some(reset) = reset else {
        return "?".into();
    };
    let seconds = (reset.max(0) as u64).saturating_sub(now);
    let days = seconds / 86400;
    let hours = seconds % 86400 / 3600;
    let minutes = seconds % 3600 / 60;
    if days > 0 {
        format!("{days}d{hours}h")
    } else if hours > 0 && minutes > 0 {
        format!("{hours}h{minutes:02}m")
    } else if hours > 0 {
        format!("{hours}h")
    } else {
        format!("{minutes}m")
    }
}

fn clock_label(epoch: i64, format: &str) -> String {
    chrono::DateTime::from_timestamp(epoch, 0)
        .map(|time| {
            time.with_timezone(&chrono::Local)
                .format(format)
                .to_string()
        })
        .unwrap_or_else(|| "unknown".into())
}

/// The line naming a profile whose reset passed without a new reading. Its
/// banked resets are a separate balance and stay known.
fn overdue_entry(member: &PoolMember, ascii: bool) -> String {
    let mut entry = format!("{} reset due, awaiting reading", member.id);
    if let Some(count) = member.banked.filter(|&count| count > 0) {
        entry.push_str(&format!(" {}{count} banked", if ascii { "B" } else { "⟲" }));
    }
    entry
}

fn pool_bar(
    pool: &QuotaPool,
    width: usize,
    now: u64,
    accent: Color,
    refreshing: &std::collections::BTreeSet<String>,
) -> (Vec<Span<'static>>, Vec<String>) {
    let weight = pool.reported_weight();
    if weight == 0.0 {
        let overdue = pool
            .members
            .iter()
            .filter(|member| member.awaiting_reading)
            .map(|member| overdue_entry(member, theme::ascii()))
            .collect();
        return (vec![Span::raw(" ".repeat(width))], overdue);
    }
    let refills: Vec<_> = pool
        .reported_members()
        .filter(|member| member.remaining < 100 || refreshing.contains(&member.id))
        .map(|member| (member, member.weight * f64::from(100 - member.remaining)))
        .collect();
    let widths = refill_widths(
        pool.available_capacity(),
        refills.iter().map(|&(_, capacity)| capacity),
        weight * 100.0,
        width,
    );
    let ascii = theme::ascii();
    let use_glyphs =
        ascii || theme::current() == theme::UiTheme::Mono || !matches!(accent, Color::Rgb(..));
    let fill_glyph = if ascii {
        "#"
    } else if use_glyphs {
        "█"
    } else {
        " "
    };
    // A full block draws in the foreground colour, so in Unicode glyph mode
    // the fill is the block itself; reversing it would paint the background
    // over the whole cell. ASCII `#` keeps reverse video to stay solid.
    let fill_style = if ascii {
        Style::default()
            .fg(readable_on(accent))
            .bg(accent)
            .add_modifier(Modifier::REVERSED)
    } else if use_glyphs {
        let colour = if theme::current() == theme::UiTheme::Mono {
            theme::palette().text
        } else {
            accent
        };
        Style::default().fg(colour).bg(theme::palette().background)
    } else {
        Style::default().fg(readable_on(accent)).bg(accent)
    };
    let mut spans = vec![Span::styled(fill_glyph.repeat(widths[0]), fill_style)];
    let mut fallback = Vec::new();
    let mut alternate = false;
    for ((member, capacity), cells) in refills.into_iter().zip(widths.into_iter().skip(1)) {
        let is_refreshing = refreshing.contains(&member.id);
        let status = if is_refreshing {
            format!("refreshing{}", theme::glyphs().ellipsis)
        } else {
            format!(
                "{}{}",
                theme::glyphs().resuming,
                reset_label(member.reset, member.reset_text.as_deref(), now)
            )
        };
        let banked = member
            .banked
            .filter(|&count| count > 0)
            .map(|count| format!("{}{count}", if ascii { "B" } else { "⟲" }));
        // Leave a glyph visible where the tint alone cannot mark the boundary.
        let label_budget = cells.saturating_sub(usize::from(use_glyphs));
        let label_width = |label: &str| Span::raw(label).width();
        let short_status = std::iter::once(status.as_str())
            .chain(
                is_refreshing
                    .then_some(["refresh", "check"])
                    .into_iter()
                    .flatten(),
            )
            .find(|label| label_width(label) <= label_budget);
        let label = if let Some(short_status) = short_status {
            if let Some(banked) = banked {
                let full_label = format!("{short_status} {banked}");
                if label_width(&full_label) <= label_budget {
                    full_label
                } else {
                    fallback.push(format!("{} {banked} banked", member.id));
                    short_status.to_string()
                }
            } else {
                short_status.to_string()
            }
        } else {
            let mut entry = member.id.clone();
            if capacity > 0.0 {
                let share = capacity / weight;
                let amount = if share < 1.0 {
                    "<1".to_string()
                } else {
                    format!("{share:.0}")
                };
                entry.push_str(&format!(" +{amount}%"));
            }
            entry.push(' ');
            entry.push_str(&status);
            if let Some(banked) = banked {
                entry.push_str(&format!(" {banked}"));
            }
            fallback.push(entry);
            String::new()
        };
        if cells == 0 {
            continue;
        }
        let tint_strength = if alternate { 0.52 } else { 0.24 };
        let glyph = if use_glyphs {
            if alternate { "-" } else { "=" }
        } else {
            " "
        };
        let background = tint(accent, tint_strength);
        spans.push(Span::styled(
            format!("{}{label}", glyph.repeat(cells - label_width(&label))),
            Style::default().fg(readable_on(background)).bg(background),
        ));
        alternate = !alternate;
    }
    // A profile whose reset passed without a new reading adds nothing to the
    // bar; name it so the pane says which profile is waiting.
    for member in pool.members.iter().filter(|member| member.awaiting_reading) {
        fallback.push(overdue_entry(member, ascii));
    }
    // A full profile has no refill chunk, but its banked resets still matter.
    for member in pool
        .reported_members()
        .filter(|member| member.remaining >= 100 && !refreshing.contains(&member.id))
    {
        if let Some(count) = member.banked.filter(|&count| count > 0) {
            fallback.push(format!(
                "{} {}{count} banked",
                member.id,
                if ascii { "B" } else { "⟲" }
            ));
        }
    }
    (spans, fallback)
}

fn lane(
    member: &PoolMember,
    period: &PoolPeriod,
    cells: usize,
    now: u64,
    accent: Color,
) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    // A reading older than its own reset (weekly or five-hour) cannot say
    // whether the profile is usable now, so draw no availability; reset
    // markers from readings that are still current stay.
    let unknown = member.awaiting_reading || member.five_hour_awaiting_reading;
    for cell in 0..cells {
        let t0 = now.saturating_add(cell as u64 * 86400 / cells as u64);
        let t1 = now.saturating_add((cell + 1) as u64 * 86400 / cells as u64);
        let contains = |reset: Option<i64>| {
            reset.is_some_and(|reset| reset >= 0 && t0 <= reset as u64 && (reset as u64) < t1)
        };
        let weekly_reset = member.reset.filter(|_| !member.awaiting_reading);
        let five_reset = member
            .five_hour
            .as_ref()
            .filter(|_| !member.five_hour_awaiting_reading)
            .and_then(|window| window.resets_at_epoch_seconds);
        let blocked = member
            .other_long_blocked_until
            .is_some_and(|until| until > t0 as i64)
            || (member.remaining == 0 && member.reset.is_none_or(|reset| reset > t0 as i64))
            || member.five_hour.as_ref().is_some_and(|window| {
                quota_remaining_percent(window) == Some(0)
                    && window
                        .resets_at_epoch_seconds
                        .is_none_or(|reset| reset > t0 as i64)
            });
        // A five-hour reading without a reset time is still the active
        // balance; only its end is unknown.
        let five = member.five_hour.as_ref().filter(|window| {
            window
                .resets_at_epoch_seconds
                .is_none_or(|reset| reset > t0 as i64)
        });
        let marker = if contains(weekly_reset) && contains(five_reset) {
            Some('*')
        } else if contains(weekly_reset) {
            Some(period.marker())
        } else if contains(five_reset) {
            Some('5')
        } else {
            None
        };
        if unknown {
            spans.push(Span::styled(
                marker
                    .unwrap_or(if theme::ascii() { '?' } else { '·' })
                    .to_string(),
                Style::default()
                    .fg(if marker.is_some() {
                        theme::palette().text
                    } else {
                        theme::palette().muted
                    })
                    .bg(theme::palette().background),
            ));
            continue;
        }
        let symbol = marker.unwrap_or(if blocked {
            if theme::ascii() { '.' } else { '·' }
        } else if theme::ascii() || theme::current() == theme::UiTheme::Mono {
            if five.is_some() { '=' } else { '-' }
        } else {
            ' '
        });
        let background = if blocked {
            theme::palette().background
        } else if let Some(window) = five {
            tint(
                accent,
                0.25 + 0.75 * f64::from(quota_remaining_percent(window).unwrap_or(0)) / 100.0,
            )
        } else {
            tint(accent, 0.18)
        };
        spans.push(Span::styled(
            symbol.to_string(),
            Style::default()
                .fg(if marker.is_some() {
                    theme::palette().text
                } else {
                    theme::palette().muted
                })
                .bg(background),
        ));
    }
    spans
}

fn lane_note(
    member: &PoolMember,
    period: &PoolPeriod,
    now: u64,
    clock: fn(i64, &str) -> String,
    refreshing: bool,
) -> String {
    // A collapsed segment can be too narrow to say so; the lane always does.
    let refresh = if refreshing {
        format!("refreshing{} · ", theme::glyphs().ellipsis)
    } else {
        String::new()
    };
    let mut note = if member.awaiting_reading {
        format!("{} reset due, awaiting reading", period.label())
    } else if member.remaining == 0 {
        format!(
            "{} back {} ({})",
            period.label(),
            member
                .reset
                .map(|reset| clock(reset, "%a %H:%M"))
                .unwrap_or_else(|| "unknown".into()),
            reset_label(member.reset, member.reset_text.as_deref(), now)
        )
    } else {
        format!(
            "{} {}%, resets in {}",
            period.label(),
            member.remaining,
            reset_label(member.reset, member.reset_text.as_deref(), now)
        )
    };
    if member.five_hour_awaiting_reading {
        note.push_str(" · 5H reset due, awaiting reading");
    } else if let Some(five) = &member.five_hour {
        note.push_str(&format!(
            " · 5H {}%, resets in {}",
            quota_remaining_percent(five)
                .map(|remaining| remaining.to_string())
                .unwrap_or_else(|| "?".into()),
            reset_label(five.resets_at_epoch_seconds, five.resets.as_deref(), now)
        ));
    } else if member.remaining > 0
        && !member.awaiting_reading
        && member.five_hour_idle
        && *period == PoolPeriod::Week
    {
        note.push_str(" · 5H idle: a fresh window starts on first use");
    }
    if member.remaining > 0
        && !member.awaiting_reading
        && let Some(fable) = member.fable
    {
        note.push_str(&format!(" · Fable {fable}%"));
    }
    if let Some(extra) = &member.note {
        note.push_str(&format!(" · {extra}"));
    }
    if let Some(banked) = member.banked.filter(|banked| *banked > 0) {
        note.push_str(&format!(
            " · {}{banked} banked",
            if theme::ascii() { "B" } else { "⟲" }
        ));
    }
    refresh + &note
}

fn wrap_note(text: &str, width: usize) -> Vec<String> {
    mj_chat::components::wrap_lines(
        [Line::raw(text)],
        u16::try_from(width.max(1)).unwrap_or(u16::MAX),
    )
    .into_iter()
    .map(|line| line.to_string())
    .collect()
}

fn refill_lines(labels: Vec<String>, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut row = String::new();
    let separator = theme::glyphs().footer_separator;
    let width = width.saturating_sub(2);
    for label in labels {
        if !row.is_empty() {
            let joined = format!("{row}{separator}{label}");
            if Span::raw(joined.as_str()).width() <= width {
                row = joined;
                continue;
            }
            lines.push(Line::styled(format!("  {row}"), theme::muted()));
        }
        row = label;
        if Span::raw(row.as_str()).width() > width {
            lines.extend(
                wrap_note(&row, width)
                    .into_iter()
                    .map(|text| Line::styled(format!("  {text}"), theme::muted())),
            );
            row.clear();
        }
    }
    if !row.is_empty() {
        lines.push(Line::styled(format!("  {row}"), theme::muted()));
    }
    lines
}

type QuotaLines = (
    Vec<Line<'static>>,
    Vec<(QuotaRow, usize)>,
    Vec<(String, usize)>,
);
fn quota_lines(
    dashboard: &DashboardState,
    width: usize,
    now: u64,
    focused: bool,
    clock: fn(i64, &str) -> String,
) -> QuotaLines {
    let mut lines = Vec::new();
    let mut pool_rows = Vec::new();
    let mut profile_rows = Vec::new();
    let providers = super::quota_pools::provider_pools(
        &dashboard.config,
        &dashboard.quotas,
        &dashboard.quota_providers,
        now,
    );
    let selection = dashboard.selected_quota_row();
    for provider in providers {
        let accent = provider_accent(&provider.provider);
        lines.push(Line::styled(
            provider.provider.display_name().to_uppercase(),
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        ));
        for pool in provider.pools {
            let row = QuotaRow::Pool(pool.key.clone());
            let selected = focused && selection.as_ref() == Some(&row);
            pool_rows.push((row, lines.len()));
            let expanded = dashboard.quota_expanded.contains(&pool.key);
            let prefix = format!(
                "{}{}{}",
                if selected { ">" } else { " " },
                if expanded { "- " } else { "+ " },
                if pool.label.is_empty() {
                    String::new()
                } else {
                    format!("{} ", pool.label)
                }
            );
            let percentage = format!(
                " {:>padding$}",
                pool.percentage(),
                padding = 4 + usize::from(pool.missing > 0)
            );
            let bar_width =
                width.saturating_sub(prefix.chars().count() + percentage.chars().count());
            let mut spans = vec![Span::styled(
                prefix,
                if selected {
                    theme::selection(true)
                } else {
                    Style::default()
                },
            )];
            let (bar, refill_labels) =
                pool_bar(&pool, bar_width, now, accent, &dashboard.quota_refreshing);
            spans.extend(bar);
            spans.push(Span::styled(
                percentage,
                Style::default().fg(accent).add_modifier(Modifier::BOLD),
            ));
            if pool.members.len() == 1 {
                profile_rows.push((pool.members[0].id.clone(), lines.len()));
            }
            lines.push(Line::from(spans));
            if pool.missing > 0 {
                lines.push(Line::styled(
                    format!(
                        "  {} profile{} not reporting",
                        pool.missing,
                        if pool.missing == 1 { "" } else { "s" }
                    ),
                    theme::muted(),
                ));
            }
            // Expanded lanes already give each profile's reset and banked
            // resets; the list only supplies what a collapsed bar cannot fit.
            if !expanded {
                lines.extend(refill_lines(refill_labels, width));
            }
            // Warnings (a refused check, unknown windows) stay visible while
            // collapsed; each line names one member and opens its menu.
            for member in &pool.members {
                if let Some(note) = &member.note {
                    for text in wrap_note(
                        &format!(
                            "{} {} · {note}",
                            member.id,
                            if member.awaiting_reading {
                                "?".to_string()
                            } else {
                                format!("{}%", member.remaining)
                            }
                        ),
                        width.saturating_sub(2),
                    ) {
                        profile_rows.push((member.id.clone(), lines.len()));
                        lines.push(Line::styled(format!("  {text}"), theme::muted()));
                    }
                }
            }
            if expanded {
                let name_width = pool
                    .members
                    .iter()
                    .map(|member| member.id.chars().count())
                    .max()
                    .unwrap_or(0)
                    + 3;
                let name_width = name_width.min(width.saturating_sub(12).max(3));
                let notes: Vec<_> = pool
                    .members
                    .iter()
                    .map(|member| {
                        lane_note(
                            member,
                            &pool.key.period,
                            now,
                            clock,
                            dashboard.quota_refreshing.contains(&member.id),
                        )
                    })
                    .collect();
                let longest_note = notes
                    .iter()
                    .map(|note| Span::raw(note).width())
                    .max()
                    .unwrap_or(0);
                let lane_width = width
                    .saturating_sub(name_width + longest_note + 2)
                    .clamp(12, 48)
                    .min(width.saturating_sub(name_width));
                let mut axis = vec![' '; lane_width];
                for cell in (0..lane_width).step_by((lane_width / 6).max(6)) {
                    let label = clock(now as i64 + (cell * 86400 / lane_width) as i64, "%H:%M");
                    if cell + label.len() <= lane_width {
                        for (i, ch) in label.chars().enumerate() {
                            axis[cell + i] = ch;
                        }
                    }
                }
                lines.push(Line::styled(
                    format!("{:name_width$}{}", "", axis.into_iter().collect::<String>()),
                    theme::muted(),
                ));
                for (member, mut note) in pool.members.iter().zip(notes) {
                    profile_rows.push((member.id.clone(), lines.len()));
                    let row = QuotaRow::Lane(pool.key.clone(), member.id.clone());
                    let selected = focused && selection.as_ref() == Some(&row);
                    pool_rows.push((row.clone(), lines.len()));
                    let mut spans = vec![Span::styled(
                        format!(
                            "{} {:name$} ",
                            if selected { ">" } else { " " },
                            truncate_to_cells(&member.id, name_width - 3, Truncate::PLAIN),
                            name = name_width - 3
                        ),
                        if selected {
                            theme::selection(true)
                        } else {
                            Style::default()
                        },
                    )];
                    let timeline = lane(member, &pool.key.period, lane_width, now, accent);
                    if timeline.iter().any(|span| span.content == "*") {
                        note.push_str(&format!(
                            " · *={}+5 (both resets)",
                            pool.key.period.marker()
                        ));
                    }
                    spans.extend(timeline);
                    if name_width + lane_width + 2 + Span::raw(&note).width() <= width {
                        spans.push(Span::styled(format!("  {note}"), theme::muted()));
                        lines.push(Line::from(spans));
                    } else {
                        lines.push(Line::from(spans));
                        for text in wrap_note(&note, width.saturating_sub(4)) {
                            pool_rows.push((row.clone(), lines.len()));
                            profile_rows.push((member.id.clone(), lines.len()));
                            lines.push(Line::styled(format!("    {text}"), theme::muted()));
                        }
                    }
                }
            }
        }
        for (id, label) in provider.text {
            let label = if dashboard.quota_refreshing.contains(&id) {
                format!("refreshing{}", theme::glyphs().ellipsis)
            } else {
                label
            };
            profile_rows.push((id.clone(), lines.len()));
            let row = QuotaRow::Profile(id.clone());
            let selected = focused && selection.as_ref() == Some(&row);
            for (index, text) in wrap_note(&format!("{id}: {label}"), width.saturating_sub(2))
                .into_iter()
                .enumerate()
            {
                pool_rows.push((row.clone(), lines.len()));
                profile_rows.push((id.clone(), lines.len()));
                lines.push(Line::styled(
                    format!("{} {text}", if selected && index == 0 { ">" } else { " " }),
                    if selected {
                        theme::selection(true)
                    } else {
                        theme::muted()
                    },
                ));
            }
        }
    }
    if lines.is_empty() {
        lines.push(Line::styled(
            "Add an agent profile in Settings.",
            theme::muted(),
        ));
    }
    (lines, pool_rows, profile_rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use mj_core::config::{HarnessProfile, SymbolSet};
    use ratatui::{Terminal, backend::TestBackend};

    fn fixture() -> (DashboardState, u64) {
        let quotas: BTreeMap<String, ProfileQuota> = serde_json::from_str(include_str!(
            "../../tests/fixtures/quota-provider-readings.json"
        ))
        .unwrap();
        let now = quotas
            .values()
            .map(|quota| quota.refreshed_at_epoch_seconds)
            .max()
            .unwrap();
        let mut config = crate::test_support::config();
        config.profiles = quotas
            .iter()
            .map(|(id, quota)| {
                (
                    id.clone(),
                    HarnessProfile::new(quota.harness, format!("/quota-fixture/{id}")),
                )
            })
            .collect();
        let mut dashboard = DashboardState::new(config, Default::default(), quotas);
        dashboard.quota_providers.settle();
        dashboard.focus = Focus::Quota;
        (dashboard, now)
    }

    fn fixture_clock(epoch: i64, format: &str) -> String {
        // The readings were recorded in Chicago in November; keep the render
        // portable across CI hosts without changing the process timezone.
        chrono::DateTime::from_timestamp(epoch, 0)
            .unwrap()
            .with_timezone(&chrono::FixedOffset::west_opt(6 * 3600).unwrap())
            .format(format)
            .to_string()
    }

    fn capture(
        output: &mut String,
        label: &str,
        dashboard: &mut DashboardState,
        now: u64,
        width: u16,
        height: u16,
    ) {
        use std::fmt::Write as _;
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                render_quotas_at(frame, frame.area(), dashboard, None, now, fixture_clock);
                if theme::ascii() {
                    theme::fold_buffer_to_ascii(frame.buffer_mut());
                }
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        writeln!(output, "=== {label} ({width}x{height}) ===").unwrap();
        for y in 0..height {
            let row: String = (0..width).map(|x| buffer[(x, y)].symbol()).collect();
            writeln!(output, "{}", row.trim_end()).unwrap();
        }
        // Labels can cover fill glyphs. Record backgrounds and modifiers so
        // monochrome fill boundaries remain part of the surface contract.
        writeln!(output, "background runs (x..end, colour):").unwrap();
        for y in 0..height {
            let mut runs = Vec::new();
            let mut start = 0;
            for x in 1..=width {
                if x == width
                    || (buffer[(x, y)].bg, buffer[(x, y)].modifier)
                        != (buffer[(start, y)].bg, buffer[(start, y)].modifier)
                {
                    runs.push(format!(
                        "{start}..{x} {:?} {:?}",
                        buffer[(start, y)].bg,
                        buffer[(start, y)].modifier
                    ));
                    start = x;
                }
            }
            writeln!(output, "{y}: {}", runs.join(", ")).unwrap();
        }
    }

    // Hard-won: quota review: an indexed accent cannot be tinted, so filled and spent cells matched.
    #[test]
    fn indexed_accent_marks_filled_cells_apart_from_spent_ones() {
        theme::with_symbols(SymbolSet::Unicode, || {
            theme::with_theme(theme::UiTheme::TerminalLight, || {
                let (dashboard, now) = fixture();
                let mut pool = dashboard.quota_pools().remove(0).pools.remove(0);
                pool.members.truncate(1);
                pool.members[0].remaining = 50;
                pool.members[0].awaiting_reading = false;
                let accent = theme::palette().muted;
                assert!(!matches!(accent, Color::Rgb(..)));
                let (spans, _) = pool_bar(&pool, 10, now, accent, &Default::default());
                let style_at = |segment: usize| spans[segment].style;
                assert_ne!(
                    (style_at(0).bg, style_at(0).add_modifier),
                    (style_at(1).bg, style_at(1).add_modifier)
                );
            })
        });
    }

    // Hard-won: db4dc77c9: refresh reordered pool lanes and stranded selection on another profile.
    #[test]
    fn quota_lane_selection_survives_refresh_by_identity() {
        let (mut dashboard, now) = fixture();
        let key = dashboard.quota_rows()[0].pool().unwrap().clone();
        dashboard.toggle_quota_pool();
        dashboard.handle_dashboard_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        let selected = QuotaRow::Lane(key.clone(), "claude2".into());
        assert_eq!(dashboard.selected_quota_row(), Some(selected.clone()));
        let before = dashboard.selection_for(Focus::Quota);
        let mut ignored = String::new();
        capture(&mut ignored, "before refresh", &mut dashboard, now, 60, 6);
        let previous_offset = dashboard.quota_scroll.get();
        dashboard.recenter_on_selection.set(None);
        dashboard.set_quotas(dashboard.quotas.clone());
        capture(
            &mut ignored,
            "unchanged refresh",
            &mut dashboard,
            now,
            60,
            6,
        );
        assert_eq!(dashboard.quota_scroll.get(), previous_offset);
        // A wheel offset away from the selection survives a stationary refresh.
        dashboard.quota_scroll.set(0);
        dashboard.recenter_on_selection.set(None);
        dashboard.set_quotas(dashboard.quotas.clone());
        capture(
            &mut ignored,
            "wheel then refresh",
            &mut dashboard,
            now,
            60,
            6,
        );
        assert_eq!(dashboard.quota_scroll.get(), 0);
        let mut reports = dashboard.quotas.clone();
        reports.get_mut("claude2").unwrap().windows[0].resets_at_epoch_seconds =
            Some(now as i64 + 10 * 86400);
        dashboard.set_quota_snapshot(mj_client::quota::QuotaSnapshot {
            reports,
            ..Default::default()
        });
        assert_eq!(dashboard.selected_quota_row(), Some(selected));
        assert_ne!(dashboard.selection_for(Focus::Quota), before);
        capture(
            &mut ignored,
            "reordered refresh",
            &mut dashboard,
            now,
            60,
            6,
        );
        assert!(
            dashboard
                .quota_pool_areas
                .borrow()
                .iter()
                .any(|(row, _)| { dashboard.selected_quota_row().as_ref() == Some(row) })
        );
        assert!(dashboard.quota_scroll.get() > previous_offset);
        let mut reports = dashboard.quotas.clone();
        reports.remove("claude2");
        dashboard.set_quotas(reports);
        // Hard-won: quota review: a profile whose row changed type lost its
        // selection to the first row, so `e` renamed another profile.
        assert_eq!(
            dashboard.selected_quota_row(),
            Some(QuotaRow::Profile("claude2".into()))
        );
        dashboard.quota_selection = Some(QuotaRow::Pool(key.clone()));
        dashboard.handle_dashboard_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert!(matches!(
            dashboard.selected_quota_row(),
            Some(QuotaRow::Lane(..))
        ));
        dashboard.handle_dashboard_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            dashboard.selected_quota_row(),
            Some(QuotaRow::Pool(key.clone()))
        );
        assert!(!dashboard.quota_expanded.contains(&key));
    }

    // Hard-won: quota review: clicks used stale positions after a quota refresh reordered rows.
    #[test]
    fn quota_click_uses_rendered_identity_after_snapshot_changes() {
        let (mut dashboard, now) = fixture();
        dashboard.toggle_quota_pool();
        let mut ignored = String::new();
        capture(&mut ignored, "click", &mut dashboard, now, 132, 40);
        let (identity, area) = dashboard
            .quota_pool_areas
            .borrow()
            .iter()
            .find(|(row, _)| matches!(row, QuotaRow::Lane(_, id) if id == "claude5"))
            .unwrap()
            .clone();
        let click = crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: area.x,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        };
        let mut reports = dashboard.quotas.clone();
        reports
            .get_mut("claude5")
            .unwrap()
            .windows
            .iter_mut()
            .for_each(|window| window.resets_at_epoch_seconds = Some(now as i64));
        dashboard.set_quotas(reports);
        dashboard.handle_mouse(click);
        assert_eq!(dashboard.selected_quota_row(), Some(identity));
        let mut reports = dashboard.quotas.clone();
        reports.remove("claude5");
        dashboard.set_quotas(reports);
        let selection = dashboard.selected_quota_row();
        dashboard.handle_mouse(click);
        assert_eq!(dashboard.selected_quota_row(), selection);
    }

    #[test]
    fn golden_quota_provider_pools() {
        let mut output = String::new();
        for symbols in [SymbolSet::Unicode, SymbolSet::Ascii] {
            theme::with_symbols(symbols, || {
                theme::with_theme(theme::UiTheme::Darcula, || {
                    let (mut dashboard, now) = fixture();
                    capture(
                        &mut output,
                        &format!("{symbols:?} collapsed"),
                        &mut dashboard,
                        now,
                        132,
                        23,
                    );
                    capture(
                        &mut output,
                        &format!("{symbols:?} narrow collapsed"),
                        &mut dashboard,
                        now,
                        60,
                        30,
                    );
                    // Show missing and cached readings through the daemon's probing snapshot.
                    let reports = dashboard.quotas.clone();
                    let mut refreshing = reports.clone();
                    refreshing.remove("claude2");
                    dashboard.set_quota_snapshot(mj_client::quota::QuotaSnapshot {
                        reports: refreshing,
                        probing: ["claude2".into(), "claude5".into(), "claude6".into()].into(),
                        ..Default::default()
                    });
                    capture(
                        &mut output,
                        &format!("{symbols:?} missing and cached readings while probing"),
                        &mut dashboard,
                        now,
                        132,
                        24,
                    );
                    dashboard.set_quota_snapshot(mj_client::quota::QuotaSnapshot {
                        reports,
                        ..Default::default()
                    });
                    // Exercise the actual key dispatch and selection transition.
                    let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
                    dashboard.handle_dashboard_key(enter);
                    assert_eq!(dashboard.quota_expanded.len(), 1);
                    let google = dashboard
                        .quota_rows()
                        .iter()
                        .position(|row| matches!(row, QuotaRow::Pool(key) if key.scope.as_deref() == Some("gemini")))
                        .unwrap();
                    dashboard.set_selection_for(Focus::Quota, google);
                    dashboard.handle_dashboard_key(enter);
                    assert_eq!(dashboard.quota_expanded.len(), 2);
                    dashboard.set_selection_for(Focus::Quota, 0);
                    capture(
                        &mut output,
                        &format!("{symbols:?} expanded Anthropic and Gemini"),
                        &mut dashboard,
                        now,
                        132,
                        36,
                    );
                    dashboard.handle_dashboard_key(enter);
                    assert_eq!(dashboard.quota_expanded.len(), 1);
                    dashboard.handle_dashboard_key(enter);
                    assert_eq!(dashboard.quota_expanded.len(), 2);
                    capture(
                        &mut output,
                        &format!("{symbols:?} narrow expanded"),
                        &mut dashboard,
                        now,
                        60,
                        15,
                    );
                    capture(
                        &mut output,
                        &format!("{symbols:?} narrow expanded full Anthropic"),
                        &mut dashboard,
                        now,
                        60,
                        40,
                    );
                    dashboard.quota_expanded = dashboard
                        .quota_pools()
                        .into_iter()
                        .flat_map(|provider| provider.pools.into_iter().map(|pool| pool.key))
                        .collect();
                    dashboard.set_selection_for(Focus::Quota, 0);
                    capture(
                        &mut output,
                        &format!("{symbols:?} all periods and status labels"),
                        &mut dashboard,
                        now,
                        132,
                        68,
                    );
                    dashboard.quota_expanded.clear();
                    let last_pool = dashboard
                        .quota_rows()
                        .iter()
                        .rposition(|row| matches!(row, QuotaRow::Pool(_)))
                        .unwrap();
                    dashboard.set_selection_for(Focus::Quota, last_pool);
                    dashboard.handle_dashboard_key(enter);
                    dashboard.handle_dashboard_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
                    assert!(matches!(
                        dashboard.selected_quota_row(),
                        Some(QuotaRow::Profile(_))
                    ));
                    dashboard.handle_dashboard_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
                    assert!(
                        matches!(dashboard.selected_quota_row(), Some(QuotaRow::Lane(_, id)) if id == "muse6")
                    );
                    let last_pool = dashboard
                        .selected_quota_row()
                        .unwrap()
                        .pool()
                        .unwrap()
                        .clone();
                    capture(
                        &mut output,
                        &format!("{symbols:?} last pool final lane in short pane"),
                        &mut dashboard,
                        now,
                        132,
                        5,
                    );
                    let lane_index = dashboard
                        .quota_rows()
                        .iter()
                        .position(|row| matches!(row, QuotaRow::Lane(_, id) if id == "muse5"))
                        .unwrap();
                    let area = dashboard
                        .quota_pool_areas
                        .borrow()
                        .iter()
                        .find(|(row, _)| *row == dashboard.quota_rows()[lane_index])
                        .unwrap()
                        .1;
                    dashboard.handle_mouse(crossterm::event::MouseEvent {
                        kind: crossterm::event::MouseEventKind::Down(
                            crossterm::event::MouseButton::Left,
                        ),
                        column: area.x,
                        row: area.y,
                        modifiers: KeyModifiers::NONE,
                    });
                    assert!(
                        matches!(dashboard.selected_quota_row(), Some(QuotaRow::Lane(_, id)) if id == "muse5")
                    );
                    dashboard.handle_dashboard_key(enter);
                    assert_eq!(
                        dashboard.selected_quota_row(),
                        Some(QuotaRow::Pool(last_pool))
                    );
                    dashboard.quota_expanded.clear();
                    let short = dashboard.quota_rows().iter().position(|row| matches!(row, QuotaRow::Pool(key) if key.period == PoolPeriod::FiveHour)).unwrap();
                    dashboard.set_selection_for(Focus::Quota, short);
                    capture(
                        &mut output,
                        &format!("{symbols:?} six-cell full and spent segments"),
                        &mut dashboard,
                        now,
                        30,
                        6,
                    );
                })
            });
        }
        theme::with_symbols(SymbolSet::Unicode, || {
            theme::with_theme(theme::UiTheme::Mono, || {
                let (mut dashboard, now) = fixture();
                dashboard.toggle_quota_pool();
                capture(
                    &mut output,
                    "monochrome expanded Anthropic",
                    &mut dashboard,
                    now,
                    132,
                    30,
                );
                dashboard.quota_expanded.clear();
                let short = dashboard.quota_rows().iter().position(|row| matches!(row, QuotaRow::Pool(key) if key.period == PoolPeriod::FiveHour)).unwrap();
                dashboard.set_selection_for(Focus::Quota, short);
                capture(
                    &mut output,
                    "monochrome six-cell full and spent segments",
                    &mut dashboard,
                    now,
                    30,
                    6,
                );
            })
        });
        theme::with_symbols(SymbolSet::Unicode, || {
            let (mut dashboard, now) = fixture();
            let mut reports = dashboard.quotas.clone();
            reports.get_mut("claude7").unwrap().windows[0].remaining_percent = Some(3);
            let mut long = reports.remove("claude4").unwrap();
            let long_id = "claude-with-an-extremely-long-profile-name-that-must-not-hide-readings";
            long.profile_id = long_id.into();
            reports.insert(long_id.into(), long);
            let profile = dashboard.config.profiles.remove("claude4").unwrap();
            dashboard.config.profiles.insert(long_id.into(), profile);
            dashboard.quota_providers =
                super::super::quota_pools::ProviderDiscovery::start(&dashboard.config);
            dashboard.quota_providers.settle();
            dashboard.set_quotas(reports);
            dashboard.toggle_quota_pool();
            capture(
                &mut output,
                "small balance and long profile at narrow width",
                &mut dashboard,
                now,
                60,
                40,
            );
            let lane = dashboard
                .quota_rows()
                .iter()
                .position(|row| matches!(row, QuotaRow::Lane(_, id) if id == "claude5"))
                .unwrap();
            dashboard.set_selection_for(Focus::Quota, lane);
            capture(
                &mut output,
                "selected wrapped lane in short viewport",
                &mut dashboard,
                now,
                60,
                6,
            );
            dashboard.recenter_on_selection.set(Some(Focus::Quota));
            capture(
                &mut output,
                "wrapped lane first line in one-line viewport",
                &mut dashboard,
                now,
                60,
                3,
            );
            let selection = dashboard.selected_quota_row();
            let first = dashboard.quota_scroll.get();
            dashboard.pane_areas = Some([Rect::default(), Rect::default(), Rect::new(0, 0, 60, 3)]);
            for step in 1..=2 {
                dashboard.handle_mouse(crossterm::event::MouseEvent {
                    kind: crossterm::event::MouseEventKind::ScrollDown,
                    column: 10,
                    row: 1,
                    modifiers: KeyModifiers::NONE,
                });
                capture(
                    &mut output,
                    &format!("wrapped lane wheel continuation {step}"),
                    &mut dashboard,
                    now,
                    60,
                    3,
                );
                assert_eq!(dashboard.quota_scroll.get(), first + step);
                assert_eq!(dashboard.selected_quota_row(), selection);
            }
            // Hard-won: quota review: keyboard-only users could not reach the rest of a wrapped note.
            for (key, step) in [(KeyCode::Up, 1), (KeyCode::Up, 2), (KeyCode::PageDown, 1)] {
                dashboard.handle_dashboard_key(KeyEvent::new(key, KeyModifiers::NONE));
                capture(
                    &mut output,
                    &format!("wrapped lane key continuation {key:?} {step}"),
                    &mut dashboard,
                    now,
                    60,
                    3,
                );
                assert_eq!(dashboard.selected_quota_row(), selection);
            }
            assert_eq!(dashboard.quota_scroll.get(), first + 1);
            dashboard.quota_expanded = dashboard
                .quota_pools()
                .into_iter()
                .flat_map(|provider| provider.pools.into_iter().map(|pool| pool.key))
                .collect();
            let moonshot = dashboard
                .quota_rows()
                .iter()
                .position(|row| matches!(row, QuotaRow::Lane(_, id) if id == "kimi"))
                .unwrap();
            dashboard.set_selection_for(Focus::Quota, moonshot);
            capture(
                &mut output,
                "Month and 5H with booster notice",
                &mut dashboard,
                now,
                100,
                12,
            );
            let mut reports = dashboard.quotas.clone();
            dashboard
                .config
                .profiles
                .retain(|id, _| matches!(id.as_str(), "codex-short-full" | "codex-short-spent"));
            reports.retain(|id, _| id == "codex-short-full");
            dashboard.set_quota_snapshot(mj_client::quota::QuotaSnapshot {
                reports,
                probing: ["codex-short-spent".into()].into(),
                ..Default::default()
            });
            dashboard.set_selection_for(Focus::Quota, 0);
            capture(
                &mut output,
                "full balance with one missing subscription",
                &mut dashboard,
                now,
                70,
                9,
            );
            let minimized = minimized_quota_line(&dashboard, 110, true, now);
            // Hard-won: quota review: Kimi's Week balance vanished behind its Month window.
            let (mut dashboard, now) = fixture();
            dashboard
                .config
                .profiles
                .retain(|id, _| matches!(id.as_str(), "kimi" | "claude2" | "grok"));
            let kimi = dashboard.quotas.get_mut("kimi").unwrap();
            let mut week = kimi.windows[0].clone();
            week.label = "Week".into();
            week.remaining_percent = Some(25);
            week.resets_at_epoch_seconds = Some(now as i64 + 3 * 3600);
            kimi.windows.push(week);
            // The cached reset has passed with no newer reading.
            let stale = dashboard.quotas.get_mut("claude2").unwrap();
            stale.windows[0].remaining_percent = Some(80);
            stale.windows[0].resets_at_epoch_seconds = Some(now as i64 - 600);
            stale.refreshed_at_epoch_seconds = now - 3600;
            dashboard.set_quotas(dashboard.quotas.clone());
            dashboard.quota_expanded = dashboard
                .quota_pools()
                .into_iter()
                .flat_map(|provider| provider.pools.into_iter().map(|pool| pool.key))
                .collect();
            dashboard.set_selection_for(Focus::Quota, 0);
            capture(
                &mut output,
                "Week and Month pools, and a reset due without a reading",
                &mut dashboard,
                now,
                100,
                20,
            );
            // Judge staleness on the fixture's clock, not the wall clock.
            let pools = crate::render::quota_pools::provider_pools(
                &dashboard.config,
                &dashboard.quotas,
                &dashboard.quota_providers,
                now,
            );
            let moonshot = pools
                .iter()
                .find(|provider| provider.provider == QuotaProvider::Moonshot)
                .unwrap();
            assert_eq!(
                moonshot
                    .pools
                    .iter()
                    .map(|pool| (pool.key.period.clone(), pool.remaining))
                    .collect::<Vec<_>>(),
                [(PoolPeriod::Week, 25), (PoolPeriod::Month, 40)]
            );
            // Hard-won: quota review: foreground over a light fill used the theme's near-white colour.
            theme::with_symbols(SymbolSet::Ascii, || {
                theme::with_theme(theme::UiTheme::Midnight, || {
                    let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
                    terminal
                        .draw(|frame| {
                            render_quotas_at(
                                frame,
                                frame.area(),
                                &mut dashboard,
                                None,
                                now,
                                fixture_clock,
                            )
                        })
                        .unwrap();
                    let buffer = terminal.backend().buffer();
                    let light = provider_accent(&QuotaProvider::XAI);
                    let filled: Vec<_> = buffer
                        .content()
                        .iter()
                        .filter(|cell| cell.bg == light && cell.symbol() == "#")
                        .collect();
                    assert!(!filled.is_empty());
                    assert!(filled.iter().all(|cell| cell.fg == readable_on(light)));
                    assert_ne!(readable_on(light), theme::palette().text);
                })
            });
            use std::fmt::Write;
            writeln!(
                &mut output,
                "minimized partial totals: {}",
                minimized.to_string().trim_end()
            )
            .unwrap();
        });
        // Hard-won: quota review: unreadable long windows hid usable profiles and short limits did not block lanes.
        for symbols in [SymbolSet::Unicode, SymbolSet::Ascii] {
            theme::with_symbols(symbols, || {
                let (mut dashboard, now) = fixture();
                dashboard
                    .config
                    .profiles
                    .retain(|id, _| id == "kimi" || id.starts_with("codex"));
                let kimi = dashboard.quotas.get_mut("kimi").unwrap();
                let mut unknown = kimi.windows[0].clone();
                unknown.label = "Week".into();
                unknown.duration_seconds = Some(604800);
                unknown.remaining_percent = None;
                unknown.used = None;
                unknown.limit = Some(100);
                kimi.windows = vec![
                    unknown,
                    mj_client::quota::QuotaWindow {
                        label: "5H".into(),
                        remaining_percent: Some(70),
                        duration_seconds: Some(18000),
                        resets_at_epoch_seconds: Some(now as i64 + 7200),
                        model_scope: None,
                        used: None,
                        limit: None,
                        resets: None,
                    },
                ];
                let short_fallback = kimi.windows[1].clone();
                for (id, week, short, reset) in [
                    ("codex", 80, 0, 7200),
                    ("codex2", 0, 0, 21600),
                    ("codex3", 0, 0, 3600),
                ] {
                    let report = dashboard.quotas.get_mut(id).unwrap();
                    report.windows = vec![
                        mj_client::quota::QuotaWindow {
                            label: "Week".into(),
                            duration_seconds: Some(604800),
                            remaining_percent: Some(week),
                            resets_at_epoch_seconds: Some(now as i64 + 10800),
                            model_scope: None,
                            used: None,
                            limit: None,
                            resets: None,
                        },
                        mj_client::quota::QuotaWindow {
                            label: "5H".into(),
                            duration_seconds: Some(18000),
                            remaining_percent: Some(short),
                            resets_at_epoch_seconds: Some(now as i64 + reset),
                            model_scope: None,
                            used: None,
                            limit: None,
                            resets: None,
                        },
                    ];
                }
                let report = dashboard.quotas.get_mut("codex4").unwrap();
                let mut unknown = report.windows[0].clone();
                unknown.remaining_percent = None;
                unknown.used = None;
                report.windows = vec![unknown, short_fallback];
                dashboard.quota_expanded = dashboard
                    .quota_pools()
                    .into_iter()
                    .flat_map(|provider| provider.pools.into_iter().map(|pool| pool.key))
                    .collect();
                dashboard.set_selection_for(Focus::Quota, 0);
                capture(
                    &mut output,
                    &format!("{symbols:?} unreadable week and exhausted overlapping windows"),
                    &mut dashboard,
                    now,
                    132,
                    34,
                );
            });
        }
        for symbols in [SymbolSet::Unicode, SymbolSet::Ascii] {
            theme::with_symbols(symbols, || {
                theme::with_theme(theme::UiTheme::Mono, || {
                    let (mut dashboard, now) = fixture();
                    dashboard
                        .config
                        .profiles
                        .retain(|id, _| id.starts_with("codex-short-"));
                    let report = dashboard.quotas.get_mut("codex-short-spent").unwrap();
                    report.windows[0].remaining_percent = Some(30);
                    dashboard.set_selection_for(Focus::Quota, 0);
                    capture(
                        &mut output,
                        &format!("{symbols:?} monochrome 30 vs 100 percent under labels"),
                        &mut dashboard,
                        now,
                        30,
                        7,
                    );
                })
            });
        }
        theme::with_symbols(SymbolSet::Unicode, || {
            let (mut dashboard, now) = fixture();
            let report = dashboard.quotas.get_mut("claude5").unwrap();
            let reset = report.weekly_window().unwrap().resets_at_epoch_seconds;
            report
                .windows
                .iter_mut()
                .find(|window| window.label == "5H")
                .unwrap()
                .resets_at_epoch_seconds = reset;
            dashboard.toggle_quota_pool();
            capture(
                &mut output,
                "coincident weekly and five-hour resets",
                &mut dashboard,
                now,
                60,
                32,
            );
        });
        // Hard-won: quota review: a Month limit did not block the weekly lane, a stale
        // five-hour reading hid the weekly reset marker, and indexed accents drew
        // filled and spent cells alike.
        for symbols in [SymbolSet::Unicode, SymbolSet::Ascii] {
            theme::with_symbols(symbols, || {
                let (mut dashboard, now) = fixture();
                dashboard
                    .config
                    .profiles
                    .retain(|id, _| matches!(id.as_str(), "kimi" | "claude5"));
                let kimi = dashboard.quotas.get_mut("kimi").unwrap();
                kimi.windows[0].remaining_percent = Some(0);
                kimi.windows[0].resets_at_epoch_seconds = Some(now as i64 + 12 * 3600);
                let mut week = kimi.windows[0].clone();
                week.label = "Week".into();
                week.remaining_percent = Some(100);
                week.resets_at_epoch_seconds = Some(now as i64 + 2 * 86400);
                kimi.windows.push(week);
                let claude = dashboard.quotas.get_mut("claude5").unwrap();
                claude.refreshed_at_epoch_seconds = now - 3600;
                for window in &mut claude.windows {
                    window.resets_at_epoch_seconds = Some(now as i64 + 4 * 3600);
                    if window.label == "5H" {
                        window.resets_at_epoch_seconds = Some(now as i64 - 600);
                    }
                }
                dashboard.quota_expanded = dashboard
                    .quota_pools()
                    .into_iter()
                    .flat_map(|provider| provider.pools.into_iter().map(|pool| pool.key))
                    .collect();
                dashboard.set_selection_for(Focus::Quota, 0);
                capture(
                    &mut output,
                    &format!(
                        "{symbols:?} month limit blocks week lane; stale five-hour keeps week marker"
                    ),
                    &mut dashboard,
                    now,
                    100,
                    20,
                );
            });
        }
        for symbols in [SymbolSet::Unicode, SymbolSet::Ascii] {
            theme::with_symbols(symbols, || {
                let (mut dashboard, now) = fixture();
                dashboard.config.profiles.retain(|id, _| {
                    id.starts_with("claude") && id != "claude-aws" && id != "claude-login"
                });
                // Twelve timeline cells at this width: reset on cell starts,
                // including the first cell, must belong to exactly one cell.
                let template = dashboard.quotas["claude5"]
                    .five_hour_window()
                    .unwrap()
                    .clone();
                dashboard.quotas.get_mut("claude2").unwrap().windows[0].resets_at_epoch_seconds =
                    Some(now as i64 + 4 * 3600);
                for (id, hours) in [("claude5", 2), ("claude7", 0)] {
                    let report = dashboard.quotas.get_mut(id).unwrap();
                    let mut five = template.clone();
                    five.resets_at_epoch_seconds = Some(now as i64 + hours * 3600);
                    report.windows.retain(|window| window.label != "5H");
                    report.windows.push(five);
                }
                dashboard.toggle_quota_pool();
                capture(
                    &mut output,
                    &format!("{symbols:?} reset at cell start and banked fallback"),
                    &mut dashboard,
                    now,
                    60,
                    36,
                );
            });
        }
        // Hard-won: quota review: one-cell and omitted segments looked spent at 40%.
        for symbols in [SymbolSet::Unicode, SymbolSet::Ascii] {
            theme::with_symbols(symbols, || {
                let (mut dashboard, now) = fixture();
                dashboard.config.profiles.retain(|id, _| {
                    id.starts_with("claude") && id != "claude-aws" && id != "claude-login"
                });
                let report = dashboard.quotas.get_mut("claude5").unwrap();
                report
                    .windows
                    .iter_mut()
                    .find(|window| window.label == "Week")
                    .unwrap()
                    .remaining_percent = Some(40);
                report.monthly_price_usd = Some(1);
                report.banked_resets = Some(1);
                for (width, height) in [(60, 8), (16, 26), (11, 44)] {
                    capture(
                        &mut output,
                        &format!("{symbols:?} tiny segment remaining and banked resets"),
                        &mut dashboard,
                        now,
                        width,
                        height,
                    );
                }
            });
        }
        // Hard-won: quota review: failed probes vanished from partial totals for other periods.
        theme::with_symbols(SymbolSet::Unicode, || {
            let (mut dashboard, now) = fixture();
            dashboard
                .config
                .profiles
                .retain(|id, _| id.starts_with("codex") || id.starts_with("gemini"));
            for id in ["codex2", "gemini2"] {
                let report = dashboard.quotas.get_mut(id).unwrap();
                report.error = Some("login refused; sign in again".into());
                report.windows.clear();
            }
            dashboard.set_selection_for(Focus::Quota, 0);
            capture(
                &mut output,
                "failed probes missing from every eligible period and scope",
                &mut dashboard,
                now,
                132,
                22,
            );
            for id in ["codex2", "gemini2"] {
                dashboard.quotas.get_mut(id).unwrap().error = None;
            }
            capture(
                &mut output,
                "empty reports missing from every eligible period and scope",
                &mut dashboard,
                now,
                132,
                22,
            );
        });
        // Muse while no five-hour window is open: Meta reports nothing, so one
        // profile shows the weekly figure carried from its earlier probes and
        // the other, with none to carry, says it is idle.
        theme::with_symbols(SymbolSet::Unicode, || {
            let (mut dashboard, now) = fixture();
            dashboard
                .config
                .profiles
                .retain(|id, _| matches!(id.as_str(), "muse2" | "muse3"));
            let carried = dashboard.quotas.get_mut("muse2").unwrap();
            carried.extra = None;
            carried.windows = vec![mj_client::quota::QuotaWindow {
                label: "Week".into(),
                remaining_percent: Some(78),
                duration_seconds: Some(604800),
                model_scope: None,
                used: None,
                limit: None,
                resets: None,
                resets_at_epoch_seconds: Some(now as i64 + 3 * 86400),
            }];
            let idle = dashboard.quotas.get_mut("muse3").unwrap();
            idle.windows.clear();
            idle.extra = Some(mj_client::quota::IDLE_LABEL.into());
            dashboard.quota_expanded = dashboard
                .quota_pools()
                .into_iter()
                .flat_map(|provider| provider.pools.into_iter().map(|pool| pool.key))
                .collect();
            dashboard.set_selection_for(Focus::Quota, 0);
            capture(
                &mut output,
                "idle Muse with and without a weekly reading",
                &mut dashboard,
                now,
                100,
                10,
            );
        });
        mj_core::golden::assert_golden(env!("CARGO_MANIFEST_DIR"), "quota-provider-pools", &output);
    }
}
