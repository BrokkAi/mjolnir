use super::*;

use mj_chat::components::ControlKind;

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
    cpu: Line<'static>,
    memory: Line<'static>,
    /// The one-line Disks button, which opens filesystem details in a popup.
    disks: Line<'static>,
    staleness: Option<String>,
}

impl CapacityTableRow {
    /// Width of the Disks button. Zero when the row has no storage reading.
    fn disks_button_width(&self) -> u16 {
        u16::try_from(self.disks.width()).unwrap_or(u16::MAX)
    }
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
            let (cpu, memory) = capacity_cpu_and_memory(detail, staleness.is_some());
            CapacityTableRow {
                host: detail.target.host.clone(),
                targets: capacity_target_labels(&detail.target.target_ids, dashboard),
                cpu,
                memory,
                disks: capacity_disks_cell(dashboard, detail),
                staleness,
            }
        })
        .collect()
}

/// The CPU and RAM cells of one row. A host reports how much of each is in
/// use; a fleet has no percentage of its own and reports what it is running.
fn capacity_cpu_and_memory(detail: &CapacityDetail, stale: bool) -> (Line<'static>, Line<'static>) {
    if detail.refreshing {
        return (
            Line::styled(
                format!("refreshing{}", theme::glyphs().ellipsis),
                theme::muted(),
            ),
            Line::default(),
        );
    }
    let unavailable = || Line::styled("unavailable", theme::muted());
    match (&detail.target.kind, &detail.usage) {
        (DeploymentCapacityKind::Host, Some(usage)) => {
            let reading = |used_percent: u8| {
                Line::styled(
                    format!("{used_percent}%"),
                    Style::default().fg(if stale {
                        theme::palette().muted
                    } else {
                        headroom_color(100_u8.saturating_sub(used_percent))
                    }),
                )
            };
            let cpu = usage.cpu_percent.map_or_else(unavailable, reading);
            let memory = if usage.memory_total_bytes == 0 {
                unavailable()
            } else {
                let percent = (u128::from(usage.memory_used_bytes) * 100
                    / u128::from(usage.memory_total_bytes))
                .min(100);
                reading(u8::try_from(percent).unwrap_or(100))
            };
            (cpu, memory)
        }
        (DeploymentCapacityKind::AwsFleet, Some(usage)) => (
            Line::raw(format!(
                "{} cores on {}",
                usage.logical_cores,
                fleet_vm_label(detail)
            )),
            Line::raw(format_resource_bytes(usage.memory_total_bytes)),
        ),
        // A fleet with nothing running has no capacity figures, and the
        // count is the whole answer.
        (DeploymentCapacityKind::AwsFleet, None) if detail.on_demand => {
            (Line::raw(fleet_vm_label(detail)), Line::default())
        }
        _ => (unavailable(), Line::default()),
    }
}

/// The Disks cell of one row. Free space comes from the daemon's storage
/// owner, the same verdict that refuses writes and holds recovery back.
///
/// The cell is one line: `Disks ▾` while every filesystem is fine, or the
/// filesystem in the worst condition, as `/srv low ▾`. Details are in the
/// anchored dropdown.
fn capacity_disks_cell(dashboard: &DashboardState, detail: &CapacityDetail) -> Line<'static> {
    use mj_core::targets::storage::StorageCondition;
    let fleet = detail.target.kind == DeploymentCapacityKind::AwsFleet;
    let filesystems = dashboard
        .capacity_storage(detail)
        .into_iter()
        .flat_map(|view| {
            view.filesystems.iter().map(move |filesystem| {
                let place = if fleet {
                    format!("{} {}", view.host, filesystem.space.mount)
                } else {
                    filesystem.space.mount.clone()
                };
                (place, filesystem)
            })
        })
        .collect::<Vec<_>>();
    if filesystems.is_empty() {
        return Line::default();
    }
    let mark = if dashboard.capacity_disks_menu_open(&detail.target.id) {
        theme::glyphs().dropup
    } else {
        theme::glyphs().dropdown
    };
    let worst = filesystems
        .iter()
        .filter(|(_, filesystem)| filesystem.condition != StorageCondition::Ok)
        .max_by_key(|(_, filesystem)| {
            (
                filesystem.condition,
                std::cmp::Reverse(filesystem.space.available_bytes),
            )
        });
    match worst {
        Some((place, filesystem)) => Line::styled(
            format!(
                "{place}{} {mark}",
                storage_condition_flag(filesystem.condition)
            ),
            storage_condition_style(filesystem.condition),
        ),
        None => Line::raw(format!("Disks {mark}")),
    }
}

