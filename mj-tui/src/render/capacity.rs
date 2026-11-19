use super::*;

use mj_chat::components::ControlKind;
use ratatui::layout::{Flex, Layout};

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

/// Why the row's reading cannot be trusted, if it cannot.
pub(crate) fn capacity_staleness(
    detail: &CapacityDetail,
    now_epoch_seconds: u64,
) -> Option<String> {
    mj_core::targets::capacity_staleness(
        detail.state.usage.is_some(),
        detail.state.probe_error.as_deref(),
        detail.state.sampled_at_epoch_seconds,
        now_epoch_seconds,
    )
}

pub(crate) struct CapacityTableRow {
    host: String,
    targets: Line<'static>,
    cpu: Line<'static>,
    memory: Line<'static>,
    /// The one-line Disks button, which opens filesystem details in a popup.
    disks: Line<'static>,
    /// Why the row's reading cannot be trusted, when it cannot. It is drawn
    /// on a second line of the row rather than in a column of its own, so a
    /// notice never changes how wide the table has to be.
    staleness: Option<String>,
}

impl CapacityTableRow {
    /// Width of the Disks button. Zero when the row has no storage reading.
    fn disks_button_width(&self) -> u16 {
        u16::try_from(self.disks.width()).unwrap_or(u16::MAX)
    }

