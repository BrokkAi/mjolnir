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
    pressed: bool,
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
            if super::capacity::all_listed_targets_unavailable(
                &detail.reading.target_ids,
                dashboard,
            ) {
                return SummaryReading {
                    name: detail.reading.host.clone(),
                    value: "unavailable".to_string(),
                    color: None,
                };
            }
            if detail.refreshing {
                return SummaryReading {
                    name: detail.reading.host.clone(),
                    value: format!("refreshing{}", theme::glyphs().ellipsis),
                    color: None,
                };
            }
            let reading = &detail.reading;
            let (value, cpu_percent) = match (reading.kind, reading.has_reading) {
                (DeploymentCapacityKind::Host, true) => match reading.cpu_percent {
                    Some(cpu) => (format!("{cpu}%"), Some(cpu)),
                    None => ("no CPU".to_string(), None),
                },
                (DeploymentCapacityKind::AwsFleet, _) if reading.fleet_instances.is_some() => {
                    (reading.fleet_machines_label(), None)
                }
                _ => ("unavailable".to_string(), None),
            };
            let stale = reading.staleness(now_epoch_seconds).is_some();
            SummaryReading {
                name: detail.reading.host.clone(),
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
    summary_row("Targets", &readings, width, pressed)
}

/// A weighted reading for each independently metered provider pool.
pub(crate) fn minimized_quota_line(
    dashboard: &DashboardState,
    width: u16,
    pressed: bool,
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
    summary_row("Profiles", &readings, width, pressed)
}

/// A minimized pane's single row, drawn as the pane's own title so it keeps
/// the rule the full pane has. Readings are comma-separated and truncated
/// rather than wrapped, because the row is one row. The label is the pane's
/// dropdown button, drawn pressed while the pointer holds it down.
pub(crate) fn summary_row(
    label: &str,
    readings: &[SummaryReading],
    width: u16,
    pressed: bool,
) -> Line<'static> {
    // A minimized pane is still a pane, so its one row opens the way a
    // bordered one does and the rule carries on between the label and the
    // readings: `─ Profiles ▾ ── claude-1 63% ────`.
    let (opening, divider) = ("─", "── ");
    let title = crate::surface_controls::pane_title_label(label, true);
    let mut used = opening.chars().count() + title.chars().count() + divider.chars().count();
    let mut spans = vec![
        Span::raw(opening),
        if pressed {
            Span::styled(title, theme::selection(true))
        } else {
            Span::raw(title)
        },
        Span::raw(divider),
    ];
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
    render_quotas_at(frame, area, dashboard, size, now);
}

/// The title's refresh status. The age is the oldest reading among profiles
/// the daemon is free to check; a profile whose check was refused is waiting
/// for its next check, which a refresh does not bring forward, so it is named
/// as pending instead of holding the age back.
pub(crate) fn quota_refresh_status(
    quotas: &std::collections::BTreeMap<String, ProfileQuota>,
    now: u64,
) -> String {
    let pending = |quota: &ProfileQuota| quota.next_check_at().is_some_and(|next| next > now);
    let age = quotas
        .values()
        .filter(|quota| !pending(quota))
        .map(|quota| quota.refreshed_at_epoch_seconds)
        .min()
        // Every profile is waiting: show how old the oldest reading is.
        .or_else(|| {
            quotas
                .values()
                .map(|quota| quota.refreshed_at_epoch_seconds)
                .min()
        });
    let waiting = quotas
        .iter()
        .filter(|(_, quota)| pending(quota))
        .map(|(id, _)| id.as_str())
        .collect::<Vec<_>>();
    let refreshed = age.map_or_else(
        || "not refreshed".to_string(),
        |refreshed| format!("refreshed {}", refresh_age(now, refreshed)),
    );
    if waiting.is_empty() {
        refreshed
    } else {
        format!("{refreshed}, pending: {}", waiting.join(", "))
    }
}

