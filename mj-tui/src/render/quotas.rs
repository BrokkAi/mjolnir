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
) -> Line<'static> {
    let mut readings = Vec::new();
    let mut statuses = Vec::new();
    for provider in dashboard.quota_pools() {
        for pool in provider.pools {
            readings.push(SummaryReading {
                name: if pool.label.is_empty() {
                    provider.provider.display_name().into()
                } else {
                    format!("{} {}", provider.provider.display_name(), pool.label)
                },
                value: format!(
                    "{}%{}",
                    pool.remaining,
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
                "refreshing".into()
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
    dashboard
        .quota_pools()
        .iter()
        .map(|provider| {
            1 + provider.text.len()
                + provider
                    .pools
                    .iter()
                    .map(|pool| {
                        1 + pool
                            .members
                            .iter()
                            .filter(|member| member.note.is_some())
                            .count()
                            + if dashboard.quota_expanded.contains(&pool.key) {
                                1 + pool.members.len()
                            } else {
                                0
                            }
                    })
                    .sum::<usize>()
        })
        .sum::<usize>()
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
    if let Some(error) = dashboard.quota_providers.poll(&dashboard.config) {
        dashboard.notices.set(error);
    }
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
    let mut offset = dashboard
        .quota_scroll
        .get()
        .min(lines.len().saturating_sub(viewport));
    if take_selection_recenter(dashboard, Focus::Quota)
        && let Some(row) = pool_rows.get(dashboard.quota_index)
    {
        if *row < offset {
            offset = *row;
        }
        if *row >= offset + viewport {
            offset = row.saturating_add(1).saturating_sub(viewport);
        }
    }
    frame.render_widget(
        Paragraph::new(lines.clone()).scroll((u16::try_from(offset).unwrap_or(u16::MAX), 0)),
        inner,
    );
    dashboard.quota_scroll.set(offset);
    *dashboard.quota_pool_areas.borrow_mut() = pool_rows
        .iter()
        .enumerate()
        .filter(|(_, row)| **row >= offset && **row < offset + viewport)
        .map(|(index, row)| {
            (
                index,
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

use super::quota_pools::{PoolMember, QuotaPool, segment_widths};
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

fn pool_bar(pool: &QuotaPool, width: usize, now: u64, accent: Color) -> Vec<Span<'static>> {
    let gaps = pool
        .members
        .len()
        .saturating_sub(1)
        .min(width.saturating_sub(pool.members.len().min(width)));
    let widths = segment_widths(&pool.members, width.saturating_sub(gaps));
    let mut spans = Vec::new();
    for (index, (member, cells)) in pool.members.iter().zip(widths).enumerate() {
        if index > 0 && index <= gaps {
            spans.push(Span::raw(if theme::ascii() { "|" } else { " " }));
        }
        let filled = (cells * usize::from(member.remaining) + 50) / 100;
        let mut label = vec![' '; cells];
        if cells >= 6 {
            let mut text = format!(
                "{}{}",
                if theme::ascii() { "r" } else { "↻" },
                reset_duration(member.reset, now)
            );
            if let Some(banked) = member.banked.filter(|banked| *banked > 0) {
                let extra = format!(" {}{banked}", if theme::ascii() { "B" } else { "⟲" });
                if text.chars().count() + extra.chars().count() < cells {
                    text.push_str(&extra);
                }
            }
            let text: Vec<_> = text.chars().collect();
            if text.len() <= cells {
                label[cells - text.len()..].copy_from_slice(&text);
            }
        }
        for (cell, ch) in label.into_iter().enumerate() {
            spans.push(Span::styled(
                if ch != ' ' {
                    ch.to_string()
                } else if theme::ascii() {
                    if cell < filled {
                        "#".into()
                    } else {
                        ".".into()
                    }
                } else if theme::current() == theme::UiTheme::Mono {
                    if cell < filled {
                        "█".into()
                    } else {
                        "░".into()
                    }
                } else {
                    " ".into()
                },
                Style::default()
                    .fg(theme::palette().text)
                    .bg(if cell < filled {
                        accent
                    } else {
                        tint(accent, 0.22)
                    }),
            ));
        }
    }
    spans
}

fn lane(member: &PoolMember, cells: usize, now: u64, accent: Color) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for cell in 0..cells {
        let t0 = now.saturating_add(cell as u64 * 86400 / cells as u64);
        let t1 = now.saturating_add((cell + 1) as u64 * 86400 / cells as u64);
        let contains = |reset: Option<i64>| {
            reset.is_some_and(|reset| reset >= 0 && t0 <= reset as u64 && (reset as u64) < t1)
        };
        let blocked = member.remaining == 0 && member.reset.is_none_or(|reset| reset > t0 as i64);
        let five = member.five_hour.as_ref().filter(|window| {
            window
                .resets_at_epoch_seconds
                .is_some_and(|reset| reset > t0 as i64)
        });
        let marker = if contains(member.reset) {
            Some('W')
        } else if !blocked && five.is_some_and(|window| contains(window.resets_at_epoch_seconds)) {
            Some('5')
        } else {
            None
        };
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

fn lane_note(member: &PoolMember, now: u64, clock: fn(i64, &str) -> String) -> String {
    let mut note = if member.remaining == 0 {
        format!(
            "week back {} ({})",
            member
                .reset
                .map(|reset| clock(reset, "%a %H:%M"))
                .unwrap_or_else(|| "unknown".into()),
            reset_duration(member.reset, now)
        )
    } else if let Some(five) = &member.five_hour {
        format!(
            "5H {}%, resets in {}",
            quota_remaining_percent(five)
                .map(|remaining| remaining.to_string())
                .unwrap_or_else(|| "?".into()),
            reset_duration(five.resets_at_epoch_seconds, now)
        )
    } else {
        "5H idle: a fresh window starts on first use".into()
    };
    if member.remaining > 0
        && let Some(fable) = member.fable
    {
        note.push_str(&format!(" · Fable {fable}%"));
    }
    if let Some(extra) = &member.note {
        note.push_str(&format!(" · {extra}"));
    }
    note
}

type QuotaLines = (Vec<Line<'static>>, Vec<usize>, Vec<(String, usize)>);
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
    for provider in providers {
        let accent = provider_accent(&provider.provider);
        lines.push(Line::styled(
            provider.provider.display_name().to_uppercase(),
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        ));
        for pool in provider.pools {
            let selected = focused && pool_rows.len() == dashboard.quota_index;
            pool_rows.push(lines.len());
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
            let bar_width = width.saturating_sub(prefix.chars().count() + 5);
            let mut spans = vec![Span::styled(
                prefix,
                if selected {
                    theme::selection(true)
                } else {
                    Style::default()
                },
            )];
            spans.extend(pool_bar(&pool, bar_width, now, accent));
            spans.push(Span::styled(
                format!(" {:>3}%", pool.remaining),
                Style::default().fg(accent).add_modifier(Modifier::BOLD),
            ));
            if pool.members.len() == 1 {
                profile_rows.push((pool.members[0].id.clone(), lines.len()));
            }
            lines.push(Line::from(spans));
            for member in &pool.members {
                if let Some(note) = &member.note {
                    lines.push(Line::styled(
                        format!("  {} {}% · {note}", member.id, member.remaining),
                        theme::muted(),
                    ));
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
                let lane_width = width
                    .saturating_sub(name_width + 49)
                    // The agreed prototype uses 48 half-hour cells.
                    .clamp(24, 48)
                    .min(width.saturating_sub(name_width));
                let mut axis = vec![' '; lane_width];
                for cell in (0..lane_width).step_by((lane_width / 6).max(5)) {
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
                for member in &pool.members {
                    profile_rows.push((member.id.clone(), lines.len()));
                    let mut spans = vec![Span::raw(format!(
                        "  {:name$} ",
                        member.id,
                        name = name_width - 3
                    ))];
                    spans.extend(lane(member, lane_width, now, accent));
                    spans.push(Span::styled(
                        format!("  {}", lane_note(member, now, clock)),
                        theme::muted(),
                    ));
                    lines.push(Line::from(spans));
                }
            }
        }
        for (id, label) in provider.text {
            let label = if dashboard.quota_refreshing.contains(&id) {
                "refreshing".into()
            } else {
                label
            };
            profile_rows.push((id.clone(), lines.len()));
            lines.push(Line::styled(format!("  {id}: {label}"), theme::muted()));
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
        // Text cannot distinguish a full segment from a spent segment. Record
        // background runs as part of the whole-surface golden as well.
        writeln!(output, "background runs (x..end, colour):").unwrap();
        for y in 0..height {
            let mut runs = Vec::new();
            let mut start = 0;
            for x in 1..=width {
                if x == width || buffer[(x, y)].bg != buffer[(start, y)].bg {
                    runs.push(format!("{start}..{x} {:?}", buffer[(start, y)].bg));
                    start = x;
                }
            }
            writeln!(output, "{y}: {}", runs.join(", ")).unwrap();
        }
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
                    // Exercise the actual key dispatch and selection transition.
                    let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
                    dashboard.handle_dashboard_key(enter);
                    assert_eq!(dashboard.quota_expanded.len(), 1);
                    let google = dashboard
                        .quota_pool_rows()
                        .iter()
                        .position(|pool| pool.key.scope.as_deref() == Some("gemini"))
                        .unwrap();
                    dashboard.set_selection_for(Focus::Quota, google);
                    dashboard.handle_dashboard_key(enter);
                    assert_eq!(dashboard.quota_expanded.len(), 2);
                    dashboard.quota_index = 0;
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
            })
        });
        mj_core::golden::assert_golden(env!("CARGO_MANIFEST_DIR"), "quota-provider-pools", &output);
    }
}