    /// The lines this target wants: its readings, plus one for the notice
    /// when the reading cannot be trusted. The pane's content height bounds
    /// it, so a short pane keeps the readings and waits for room for the
    /// notice.
    fn height(&self) -> usize {
        1 + usize::from(self.staleness.is_some())
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
            // Every listed target is unavailable: say so once for the host
            // and draw nothing else (no capacity, no Disks button).
            if all_listed_targets_unavailable(&detail.target.target_ids, dashboard) {
                return CapacityTableRow {
                    host: detail.target.host.clone(),
                    targets: Line::styled("unavailable", theme::muted()),
                    cpu: Line::default(),
                    memory: Line::default(),
                    disks: Line::default(),
                    staleness: None,
                };
            }
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

/// Whether the row lists targets and every one of them is unavailable. Targets
/// that are not listed (a never-installed standard runtime) do not count.
fn all_listed_targets_unavailable(target_ids: &[String], dashboard: &DashboardState) -> bool {
    let mut listed = target_ids
        .iter()
        .filter(|id| dashboard.target_missing(id) != Some(crate::availability::Missing::Silent))
        .peekable();
    listed.peek().is_some() && listed.all(|id| dashboard.target_missing(id).is_some())
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
    match (&detail.target.kind, &detail.state.usage) {
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
        (DeploymentCapacityKind::AwsFleet, None) if detail.state.on_demand => {
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

use mj_core::targets::storage_condition_flag;

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

/// The lines the Targets table draws: one line per target, plus one line
/// under each target whose reading cannot be trusted and says why.
pub(crate) fn capacity_table_lines(dashboard: &DashboardState) -> usize {
    let now_epoch_seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    capacity_table_rows(dashboard, now_epoch_seconds)
        .iter()
        .map(CapacityTableRow::height)
        .sum()
}

/// The Disks column has no heading: its cell already says `Disks ▾`, or
/// names the filesystem it is about.
const CAPACITY_HEADERS: [&str; 5] = ["Host / fleet", "Targets", "CPU", "RAM", ""];

/// Column widths of the Targets table, one per heading. A notice line is
/// deliberately absent from this list: it spans the columns under Targets
/// instead, so a stale reading cannot make the pane ask for more width.
pub(crate) fn capacity_column_widths(rows: &[CapacityTableRow]) -> Vec<u16> {
    vec![
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
    ]
}

/// Columns are separated by this many cells.
const CAPACITY_COLUMN_SPACING: u16 = 2;

/// Width needed to draw the complete Targets table, including its table
/// spacing, border, and always-present selection marker. A notice line needs
/// no width of its own: it shares the row's second line.
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
    // A row never asks for more lines than the pane has room for. The table
    // scrolls a selected row into view by dropping whole rows from the top of
    // the page, and a row taller than the content area makes it drop the
    // selected row itself, so a notice that does not fit waits for room
    // instead of costing the readings their line.
    let viewport = usize::from(area.height.saturating_sub(SESSION_TABLE_CHROME_HEIGHT));
    let row_heights = rows
        .iter()
        .map(|row| row.height().min(viewport.max(1)))
        .collect::<Vec<_>>();
    let disks_buttons = rows
        .iter()
        .map(CapacityTableRow::disks_button_width)
        .collect::<Vec<_>>();
    let notices = rows
        .iter()
        .zip(&row_heights)
        .map(|(row, height)| {
            (*height > 1)
                .then(|| row.staleness.clone())
                .flatten()
                .unwrap_or_default()
        })
        .collect::<Vec<_>>();
    let table = Table::new(
        rows.into_iter().zip(&row_heights).map(|(row, height)| {
            Row::new(vec![
                Cell::from(row.host).style(Style::default().add_modifier(Modifier::BOLD)),
                Cell::from(row.targets).style(theme::muted()),
                Cell::from(row.cpu),
                Cell::from(row.memory),
                Cell::from(row.disks),
            ])
            .height(u16::try_from(*height).unwrap_or(u16::MAX))
        }),
        column_widths.iter().copied().map(Constraint::Length),
    )
    .column_spacing(CAPACITY_COLUMN_SPACING)
    .header(Row::new(CAPACITY_HEADERS.to_vec()).style(theme::muted().patch(theme::raised())))
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
    // The notice lines are drawn after the table, which laid out the rows
    // they belong to.
    let columns = capacity_columns(area, &column_widths);
    register_disks_buttons(
        dashboard,
        &columns,
        &disks_buttons,
        &row_heights,
        state.offset(),
    );
    render_capacity_notices(frame, &columns, &notices, &row_heights, state.offset());
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
        row_heights.iter().sum(),
        row_heights.iter().take(state.offset()).sum(),
        usize::from(area.height.saturating_sub(SESSION_TABLE_CHROME_HEIGHT)),
    );
}

/// The inner area of the Targets table and the columns inside it, laid out
/// the way the table lays them out, so hit regions and notice lines follow
/// the rendered text even when columns shrink to fit the pane.
struct CapacityColumns {
    inner: Rect,
    columns: Vec<Rect>,
}

impl CapacityColumns {
    /// The Targets column, under which every notice line starts.
    fn targets(&self) -> Rect {
        self.columns[1]
    }

    fn disks(&self) -> Rect {
        self.columns[CAPACITY_HEADERS.len() - 1]
    }
}

/// Width of the selection marker a table row reserves, always present. It is
/// the width of `theme::glyphs().selected`, the glyph the table highlights a
/// row with.
const CAPACITY_SELECTION_WIDTH: u16 = 2;

fn capacity_columns(area: Rect, column_widths: &[u16]) -> CapacityColumns {
    let inner = area.inner(Margin::new(1, 1));
    let [_, columns_area] = Layout::horizontal([
        Constraint::Length(CAPACITY_SELECTION_WIDTH),
        Constraint::Fill(0),
    ])
    .areas(inner);
    let columns = Layout::horizontal(column_widths.iter().copied().map(Constraint::Length))
        .flex(Flex::Start)
        .spacing(CAPACITY_COLUMN_SPACING)
        .split(columns_area)
        .to_vec();
    CapacityColumns { inner, columns }
}

/// Draws each row's notice across the columns under Targets, on the second
/// line the row reserves for it. The line runs to the pane's inner edge and
/// is cut with an ellipsis, so a long notice costs width the table already
/// has instead of widening the pane.
fn render_capacity_notices(
    frame: &mut Frame,
    columns: &CapacityColumns,
    notices: &[String],
    row_heights: &[usize],
    offset: usize,
) {
    let targets = columns.targets();
    let width = columns.inner.right().saturating_sub(targets.x);
    if width == 0 {
        return;
    }
    // The first row sits under the border and the heading row.
    let mut y = columns.inner.y.saturating_add(1);
    for (notice, height) in notices.iter().zip(row_heights).skip(offset) {
        if y >= columns.inner.bottom() {
            break;
        }
        let notice_y = y.saturating_add(1);
        if !notice.is_empty() && notice_y < columns.inner.bottom() {
            let text = truncate_to_cells(notice, usize::from(width), Truncate::SUMMARY);
            frame.render_widget(
                Paragraph::new(Line::styled(text, theme::muted())),
                Rect::new(targets.x, notice_y, width, 1),
            );
        }
        y = y.saturating_add(u16::try_from(*height).unwrap_or(u16::MAX));
    }
}

/// Registers each visible row's Disks summary as the button that opens its
/// filesystem dropdown.
fn register_disks_buttons(
    dashboard: &DashboardState,
    columns: &CapacityColumns,
    buttons: &[u16],
    row_heights: &[usize],
    offset: usize,
) {
    let disks = columns.disks();
    let mut y = columns.inner.y.saturating_add(1);
    let mut areas = vec![None; buttons.len()];
    let mut form = dashboard.surface_form.borrow_mut();
    for ((index, width), height) in buttons
        .iter()
        .enumerate()
        .zip(row_heights.iter())
        .skip(offset)
    {
        if y >= columns.inner.bottom() {
            break;
        }
        let width = (*width).min(disks.width);
        if width > 0 {
            let button_area = Rect::new(disks.x, y, width, 1);
            form.register(
                crate::surface_controls::SurfaceControl::CapacityDisks(index),
                ControlKind::Button,
                button_area,
                true,
            );
            areas[index] = Some(button_area);
        }
        y = y.saturating_add(u16::try_from(*height).unwrap_or(u16::MAX));
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
