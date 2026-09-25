use super::*;

/// How many machines an EC2 fleet is running, as the fleet's answer to the
/// "In Use" question.
///
/// A fleet gets one probe per live instance, so the probe list is the fleet's
/// size. A fleet has no CPU percentage of its own, and how many machines are
/// up is what it costs.
pub(crate) fn fleet_vm_label(detail: &CapacityDetail) -> String {
    let count = detail.target.probes.len();
    crate::widgets::counted(count, "VM", "VMs")
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
        // A probe that never produced a reading already shows "unavailable"
        // in the capacity column; only say more when stale numbers are still
        // on screen and could be mistaken for current.
        return detail.usage.is_some().then(|| format!("stale: {error}"));
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

pub(crate) struct CapacityTableRow {
    host: String,
    targets: Line<'static>,
    in_use: Line<'static>,
}

pub(crate) fn capacity_table_rows(
    dashboard: &DashboardState,
    now_epoch_seconds: u64,
) -> Vec<CapacityTableRow> {
    dashboard
        .capacity_details
        .values()
        .map(|detail| {
            let staleness = capacity_staleness(detail, now_epoch_seconds);
            let mut in_use = if detail.refreshing {
                vec![Span::styled(
                    format!("refreshing{}", theme::glyphs().ellipsis),
                    theme::muted(),
                )]
            } else {
                match (&detail.target.kind, &detail.usage) {
                    (DeploymentCapacityKind::Host, Some(usage)) => {
                        let reading_style = |headroom| {
                            Style::default().fg(if staleness.is_some() {
                                theme::palette().muted
                            } else {
                                headroom_color(headroom)
                            })
                        };
                        let memory_percent = if usage.memory_total_bytes == 0 {
                            0
                        } else {
                            (u128::from(usage.memory_used_bytes) * 100
                                / u128::from(usage.memory_total_bytes))
                            .min(100)
                        };
                        let cpu = usage.cpu_percent.map_or_else(
                            || Span::styled("CPU unavailable", theme::muted()),
                            |cpu| {
                                Span::styled(
                                    format!("{cpu}% CPU"),
                                    reading_style(100_u8.saturating_sub(cpu)),
                                )
                            },
                        );
                        let memory = if usage.memory_total_bytes == 0 {
                            Span::styled("RAM unavailable", theme::muted())
                        } else {
                            Span::styled(
                                format!("{memory_percent}% RAM"),
                                reading_style(100_u8.saturating_sub(memory_percent as u8)),
                            )
                        };
                        vec![
                            cpu,
                            Span::styled(theme::footer_separator(), theme::muted()),
                            memory,
                        ]
                    }
                    (DeploymentCapacityKind::AwsFleet, Some(usage)) => {
                        vec![Span::raw(format!(
                            "{} · {} cores · {} RAM · {} disk",
                            fleet_vm_label(detail),
                            usage.logical_cores,
                            format_resource_bytes(usage.memory_total_bytes),
                            format_resource_bytes(usage.disk_total_bytes.unwrap_or(0))
                        ))]
                    }
                    // A fleet with nothing running has no capacity figures,
                    // and the count is the whole answer.
                    (DeploymentCapacityKind::AwsFleet, None) if detail.on_demand => {
                        vec![Span::raw(fleet_vm_label(detail))]
                    }
                    _ => vec![Span::styled("unavailable", theme::muted())],
                }
            };
            if let Some(staleness) = staleness {
                in_use.push(Span::styled(
                    format!("  · {staleness}"),
                    Style::default().fg(theme::palette().muted),
                ));
            }
            CapacityTableRow {
                host: detail.target.host.clone(),
                targets: capacity_target_labels(&detail.target.target_ids, dashboard),
                in_use: Line::from(in_use),
            }
        })
        .collect()
}

pub(crate) fn capacity_column_widths(rows: &[CapacityTableRow]) -> [u16; 3] {
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
    // On the combined surface (`size` is set) the title opens the pane's
    // menu, and says so with a dropdown mark.
    let label = crate::surface_controls::pane_title_label("Targets", size.is_some());
    let pressed = size.is_some_and(|_| {
        let title_budget = pane_title_content_width(
            area.width,
            dashboard.pane_maximize_enabled(SupportPane::Targets),
        );
        crate::surface_controls::register_pane_title_menu(
            dashboard,
            SupportPane::Targets,
            Rect::new(
                area.x.saturating_add(1),
                area.y,
                u16::try_from(Line::raw(label.as_str()).width())
                    .unwrap_or(u16::MAX)
                    .min(title_budget),
                area.height.min(1),
            ),
        )
    });
    let block = theme::panel(focused).title(if pressed {
        Span::styled(label, theme::selection(true))
    } else {
        Span::raw(label)
    });
    let block = size.map_or(block.clone(), |size| {
        block.title(pane_size_controls(
            size,
            dashboard.pane_maximize_enabled(SupportPane::Targets),
        ))
    });
    let table = Table::new(
        rows.into_iter().map(|row| {
            Row::new([
                Cell::from(row.host).style(Style::default().add_modifier(Modifier::BOLD)),
                Cell::from(row.targets).style(theme::muted()),
                Cell::from(row.in_use),
            ])
        }),
        column_widths.map(Constraint::Length),
    )
    .column_spacing(2)
    .header(
        Row::new(["Host / fleet", "Targets", "In Use"])
            .style(theme::muted().patch(theme::raised())),
    )
    .row_highlight_style(if focused {
        theme::selection(true)
    } else {
        Style::default()
    })
    .highlight_symbol(if focused {
        theme::glyphs().selected
    } else {
        "  "
    })
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
    if dashboard.capacity_details.is_empty() {
        let message = if dashboard.config.targets.is_empty() {
            "Add a target in Settings."
        } else {
            "Waiting for target readings."
        };
        frame.render_widget(
            Paragraph::new(message)
                .style(theme::muted())
                .wrap(Wrap { trim: true }),
            Rect::new(
                area.x.saturating_add(3),
                area.y.saturating_add(2),
                area.width.saturating_sub(4),
                area.height.saturating_sub(3),
            ),
        );
    }
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
pub(crate) fn headroom_color(headroom_percent: u8) -> Color {
    match headroom_percent {
        0..=20 => theme::palette().error,
        21..=50 => theme::palette().warning,
        _ => theme::palette().success,
    }
}