fn storage_condition_style(condition: mj_core::targets::storage::StorageCondition) -> Style {
    use mj_core::targets::storage::StorageCondition;
    match condition {
        StorageCondition::Full => Style::default().fg(theme::palette().error),
        StorageCondition::Low => Style::default().fg(theme::palette().warning),
        StorageCondition::Ok => Style::default(),
    }
}

fn storage_condition_flag(condition: mj_core::targets::storage::StorageCondition) -> &'static str {
    use mj_core::targets::storage::StorageCondition;
    match condition {
        StorageCondition::Full => " full",
        StorageCondition::Low => " low",
        StorageCondition::Ok => "",
    }
}

/// Filesystem details for the dropdown, styled like the former expanded rows.
pub(crate) fn capacity_disks_menu_lines(
    dashboard: &DashboardState,
    detail: &CapacityDetail,
) -> Vec<Line<'static>> {
    let fleet = detail.target.kind == DeploymentCapacityKind::AwsFleet;
    dashboard
        .capacity_storage(detail)
        .into_iter()
        .flat_map(|view| {
            view.filesystems.iter().map(move |filesystem| {
                let place = if fleet {
                    format!("{} {}", view.host, filesystem.space.mount)
                } else {
                    filesystem.space.mount.clone()
                };
                Line::styled(
                    format!(
                        "{place} {} free of {}{}",
                        format_resource_bytes(filesystem.space.available_bytes),
                        format_resource_bytes(filesystem.space.total_bytes),
                        storage_condition_flag(filesystem.condition)
                    ),
                    storage_condition_style(filesystem.condition),
                )
            })
        })
        .collect()
}

/// The Targets table always has one line per row; filesystem details are
/// rendered in a dropdown above the table.
pub(crate) fn capacity_table_lines(dashboard: &DashboardState) -> usize {
    dashboard.capacity_details.len()
}

/// The Disks column has no heading: its cell already says `Disks ▾`, or
/// names the filesystem it is about.
const CAPACITY_HEADERS: [&str; 5] = ["Host / fleet", "Targets", "CPU", "RAM", ""];

/// Column widths of the Targets table. A sixth, untitled column says why a
/// reading is stale, and exists only while some row's reading is.
pub(crate) fn capacity_column_widths(rows: &[CapacityTableRow]) -> Vec<u16> {
    let mut widths = vec![
        quota_column_width(
            CAPACITY_HEADERS[0],
            rows.iter().map(|row| Line::raw(row.host.as_str()).width()),
            u16::MAX,
        ),
        quota_column_width(
            CAPACITY_HEADERS[1],
            rows.iter().map(|row| row.targets.width()),
            u16::MAX,
        ),
        quota_column_width(
            CAPACITY_HEADERS[2],
            rows.iter().map(|row| row.cpu.width()),
            u16::MAX,
        ),
        quota_column_width(
            CAPACITY_HEADERS[3],
            rows.iter().map(|row| row.memory.width()),
            u16::MAX,
        ),
        quota_column_width(
            CAPACITY_HEADERS[4],
            rows.iter().map(|row| row.disks.width()),
            u16::MAX,
        ),
    ];
    if rows.iter().any(|row| row.staleness.is_some()) {
        widths.push(quota_column_width(
            "",
            rows.iter()
                .filter_map(|row| row.staleness.as_deref())
                .map(|staleness| Line::raw(staleness).width()),
            u16::MAX,
        ));
    }
    widths
}

/// Columns are separated by this many cells.
const CAPACITY_COLUMN_SPACING: u16 = 2;