pub(crate) fn render_quotas_at(
    frame: &mut Frame,
    area: Rect,
    dashboard: &mut DashboardState,
    size: Option<PaneSize>,
    now: u64,
) {
    // Provider discovery is polled once per frame by the combined renderer,
    // before pane sizing, so sizing and drawing use the same snapshot.
    let refresh_status = if !dashboard.quota_refreshing.is_empty() {
        format!("refreshing{}", theme::glyphs().ellipsis)
    } else {
        quota_refresh_status(&dashboard.quotas, now)
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
        quota_lines(dashboard, usize::from(inner.width), now, focused);
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

use super::quota_pools::{
    PoolMember, PoolPeriod, QuotaPool, QuotaRow, profile_status, provider_status_summary,
    provider_text_expanded, refill_widths,
};
use mj_core::config::QuotaProvider;

#[allow(
    clippy::disallowed_methods,
    reason = "Provider colours paint both foreground and background against the active theme; monochrome uses the theme palette."
)]
pub(crate) fn provider_accent(provider: &QuotaProvider) -> Color {
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

fn pool_bar(
    pool: &QuotaPool,
    width: usize,
    now: u64,
    accent: Color,
    refreshing: &std::collections::BTreeSet<String>,
) -> Vec<Span<'static>> {
    let percentage = pool.percentage();
    let suffix = format!(
        " {:>padding$}",
        percentage,
        padding = 4 + usize::from(pool.missing > 0)
    );
    let suffix_width = suffix.len();
    let suffix = || {
        Span::styled(
            suffix,
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        )
    };
    let weight = pool.reported_weight();
    if weight == 0.0 {
        return vec![
            Span::raw(" ".repeat(width.saturating_sub(suffix_width))),
            suffix(),
        ];
    }
    let refills: Vec<_> = pool
        .reported_members()
        .filter(|member| member.remaining < 100 || refreshing.contains(&member.id))
        .map(|member| (member, member.weight * f64::from(100 - member.remaining)))
        .collect();
    let available = pool.available_capacity();
    let apportion = |width| {
        refill_widths(
            available,
            refills.iter().map(|&(_, capacity)| capacity),
            weight * 100.0,
            width,
        )
    };
    let label_width = |label: &str| Span::raw(label).width();
    let percentage_width = label_width(&percentage);
    let mut widths = apportion(width);
    let has_refill = widths[1..].iter().any(|&cells| cells > 0);
    let inline = percentage_width <= widths[0].saturating_sub(usize::from(has_refill));
    if !inline {
        widths = apportion(width.saturating_sub(suffix_width));
    }
    let last_segment = widths.iter().rposition(|&cells| cells > 0).unwrap_or(0);
    let ascii = theme::ascii();
    let pane = theme::palette().surface;
    let use_glyphs =
        ascii || theme::current() == theme::UiTheme::Mono || !matches!(accent, Color::Rgb(..));
    let refill = tint(accent, 0.24);
    // Text or a glyph on a segment colour. `readable_on` can only contrast
    // with an RGB colour; for a theme colour (and in monochrome) it returns
    // the theme text colour, which may be the segment colour itself, so draw
    // in the segment colour on the pane instead.
    let mono = theme::current() == theme::UiTheme::Mono;
    let on = move |colour: Color| {
        if matches!(colour, Color::Rgb(..)) && !mono {
            Style::default().fg(readable_on(colour)).bg(colour)
        } else {
            Style::default().fg(colour).bg(pane)
        }
    };
    let divider = |colour| {
        if ascii {
            Span::styled("|", on(colour))
        } else {
            // The segment keeps this cell; its right eighth exposes the pane.
            Span::styled("▉", Style::default().fg(colour).bg(pane))
        }
    };
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
    // ASCII fills in reverse video so a percentage printed over the whole
    // fill keeps the fill's look; `on` keeps its two colours distinct.
    let fill_style = if ascii {
        on(accent).add_modifier(Modifier::REVERSED)
    } else if use_glyphs {
        Style::default().fg(accent).bg(pane)
    } else {
        Style::default().fg(readable_on(accent)).bg(accent)
    };
    let mut spans = Vec::new();
    if widths[0] > 0 {
        let split = last_segment > 0;
        let mut cells = widths[0] - usize::from(split);
        if inline {
            let label_style = if use_glyphs && !ascii {
                Style::default()
                    .fg(readable_on(pane))
                    .bg(pane)
                    .add_modifier(Modifier::REVERSED)
            } else if ascii {
                fill_style
            } else {
                on(accent)
            };
            spans.push(Span::styled(percentage, label_style));
            cells -= percentage_width;
        }
        if cells > 0 {
            spans.push(Span::styled(fill_glyph.repeat(cells), fill_style));
        }
        if split {
            spans.push(divider(accent));
        }
    }
    for (index, ((member, _), cells)) in refills
        .into_iter()
        .zip(widths.into_iter().skip(1))
        .enumerate()
    {
        let is_refreshing = refreshing.contains(&member.id);
        let status = if is_refreshing {
            format!("refreshing{}", theme::glyphs().ellipsis)
        } else {
            reset_label(member.reset, member.reset_text.as_deref(), now)
        };
        let banked = member
            .banked
            .filter(|&count| count > 0)
            .map(|count| format!("{}{count}", if ascii { "B" } else { "⟲" }));
        let split = index + 1 < last_segment;
        // Keep a divider cell at a boundary, and a refill glyph at the end
        // of a glyph bar when text would otherwise cover the segment.
        let label_budget = cells.saturating_sub(usize::from(split || use_glyphs));
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
                    short_status.to_string()
                }
            } else {
                short_status.to_string()
            }
        } else {
            String::new()
        };
        if cells == 0 {
            continue;
        }
        let body = cells - usize::from(split);
        if body > 0 {
            let glyph = if use_glyphs { "=" } else { " " };
            spans.push(Span::styled(
                format!("{}{label}", glyph.repeat(body - label_width(&label))),
                on(refill),
            ));
        }
        if split {
            spans.push(divider(refill));
        }
    }
    if !inline {
        spans.push(suffix());
    }
    spans
}

fn member_has_idle_five_hour(member: &PoolMember, period: &PoolPeriod) -> bool {
    member.five_hour.is_none()
        && !member.five_hour_awaiting_reading
        && member.five_hour_idle
        && *period == PoolPeriod::Week
}

fn other_long_blocked(member: &PoolMember, now: u64) -> bool {
    member
        .other_long_blocked_until
        .is_some_and(|until| until >= 0 && until as u64 > now)
}

fn fable_same_reset(member: &PoolMember, fable: &QuotaWindow) -> bool {
    match (fable.resets_at_epoch_seconds, member.reset) {
        (None, None) => fable.resets == member.reset_text,
        (fable, primary) => fable == primary,
    }
}

fn has_fable_row(member: &PoolMember) -> bool {
    member.fable.as_ref().is_some_and(|fable| {
        !(member.remaining == 0
            && !member.awaiting_reading
            && !member.fable_awaiting_reading
            && quota_remaining_percent(fable) == Some(0)
            && fable_same_reset(member, fable))
    })
}

fn hairline_bar(percent: u8, width: usize, fill: Color) -> Vec<Span<'static>> {
    let palette = theme::palette();
    let (full, empty, halves) = if theme::ascii() {
        let mut cells = (usize::from(percent) * width + 50) / 100;
        if percent > 0 && width > 0 {
            cells = cells.max(1);
        }
        if percent < 100 && width >= 2 {
            cells = cells.min(width - 1);
        }
        ("=", "-", cells * 2)
    } else {
        let mut halves = (usize::from(percent) * width * 2 + 50) / 100;
        if (1..100).contains(&percent) && width > 0 {
            halves = halves.clamp(1, width * 2 - 1);
        }
        ("━", "─", halves)
    };
    let mut spans = vec![Span::styled(
        full.repeat(halves / 2),
        Style::default().fg(fill),
    )];
    if halves % 2 != 0 {
        spans.push(Span::styled("╸", Style::default().fg(fill)));
    }
    spans.push(Span::styled(
        empty.repeat(width - halves.div_ceil(2)),
        Style::default().fg(palette.border),
    ));
    spans
}

fn cell_width(text: &str) -> usize {
    Span::raw(text).width()
}

fn padded_cell(text: &str, width: usize) -> String {
    let text = truncate_to_cells(text, width, Truncate::PLAIN);
    let padding = " ".repeat(width.saturating_sub(cell_width(&text)));
    format!("{text}{padding}")
}

struct DetailReading {
    percent: Option<u8>,
    reset: String,
}

impl DetailReading {
    fn new(percent: Option<u8>, reset: String, awaiting: bool) -> Self {
        if awaiting {
            Self {
                percent: None,
                reset: "due".into(),
            }
        } else {
            Self { percent, reset }
        }
    }

    fn fill(&self, normal: Color) -> Color {
        if self
            .percent
            .is_some_and(|percent| (1..=15).contains(&percent))
        {
            theme::palette().error
        } else {
            normal
        }
    }

    fn percent_style(&self, fill: Color, bold: bool) -> Style {
        let style = if self.percent.is_none_or(|percent| percent == 0) {
            theme::muted()
        } else {
            Style::default().fg(fill)
        };
        if bold {
            style.add_modifier(Modifier::BOLD)
        } else {
            style
        }
    }

    fn spans(
        &self,
        bar_width: usize,
        reset_width: usize,
        fill: Color,
        percent_style: Style,
        reset_style: Style,
    ) -> Vec<Span<'static>> {
        let mut spans = hairline_bar(self.percent.unwrap_or(0), bar_width, fill);
        let percent = self
            .percent
            .map_or_else(|| "   ?".into(), |percent| format!("{percent:>3}%"));
        spans.push(Span::styled(format!(" {percent} "), percent_style));
        spans.push(Span::styled(
            padded_cell(&self.reset, reset_width),
            reset_style,
        ));
        spans
    }
}

enum FiveHourDetail {
    Reading {
        reading: DetailReading,
        blocked: bool,
    },
    Due,
    Ready,
    Unavailable,
}

struct MemberReadings {
    primary: DetailReading,
    fable: Option<DetailReading>,
    five_hour: FiveHourDetail,
    other_long_blocked: bool,
}

impl MemberReadings {
    fn new(member: &PoolMember, period: &PoolPeriod, now: u64) -> Self {
        let primary = DetailReading::new(
            Some(member.remaining),
            reset_label(member.reset, member.reset_text.as_deref(), now),
            member.awaiting_reading,
        );
        let other_long_blocked = other_long_blocked(member, now);
        let blocked = other_long_blocked || (member.remaining == 0 && !member.awaiting_reading);
        let five_hour = if member.five_hour_awaiting_reading {
            FiveHourDetail::Due
        } else if let Some(five) = &member.five_hour {
            FiveHourDetail::Reading {
                reading: DetailReading::new(
                    quota_remaining_percent(five),
                    reset_label(five.resets_at_epoch_seconds, five.resets.as_deref(), now),
                    false,
                ),
                blocked,
            }
        } else if !blocked && !member.awaiting_reading && member_has_idle_five_hour(member, period)
        {
            FiveHourDetail::Ready
        } else {
            FiveHourDetail::Unavailable
        };
        let fable = member
            .fable
            .as_ref()
            .filter(|_| has_fable_row(member))
            .map(|fable| {
                let reset = if fable.resets_at_epoch_seconds.is_some() || fable.resets.is_some() {
                    reset_label(fable.resets_at_epoch_seconds, fable.resets.as_deref(), now)
                } else {
                    String::new()
                };
                DetailReading::new(
                    quota_remaining_percent(fable),
                    reset,
                    member.fable_awaiting_reading,
                )
            });
        Self {
            primary,
            fable,
            five_hour,
            other_long_blocked,
        }
    }
}

struct PoolDetail {
    width: usize,
    name_width: usize,
    primary_bar: usize,
    primary_reset: usize,
    five_bar: usize,
    five_reset: usize,
    bank: usize,
    gap: usize,
    wrap_secondary: bool,
    wrap_bank: bool,
    readings: Vec<MemberReadings>,
}

impl PoolDetail {
    const INDENT: &'static str = "    ";
    const MIN_BAR: usize = 6;
    const READING_SPACING: usize = 6; // Four cells for the percentage and two spaces.