/// Width needed to draw the complete Targets table, including its table
/// spacing, border, and always-present selection marker.
pub(crate) fn capacity_table_width(dashboard: &DashboardState) -> u16 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let widths = capacity_column_widths(&capacity_table_rows(dashboard, now));
    let gaps = u16::try_from(widths.len().saturating_sub(1)).unwrap_or(u16::MAX);
    widths
        .into_iter()
        .fold(0_u16, u16::saturating_add)
        .saturating_add(gaps.saturating_mul(CAPACITY_COLUMN_SPACING))
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
    let row_heights = vec![1; rows.len()];
    let disks_buttons = rows
        .iter()
        .map(CapacityTableRow::disks_button_width)
        .collect::<Vec<_>>();
    let mut header = CAPACITY_HEADERS.to_vec();
    if column_widths.len() > header.len() {
        header.push("");
    }
    let stale_column = column_widths.len() > CAPACITY_HEADERS.len();
    let table = Table::new(
        rows.into_iter().map(|row| {
            let mut cells = vec![
                Cell::from(row.host).style(Style::default().add_modifier(Modifier::BOLD)),
                Cell::from(row.targets).style(theme::muted()),
                Cell::from(row.cpu),
                Cell::from(row.memory),
                Cell::from(row.disks),
            ];
            if stale_column {
                cells.push(Cell::from(row.staleness.unwrap_or_default()).style(theme::muted()));
            }
            Row::new(cells).height(1)
        }),
        column_widths.iter().copied().map(Constraint::Length),
    )
    .column_spacing(CAPACITY_COLUMN_SPACING)
    .header(Row::new(header).style(theme::muted().patch(theme::raised())))
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
    let viewport = usize::from(area.height.saturating_sub(SESSION_TABLE_CHROME_HEIGHT));
    let mut offset = mj_chat::components::clamp_offset_to_last_page(
        dashboard.targets_scroll.get(),
        &row_heights,
        viewport,
    );
    if take_selection_recenter(dashboard, Focus::Targets) {
        offset =
            mj_chat::components::centered_offset(dashboard.capacity_index, &row_heights, viewport);
    }
    let mut state = TableState::default().with_offset(offset).with_selected(
        (!dashboard.capacity_details.is_empty()).then_some(dashboard.capacity_index),
    );
    frame.render_stateful_widget(table, area, &mut state);
    register_disks_buttons(
        dashboard,
        area,
        &column_widths,
        row_heights.len(),
        &disks_buttons,
        state.offset(),
    );
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

/// Registers each visible row's Disks summary as the button that opens its
/// filesystem dropdown. The cell positions repeat the table's own layout:
/// a border and the two-cell selection marker, then each column and its gap,
/// and below the border, the header and the rows from the scroll offset.
fn register_disks_buttons(
    dashboard: &DashboardState,
    area: Rect,
    column_widths: &[u16],
    row_count: usize,
    buttons: &[u16],
    offset: usize,
) {
    let inner_right = area.right().saturating_sub(1);
    let inner_bottom = area.bottom().saturating_sub(1);
    let x = column_widths.iter().take(CAPACITY_HEADERS.len() - 1).fold(
        area.x.saturating_add(3),
        |x, width| {
            x.saturating_add(*width)
                .saturating_add(CAPACITY_COLUMN_SPACING)
        },
    );
    let mut y = area.y.saturating_add(2);
    let mut areas = vec![None; row_count];
    let mut form = dashboard.surface_form.borrow_mut();
    for (index, width) in buttons.iter().enumerate().skip(offset) {
        if y >= inner_bottom || x >= inner_right {
            break;
        }
        let width = (*width).min(inner_right - x);
        if width > 0 {
            let button_area = Rect::new(x, y, width, 1);
            form.register(
                crate::surface_controls::SurfaceControl::CapacityDisks(index),
                ControlKind::Button,
                button_area,
                true,
            );
            areas[index] = Some(button_area);
        }
        y = y.saturating_add(1);
    }
    *dashboard.capacity_disks_areas.borrow_mut() = areas;
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