    fn new(pool: &QuotaPool, width: usize, now: u64) -> Self {
        let period = &pool.key.period;
        let readings: Vec<_> = pool
            .members
            .iter()
            .map(|member| MemberReadings::new(member, period, now))
            .collect();
        let show_five_hour = *period != PoolPeriod::FiveHour
            && readings
                .iter()
                .any(|reading| !matches!(reading.five_hour, FiveHourDetail::Unavailable));
        let bank = pool
            .members
            .iter()
            .filter_map(|member| member.banked)
            .filter(|&num| num > 0)
            .map(|num| (num.to_string().len() + 1).max(4))
            .max()
            .unwrap_or(0);
        let primary_reset = readings
            .iter()
            .flat_map(|reading| std::iter::once(&reading.primary).chain(reading.fable.as_ref()))
            .map(|reading| cell_width(&reading.reset))
            .max()
            .unwrap_or(0)
            .clamp(5, 7);
        let five_reset = if show_five_hour {
            readings
                .iter()
                .filter_map(|reading| match &reading.five_hour {
                    FiveHourDetail::Reading { reading, .. } => Some(cell_width(&reading.reset)),
                    _ => None,
                })
                .max()
                .unwrap_or(0)
                .clamp(5, 7)
        } else {
            0
        };

        let fable_name = if readings.iter().any(|reading| reading.fable.is_some()) {
            9
        } else {
            0
        };
        let desired_name = pool
            .members
            .iter()
            .map(|member| cell_width(&member.id))
            .max()
            .unwrap_or(0)
            .max(fable_name);
        let primary_target = if show_five_hour { 12 } else { 16 };
        let five_target = if show_five_hour { 10 } else { 0 };
        let primary_fixed = 4 + Self::READING_SPACING + primary_reset;
        let five_fixed = if show_five_hour {
            Self::READING_SPACING + five_reset
        } else {
            0
        };
        let columns = usize::from(show_five_hour) + usize::from(bank > 0);
        let min_bars = Self::MIN_BAR * (1 + usize::from(show_five_hour));
        let fixed = primary_fixed + five_fixed + bank;
        let minimum_name = desired_name.min(9);
        let name_budget = |available: usize| {
            let preserve_name = minimum_name.min(available.saturating_sub(1));
            desired_name.min(available.saturating_sub(Self::MIN_BAR).max(preserve_name))
        };
        let bounded_name = if columns == 0 {
            name_budget(width.saturating_sub(primary_fixed))
        } else {
            desired_name.min(width.saturating_sub(fixed + 2 * columns + min_bars))
        };
        let wrap_secondary = columns > 0 && width < minimum_name + fixed + 2 * columns + min_bars;
        let (name_width, primary_bar, five_bar, gap, wrap_bank) = if wrap_secondary {
            let name = name_budget(width.saturating_sub(primary_fixed));
            let primary = primary_target.min(width.saturating_sub(name + primary_fixed));
            let available = width.saturating_sub(Self::INDENT.len() + five_fixed);
            let wrap_bank = show_five_hour && bank > 0 && available < Self::MIN_BAR + 2 + bank;
            let bank_space = if show_five_hour && bank > 0 && !wrap_bank {
                2 + bank
            } else {
                0
            };
            let five = five_target.min(available.saturating_sub(bank_space));
            (name, primary, five, 2, wrap_bank)
        } else {
            let gap = if bounded_name + fixed + 3 * columns + primary_target + five_target <= width
            {
                3
            } else {
                2
            };
            let available = width.saturating_sub(bounded_name + fixed + gap * columns);
            let (primary, five) = if show_five_hour {
                // Share the remaining cells in proportion to each bar's target above its minimum.
                let extra = (available - min_bars).min(primary_target + five_target - min_bars);
                let primary_extra = (extra * (primary_target - Self::MIN_BAR)
                    + (primary_target + five_target - min_bars) / 2)
                    / (primary_target + five_target - min_bars);
                (
                    Self::MIN_BAR + primary_extra,
                    Self::MIN_BAR + extra - primary_extra,
                )
            } else {
                (available.min(primary_target), 0)
            };
            (bounded_name, primary, five, gap, false)
        };

        Self {
            width,
            name_width,
            primary_bar,
            primary_reset,
            five_bar,
            five_reset,
            bank,
            gap,
            wrap_secondary,
            wrap_bank,
            readings,
        }
    }

    fn has_secondary(&self) -> bool {
        self.five_reset > 0 || self.bank > 0
    }

    fn primary_end(&self) -> usize {
        self.name_width + 4 + self.primary_bar + Self::READING_SPACING + self.primary_reset
    }

    fn five_width(&self) -> usize {
        self.five_bar + Self::READING_SPACING + self.five_reset
    }

    fn append_secondary(
        &self,
        primary: &mut Vec<Span<'static>>,
        five: Vec<Span<'static>>,
        bank: Option<Span<'static>>,
    ) -> Vec<Line<'static>> {
        let mut continuation = Vec::new();
        let mut following = Vec::new();
        let target = if self.wrap_secondary {
            if !five.is_empty() || (bank.is_some() && !self.wrap_bank) {
                continuation.push(Span::raw(Self::INDENT));
            }
            &mut continuation
        } else {
            primary.push(Span::raw(" ".repeat(self.gap)));
            primary
        };
        let has_five = !five.is_empty();
        target.extend(five);
        if let Some(bank) = bank {
            if self.wrap_bank {
                following.push(Line::from(vec![Span::raw(Self::INDENT), bank]));
            } else {
                if has_five {
                    target.push(Span::raw(" ".repeat(self.gap)));
                }
                target.push(bank);
            }
        }
        if !continuation.is_empty() {
            following.insert(0, Line::from(continuation));
        }
        following
    }

    fn headers(&self, period: &PoolPeriod) -> Vec<Line<'static>> {
        let title = period.label().to_ascii_uppercase();
        let mut primary = vec![Span::styled(
            format!(
                "{}{}",
                " ".repeat(self.name_width + 4),
                padded_cell(&title, self.primary_end() - self.name_width - 4)
            ),
            theme::muted(),
        )];
        let five = if self.five_reset > 0 {
            vec![Span::styled(
                padded_cell("5H", self.five_width()),
                theme::muted(),
            )]
        } else {
            Vec::new()
        };
        let bank = (self.bank > 0).then(|| Span::styled("BANK", theme::muted()));
        let mut following = self.append_secondary(&mut primary, five, bank);
        following.insert(0, Line::from(primary));
        following
    }

    fn five_hour_spans(&self, five: &FiveHourDetail, accent: Color) -> Vec<Span<'static>> {
        let palette = theme::palette();
        let (text, color) = match five {
            FiveHourDetail::Reading { reading, blocked } => {
                let fill = if *blocked {
                    palette.muted
                } else {
                    reading.fill(accent)
                };
                let percent_style = if reading.percent == Some(0) {
                    Style::default()
                        .fg(palette.error)
                        .add_modifier(Modifier::BOLD)
                } else {
                    reading.percent_style(fill, !blocked && reading.percent.is_some())
                };
                return reading.spans(
                    self.five_bar,
                    self.five_reset,
                    fill,
                    percent_style,
                    theme::muted(),
                );
            }
            FiveHourDetail::Due => ("due", palette.warning),
            FiveHourDetail::Ready => ("ready", palette.success),
            FiveHourDetail::Unavailable => (if theme::ascii() { "-" } else { "—" }, palette.muted),
        };
        vec![Span::styled(
            padded_cell(text, self.five_width()),
            Style::default().fg(color),
        )]
    }

    fn member_lines(
        &self,
        member: &PoolMember,
        readings: &MemberReadings,
        selected: bool,
        refreshing: bool,
        accent: Color,
    ) -> Vec<Line<'static>> {
        let palette = theme::palette();
        let usable = member.remaining > 0 && !member.awaiting_reading;
        let name_style = if selected {
            theme::selection(true)
        } else if usable {
            Style::default().fg(palette.text)
        } else {
            theme::muted()
        };
        let selection = if selected { ">" } else { " " };
        let name = padded_cell(&member.id, self.name_width);
        let mut spans = vec![
            Span::styled(format!("{selection} {name}"), name_style),
            Span::raw("  "),
        ];
        let primary = &readings.primary;
        let fill = primary.fill(accent);
        spans.extend(primary.spans(
            self.primary_bar,
            self.primary_reset,
            fill,
            primary.percent_style(fill, usable),
            theme::muted(),
        ));
        let secondary = if self.has_secondary() {
            let five = if self.five_reset > 0 {
                self.five_hour_spans(&readings.five_hour, accent)
            } else {
                Vec::new()
            };
            self.append_secondary(&mut spans, five, banked_span(member))
        } else {
            Vec::new()
        };

        let mut notes = Vec::new();
        if refreshing {
            notes.push(format!("refreshing{}", theme::glyphs().ellipsis));
        }
        if member.awaiting_reading {
            notes.push("reset due, awaiting reading".into());
        }
        if member.five_hour_awaiting_reading {
            notes.push("5H reset due, awaiting reading".into());
        }
        if member.fable_awaiting_reading {
            notes.push("Fable reset due, awaiting reading".into());
        }
        if readings.other_long_blocked {
            notes.push("blocked by other limit".into());
        }
        if let Some(note) = &member.note {
            notes.push(note.clone());
        }
        // Keep provider-supplied reset text readable without widening the countdown columns.
        let mut full_reset = |label: &str, reset: &str, width| {
            if cell_width(reset) > width {
                notes.push(format!("{label} {reset}"));
            }
        };
        full_reset("Resets", &primary.reset, self.primary_reset);
        if let Some(fable) = &readings.fable {
            full_reset("Fable resets", &fable.reset, self.primary_reset);
        }
        if self.five_reset > 0
            && let FiveHourDetail::Reading { reading, .. } = &readings.five_hour
        {
            full_reset("5H resets", &reading.reset, self.five_reset);
        }
        let note = notes.join(" · ");
        let inline_note = !self.has_secondary()
            && self.primary_end() + self.gap + cell_width(&note) <= self.width;
        if inline_note && !note.is_empty() {
            spans.push(Span::styled(
                format!("{}{note}", " ".repeat(self.gap)),
                theme::muted(),
            ));
        }
        let mut lines = vec![Line::from(spans)];
        lines.extend(secondary);

        if let Some(fable) = &readings.fable {
            let label = if theme::ascii() {
                "  > fable"
            } else {
                "  ↳ fable"
            };
            let blocked = member.remaining == 0 && !member.awaiting_reading;
            let (label_color, fill) = if blocked {
                (palette.muted, palette.muted)
            } else {
                (palette.warning, fable.fill(palette.warning))
            };
            let same_reset = member
                .fable
                .as_ref()
                .is_some_and(|window| fable_same_reset(member, window));
            let reset_color = if same_reset {
                palette.border
            } else {
                palette.muted
            };
            let mut spans = vec![Span::styled(
                format!("  {}  ", padded_cell(label, self.name_width)),
                Style::default().fg(label_color),
            )];
            spans.extend(fable.spans(
                self.primary_bar,
                self.primary_reset,
                fill,
                fable.percent_style(fill, !blocked && fable.percent.is_some_and(|pct| pct <= 15)),
                Style::default().fg(reset_color),
            ));
            let annotation = if blocked && fable.percent != Some(0) {
                Some(("week 0%", palette.muted))
            } else if !member.awaiting_reading
                && fable.percent.is_some_and(|pct| pct < member.remaining)
            {
                let text = if theme::ascii() {
                    "< fable cap"
                } else {
                    "◀ fable cap"
                };
                let color = if fable.percent <= Some(15) {
                    palette.error
                } else {
                    palette.warning
                };
                Some((text, color))
            } else {
                None
            };
            if let Some((text, color)) = annotation
                && self.primary_end() + self.gap + cell_width(text) <= self.width
            {
                spans.push(Span::styled(
                    format!("{}{text}", " ".repeat(self.gap)),
                    Style::default().fg(color),
                ));
            }
            lines.push(Line::from(spans));
        }

        if !inline_note && !note.is_empty() {
            let indent = &Self::INDENT[..Self::INDENT.len().min(self.width.saturating_sub(1))];
            for text in wrap_note(&note, self.width.saturating_sub(indent.len())) {
                lines.push(Line::styled(format!("{indent}{text}"), theme::muted()));
            }
        }
        lines
    }
}

fn banked_span(member: &PoolMember) -> Option<Span<'static>> {
    let banked = member.banked.filter(|&banked| banked > 0)?;
    let symbol = if theme::ascii() { "B" } else { "⟲" };
    Some(Span::styled(
        format!("{symbol}{banked}"),
        Style::default()
            .fg(theme::palette().warning)
            .add_modifier(Modifier::BOLD),
    ))
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

fn quota_group_prefix(selected: bool, expanded: bool) -> &'static str {
    match (selected, expanded) {
        (true, true) => ">- ",
        (true, false) => ">+ ",
        (false, true) => " - ",
        (false, false) => " + ",
    }
}

type QuotaLines = (
    Vec<Line<'static>>,
    Vec<(QuotaRow, usize)>,
    Vec<(String, usize)>,
);
fn quota_lines(dashboard: &DashboardState, width: usize, now: u64, focused: bool) -> QuotaLines {
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
        let text_expanded = provider_text_expanded(&provider, &dashboard.quota_expanded);
        if let Some(row) = provider.text_header() {
            let selected = focused && selection.as_ref() == Some(&row);
            let summary = provider_status_summary(&provider, &dashboard.quota_refreshing);
            for (index, text) in wrap_note(&summary, width.saturating_sub(3))
                .into_iter()
                .enumerate()
            {
                pool_rows.push((row.clone(), lines.len()));
                if let [(id, _)] = provider.text.as_slice() {
                    profile_rows.push((id.clone(), lines.len()));
                }
                let prefix = if index == 0 {
                    quota_group_prefix(selected, text_expanded)
                } else {
                    "   "
                };
                lines.push(Line::styled(
                    format!("{prefix}{text}"),
                    if selected {
                        theme::selection(true)
                    } else {
                        theme::muted()
                    },
                ));
            }
        }
        for pool in &provider.pools {
            let row = QuotaRow::Pool(pool.key.clone());
            let selected = focused && selection.as_ref() == Some(&row);
            pool_rows.push((row, lines.len()));
            let expanded = pool.is_expanded(&dashboard.quota_expanded);
            let mut prefix = quota_group_prefix(selected, expanded).to_owned();
            if !pool.label.is_empty() {
                prefix.push_str(&pool.label);
                prefix.push(' ');
            }
            let bar_width = width.saturating_sub(prefix.chars().count());
            let mut spans = vec![Span::styled(
                prefix,
                if selected {
                    theme::selection(true)
                } else {
                    Style::default()
                },
            )];
            spans.extend(pool_bar(
                pool,
                bar_width,
                now,
                accent,
                &dashboard.quota_refreshing,
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
            if expanded {
                let detail = PoolDetail::new(pool, width, now);
                if detail.has_secondary() {
                    lines.extend(detail.headers(&pool.key.period));
                }
                for (member, readings) in pool.members.iter().zip(&detail.readings) {
                    let row = QuotaRow::Lane(pool.key.clone(), member.id.clone());
                    let selected = focused && selection.as_ref() == Some(&row);
                    let refreshing = dashboard.quota_refreshing.contains(&member.id);
                    for line in detail.member_lines(member, readings, selected, refreshing, accent)
                    {
                        pool_rows.push((row.clone(), lines.len()));
                        profile_rows.push((member.id.clone(), lines.len()));
                        lines.push(line);
                    }
                }
            }
        }

        for (id, label) in provider.text.into_iter().filter(|_| text_expanded) {
            let label = profile_status(&id, &label, &dashboard.quota_refreshing);
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
    use super::super::quota_pools::QuotaGroup;
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use mj_core::config::{HarnessProfile, SymbolSet};
    use ratatui::{Terminal, backend::TestBackend};

    // Hard-won: 8253cfeee: tiny refills took their minimum cells from the
    // available fill, drawing a 97% pool as 70% of a 20-cell bar.
    #[test]
    fn refill_minimum_cells_never_shrink_the_available_fill() {
        let widths = super::super::quota_pools::refill_widths(
            200.0 * 100.0,
            std::iter::repeat_n(100.0, 6),
            206.0 * 100.0,
            20,
        );
        assert_eq!(widths.iter().sum::<usize>(), 20);
        // 97% of 20 cells; refill segments cannot shrink the available fill.
        assert!(widths[0] >= 19, "{widths:?}");
    }

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
                render_quotas_at(frame, frame.area(), dashboard, None, now);
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
                pool.remaining = 50;
                let accent = theme::palette().muted;
                assert!(!matches!(accent, Color::Rgb(..)));
                let spans = pool_bar(&pool, 20, now, accent, &Default::default());
                let available = spans
                    .iter()
                    .find(|span| span.content.contains('█'))
                    .unwrap();
                let refill = spans
                    .iter()
                    .find(|span| span.content.contains('='))
                    .unwrap();
                // Filled and spent cells must look different: glyph or colour.
                let look =
                    |span: &Span<'_>| (span.content.chars().next(), span.style.fg, span.style.bg);
                assert_ne!(look(available), look(refill));
            })
        });
    }

    #[test]
    fn collapsed_quota_pool_hides_narrow_refills_banked_profiles_and_notes() {
        let (mut dashboard, now) = fixture();
        let ids = [
            "claude2",
            "claude5",
            "claude6",
            "claude7",
            "claude-aws",
            "claude-login",
        ];
        dashboard
            .config
            .profiles
            .retain(|id, _| ids.contains(&id.as_str()));
        dashboard.quotas.get_mut("claude2").unwrap().windows[0].resets_at_epoch_seconds =
            Some(now as i64 - 1);
        let banked = dashboard.quotas.get_mut("claude5").unwrap();
        banked.windows[1].remaining_percent = Some(100);
        banked.extra = Some("Booster balance".into());
        dashboard.quotas.get_mut("claude7").unwrap().windows[0].remaining_percent = Some(99);

        let render = |dashboard: &DashboardState, width| {
            let (lines, _, _) = quota_lines(dashboard, width, now, true);
            lines
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n")
        };
        let collapsed = render(&dashboard, 22);
        for id in ids {
            assert!(!collapsed.contains(id), "{collapsed}");
        }
        assert!(
            collapsed.contains("2 profiles not reporting"),
            "{collapsed}"
        );
        assert_eq!(dashboard.quota_rows().len(), 1);
        dashboard.toggle_quota_pool();
        let expanded = render(&dashboard, 26);
        for label in [
            "claude2",
            "claude5",
            "claude-aws",
            "claude-login",
            "Booster balance",
            "pay-per-use",
        ] {
            assert!(expanded.contains(label), "{expanded}");
        }
        assert_eq!(expanded.matches("Booster balance").count(), 1);
        dashboard.quota_selection = Some(QuotaRow::Profile("claude-login".into()));
        dashboard.toggle_quota_pool();
        assert!(matches!(
            dashboard.selected_quota_row(),
            Some(QuotaRow::Pool(_))
        ));
        assert_eq!(render(&dashboard, 22), collapsed);
    }

    #[test]
    fn provider_text_quota_header_toggles_with_keyboard_and_wrapped_mouse_hit() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

        let (mut dashboard, now) = fixture();
        dashboard
            .config
            .profiles
            .retain(|id, _| id == "claude5" || id == "dsh");
        let mut unreadable = dashboard.quotas["claude-login"].clone();
        unreadable.profile_id = "claude5".into();
        unreadable.extra = Some("Log in again to check usage".into());
        let reading = dashboard
            .quotas
            .insert("claude5".into(), unreadable.clone())
            .unwrap();
        let header = QuotaRow::ProviderText(mj_core::config::QuotaProvider::Anthropic);
        dashboard.quota_selection = Some(header.clone());

        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        dashboard.handle_dashboard_key(enter);
        assert!(
            dashboard
                .quota_rows()
                .contains(&QuotaRow::Profile("claude5".into()))
        );
        assert_eq!(dashboard.selected_quota_row(), Some(header.clone()));
        let mut output = String::new();
        capture(&mut output, "wrapped status", &mut dashboard, now, 30, 22);
        let continuation = dashboard
            .quota_pool_areas
            .borrow()
            .iter()
            .filter(|(row, _)| row == &header)
            .nth(1)
            .unwrap()
            .1;
        assert!(
            dashboard
                .quota_row_areas
                .borrow()
                .iter()
                .any(|(id, area)| id == "claude5" && *area == continuation)
        );
        dashboard.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: continuation.x,
            row: continuation.y,
            modifiers: KeyModifiers::NONE,
        });
        assert!(
            !dashboard
                .quota_rows()
                .contains(&QuotaRow::Profile("claude5".into()))
        );
        assert_eq!(dashboard.selected_quota_row(), Some(header.clone()));
        assert_eq!(dashboard.quota_rows().len(), 2);

        dashboard.apply_quota(reading);
        assert!(
            matches!(dashboard.selected_quota_row(), Some(QuotaRow::Pool(key))
            if key.provider == mj_core::config::QuotaProvider::Anthropic)
        );
        dashboard.apply_quota(unreadable);
        assert_eq!(dashboard.selected_quota_row(), Some(header));
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
        assert!(!dashboard.quota_expanded.contains(&QuotaGroup::Pool(key)));
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
                    capture(
                        &mut output,
                        &format!("{symbols:?} wrapped columns below minimum width"),
                        &mut dashboard,
                        now,
                        44,
                        32,
                    );
                    dashboard.quota_expanded = dashboard
                        .quota_pools()
                        .into_iter()
                        .flat_map(|provider| {
                            provider
                                .pools
                                .into_iter()
                                .map(|pool| QuotaGroup::Pool(pool.key))
                        })
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
            reports.get_mut("claude5").unwrap().extra = Some(
                "Refreshing the subscription reading is currently unavailable. The provider refused the latest check. The last reading is still shown.".into()
            );
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
            let row = dashboard.quota_rows()[lane].clone();
            let (lines, pool_rows, profile_rows) = quota_lines(&dashboard, 58, now, true);
            let [_, fable, ..] = pool_rows
                .iter()
                .filter_map(|(candidate, index)| (candidate == &row).then_some(*index))
                .collect::<Vec<_>>()[..]
            else {
                panic!("Claude's Fable line has no selection row");
            };
            assert!(lines[fable].to_string().starts_with("    ↳ fable"));
            assert!(
                profile_rows
                    .iter()
                    .any(|(id, index)| id == "claude5" && *index == fable)
            );
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
                .flat_map(|provider| {
                    provider
                        .pools
                        .into_iter()
                        .map(|pool| QuotaGroup::Pool(pool.key))
                })
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
            dashboard
                .quotas
                .get_mut("kimi")
                .unwrap()
                .windows
                .retain(|window| window.label != "5H");
            capture(
                &mut output,
                "Month without 5H shows the booster notice inline",
                &mut dashboard,
                now,
                94,
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
            let minimized = minimized_quota_line(&dashboard, 110, false, now);
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
                .flat_map(|provider| {
                    provider
                        .pools
                        .into_iter()
                        .map(|pool| QuotaGroup::Pool(pool.key))
                })
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
                            render_quotas_at(frame, frame.area(), &mut dashboard, None, now)
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
                    ("codex3", 0, 9, 3600),
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
                    .flat_map(|provider| {
                        provider
                            .pools
                            .into_iter()
                            .map(|pool| QuotaGroup::Pool(pool.key))
                    })
                    .collect();
                dashboard.set_selection_for(Focus::Quota, 0);
                // A live 5H throttle and a spent Week both read 0%, but only the
                // throttle is red and bold. The active Week uses OpenAI's color.
                let (lines, _, _) = quota_lines(&dashboard, 130, now, true);
                let profile_line = |name: &str| {
                    lines
                        .iter()
                        .find(|line| {
                            line.spans
                                .first()
                                .is_some_and(|span| span.content.trim() == name)
                        })
                        .unwrap()
                };
                let balance_styles = |name| {
                    profile_line(name)
                        .spans
                        .iter()
                        .filter(|span| span.content.trim_end().ends_with('%'))
                        .map(|span| span.style)
                        .collect::<Vec<_>>()
                };
                let bold = |color| Style::default().fg(color).add_modifier(Modifier::BOLD);
                assert_eq!(
                    balance_styles("codex"),
                    [
                        bold(provider_accent(&QuotaProvider::OpenAI)),
                        bold(theme::palette().error),
                    ]
                );
                assert_eq!(
                    balance_styles("codex2"),
                    [theme::muted(), bold(theme::palette().error)]
                );
                assert_eq!(balance_styles("codex3"), [theme::muted(), theme::muted()]);
                assert!(
                    profile_line("codex3")
                        .to_string()
                        .trim_end()
                        .ends_with("9% 1h")
                );
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
            theme::with_theme(theme::UiTheme::Light, || {
                let (mut dashboard, now) = fixture();
                let report = dashboard.quotas.get_mut("claude5").unwrap();
                let reset = report.weekly_window().unwrap().resets_at_epoch_seconds;
                report
                    .windows
                    .iter_mut()
                    .find(|window| window.label == "5H")
                    .unwrap()
                    .resets_at_epoch_seconds = reset;
                report
                    .windows
                    .iter_mut()
                    .find(|window| window.model_scope.as_deref() == Some("fable"))
                    .unwrap()
                    .resets_at_epoch_seconds = Some(now as i64 - 4);
                let overdue = dashboard.quotas.get_mut("claude2").unwrap();
                overdue.refreshed_at_epoch_seconds = now - 3600;
                overdue.windows[0].resets_at_epoch_seconds = Some(now as i64 - 4);
                let fallback = &mut dashboard.quotas.get_mut("claude7").unwrap().windows[1];
                fallback.resets_at_epoch_seconds = None;
                fallback.resets = Some("Nov 24 at 1pm".into());
                let staggered = &mut dashboard.quotas.get_mut("claude").unwrap().windows[1];
                staggered.remaining_percent = Some(0);
                staggered.resets_at_epoch_seconds = Some(now as i64 + 2 * 86400);
                dashboard.toggle_quota_pool();
                let (lines, rows, _) = quota_lines(&dashboard, 74, now, true);
                let profile_lines = |name| {
                    rows.iter()
                        .filter(|(row, _)| matches!(row, QuotaRow::Lane(_, id) if id == name))
                        .map(|(_, index)| lines[*index].to_string())
                        .collect::<Vec<_>>()
                };
                let staggered = profile_lines("claude");
                assert_eq!(staggered.len(), 2, "{staggered:?}");
                assert!(staggered[1].contains("↳ fable"), "{staggered:?}");
                assert!(
                    staggered[1].trim_end().ends_with("0% 2d0h"),
                    "{staggered:?}"
                );
                let same_reset = profile_lines("claude4");
                assert_eq!(same_reset.len(), 1, "{same_reset:?}");
                assert!(same_reset[0].trim_end().ends_with('—'), "{same_reset:?}");
                capture(
                    &mut output,
                    "Light · coincident Week and 5H, overdue Fable, and reset text",
                    &mut dashboard,
                    now,
                    76,
                    32,
                );
            });
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
                    .flat_map(|provider| {
                        provider
                            .pools
                            .into_iter()
                            .map(|pool| QuotaGroup::Pool(pool.key))
                    })
                    .collect();
                dashboard.set_selection_for(Focus::Quota, 0);
                capture(
                    &mut output,
                    &format!(
                        "{symbols:?} month limit blocks 5H on the Week row; overdue 5H preserves Week"
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
                    &format!("{symbols:?} 5H reset at now and banked balance"),
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
                .flat_map(|provider| {
                    provider
                        .pools
                        .into_iter()
                        .map(|pool| QuotaGroup::Pool(pool.key))
                })
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
        // A Pro plan reports Gemini 4 Argon as its own pool; a plan that does
        // not keeps its Gemini reading in the same Gemini pool and is not
        // missing from Argon's.
        theme::with_symbols(SymbolSet::Unicode, || {
            use mj_core::config::AntigravityQuotaPool;
            let (mut dashboard, now) = fixture();
            dashboard
                .config
                .profiles
                .retain(|id, _| id.starts_with("gemini"));
            let separate = dashboard.quotas.get_mut("gemini").unwrap();
            for window in &mut separate.windows {
                if mj_client::quota::antigravity_pool_of_window(window)
                    == Some(AntigravityQuotaPool::Gemini)
                {
                    window.model_scope =
                        Some(AntigravityQuotaPool::Gemini.reported_scope(true).into());
                }
            }
            separate.windows.push(mj_client::quota::QuotaWindow {
                label: "Week".into(),
                remaining_percent: Some(64),
                duration_seconds: Some(604800),
                model_scope: Some(AntigravityQuotaPool::Argon.reported_scope(true).into()),
                used: None,
                limit: None,
                resets: None,
                resets_at_epoch_seconds: Some(now as i64 + 6 * 86400 + 23 * 3600),
            });
            dashboard.quota_expanded = dashboard
                .quota_pools()
                .into_iter()
                .flat_map(|provider| {
                    provider
                        .pools
                        .into_iter()
                        .map(|pool| QuotaGroup::Pool(pool.key))
                })
                .collect();
            dashboard.set_selection_for(Focus::Quota, 0);
            capture(
                &mut output,
                "Antigravity with and without a separate Argon pool",
                &mut dashboard,
                now,
                132,
                16,
            );
        });
        mj_core::golden::assert_golden(env!("CARGO_MANIFEST_DIR"), "quota-provider-pools", &output);
    }
}
