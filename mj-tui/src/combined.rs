//! The combined conversation surface.
//!
//! One screen holds all of Hel's terminal UI: a Sessions pane, the transcript
//! of the conversation on screen, the Prompt composer, and Targets and Quota
//! summaries under it, with a shared one-row footer. There is no second screen
//! to switch to, so nothing is ever hidden behind a navigation step.

use mj_chat::hel_chat::{ActiveChat, ChatRegions};
use mj_chat::hel_selection::{SurfaceFrame, SurfaceId};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};

use crate::render::{
    MINIMUM_TERMINAL_WIDTH, TerminalSizeRequirement, minimized_pane_size_controls,
    minimized_quota_line, minimized_targets_line, pane_size_control_areas,
    pane_title_content_width, render_capacity, render_footer, render_modal,
    render_onboarding_surface, render_quotas, render_sessions, render_terminal_too_small,
    sessions_content_height,
};
use crate::resume::resume_sessions_pane;
use crate::widgets::bordered_content;
use crate::{DashboardState, Focus, Mode, PaneSize, SupportPane};

/// Rows the footer always keeps.
const FOOTER_HEIGHT: u16 = 1;
/// The fewest rows the transcript is worth drawing in.
const TRANSCRIPT_MINIMUM: u16 = 3;
/// A bordered composer with one row of text.
const PROMPT_MINIMUM: u16 = 3;
/// A border plus the two lines of create-or-resume guidance.
const EMPTY_PROMPT_HEIGHT: u16 = 4;
/// A bordered pane with one row of content.
const PANE_MINIMUM: u16 = 3;
/// The one-row form Targets and Quota collapse to.
const SUMMARY_ROW: u16 = 1;

/// The terminal height at or above which the minimized Sessions grid gets its
/// taller five-row cap; below it the grid is capped at two rows.
const TALL_TERMINAL_HEIGHT: u16 = 40;

/// The number of columns in the minimized Sessions grid.
pub(crate) const MINIMIZED_GRID_COLUMNS: usize = 3;

/// How many content rows the minimized Sessions grid needs for its cells.
/// Sparse grids use only the rows they need; short and tall terminals cap
/// that count at the existing two- and five-row limits respectively.
pub(crate) fn minimized_grid_rows(frame_height: u16, cell_count: usize) -> u16 {
    let required = cell_count
        .div_ceil(MINIMIZED_GRID_COLUMNS)
        .max(1)
        .try_into()
        .unwrap_or(u16::MAX);
    let cap = if frame_height >= TALL_TERMINAL_HEIGHT {
        5
    } else {
        2
    };
    required.min(cap)
}

/// The height the composer settles at: its desired height, but never below
/// [`PROMPT_MINIMUM`] and never above a third of the frame.
fn prompt_target(desired_prompt: u16, frame_height: u16) -> u16 {
    let ceiling = (frame_height / 3).max(PROMPT_MINIMUM);
    desired_prompt.clamp(PROMPT_MINIMUM, ceiling)
}

/// How tall one band wants to be and how short it may get.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PaneBand {
    minimum: u16,
    full: u16,
    cap: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PaneDimensions {
    minimized: u16,
    full: u16,
    standard_cap: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CombinedHeights {
    sessions: u16,
    transcript: u16,
    prompt: u16,
    targets: u16,
    quota: u16,
    footer: u16,
}

impl CombinedHeights {
    fn pane(self, pane: SupportPane) -> u16 {
        match pane {
            SupportPane::Sessions => self.sessions,
            SupportPane::Targets => self.targets,
            SupportPane::Quota => self.quota,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CombinedAllocation {
    Fits(CombinedHeights),
    TooSmall { required_frame_height: u16 },
}

/// Divides the frame between the six bands.
///
/// Every band starts at its minimum, and the surplus is then spent in a fixed
/// order: the composer first, because it is where the user is typing; then the
/// one maximized pane; then Standard Sessions, Targets, and Quota in screen
/// order. The transcript takes whatever is left. Focus never participates.
fn allocate_combined_heights(
    frame_height: u16,
    sessions: PaneBand,
    targets: PaneBand,
    quota: PaneBand,
    desired_prompt: u16,
    sizes: [(SupportPane, PaneSize); 3],
) -> CombinedAllocation {
    let required = sessions
        .minimum
        .saturating_add(TRANSCRIPT_MINIMUM)
        .saturating_add(PROMPT_MINIMUM)
        .saturating_add(targets.minimum)
        .saturating_add(quota.minimum)
        .saturating_add(FOOTER_HEIGHT);
    if frame_height < required {
        return CombinedAllocation::TooSmall {
            required_frame_height: required,
        };
    }
    let mut heights = CombinedHeights {
        sessions: sessions.minimum,
        transcript: TRANSCRIPT_MINIMUM,
        prompt: PROMPT_MINIMUM,
        targets: targets.minimum,
        quota: quota.minimum,
        footer: FOOTER_HEIGHT,
    };
    let mut surplus = frame_height.saturating_sub(required);
    let grow = |current: &mut u16, want: u16, surplus: &mut u16| {
        let step = (*surplus).min(want.saturating_sub(*current));
        *current = current.saturating_add(step);
        *surplus = surplus.saturating_sub(step);
    };
    grow(
        &mut heights.prompt,
        prompt_target(desired_prompt, frame_height),
        &mut surplus,
    );
    {
        let mut grow_pane = |pane: SupportPane, surplus: &mut u16| match pane {
            SupportPane::Sessions => grow(
                &mut heights.sessions,
                sessions.full.min(sessions.cap),
                surplus,
            ),
            SupportPane::Targets => {
                grow(&mut heights.targets, targets.full.min(targets.cap), surplus)
            }
            SupportPane::Quota => grow(&mut heights.quota, quota.full.min(quota.cap), surplus),
        };
        if let Some((pane, _)) = sizes.iter().find(|(_, size)| *size == PaneSize::Maximized) {
            grow_pane(*pane, &mut surplus);
        }
        for (pane, size) in sizes {
            if size == PaneSize::Standard {
                grow_pane(pane, &mut surplus);
            }
        }
    }
    heights.transcript = heights.transcript.saturating_add(surplus);
    CombinedAllocation::Fits(heights)
}

/// Whether making each pane Maximized would give it more rows than Standard
/// at this frame size. Maximized is exclusive, so the hypothetical maximum
/// demotes any other maximum before running the same allocator used to draw
/// the frame.
fn maximized_pane_is_effective(
    frame_height: u16,
    dimensions: [(SupportPane, PaneDimensions); 3],
    desired_prompt: u16,
    sizes: [(SupportPane, PaneSize); 3],
) -> [(SupportPane, bool); 3] {
    let bands = dimensions;
    std::array::from_fn(|index| {
        let (pane, _) = bands[index];
        let standard_sizes = sizes_with_pane_size(sizes, pane, PaneSize::Standard);
        let maximum_sizes = sizes_with_pane_size(sizes, pane, PaneSize::Maximized);
        let bands_for = |sizes: [(SupportPane, PaneSize); 3]| {
            std::array::from_fn(|index| {
                let (_, size) = sizes[index];
                let dimensions = bands[index].1;
                sized_band(
                    size,
                    dimensions.minimized,
                    dimensions.full,
                    dimensions.standard_cap,
                )
            })
        };
        let standard_bands: [PaneBand; 3] = bands_for(standard_sizes);
        let maximum_bands: [PaneBand; 3] = bands_for(maximum_sizes);
        let effective = match (
            allocate_combined_heights(
                frame_height,
                standard_bands[0],
                standard_bands[1],
                standard_bands[2],
                desired_prompt,
                standard_sizes,
            ),
            allocate_combined_heights(
                frame_height,
                maximum_bands[0],
                maximum_bands[1],
                maximum_bands[2],
                desired_prompt,
                maximum_sizes,
            ),
        ) {
            (CombinedAllocation::Fits(standard), CombinedAllocation::Fits(maximum)) => {
                maximum.pane(pane) > standard.pane(pane)
            }
            // Both allocations use the same minimum requirements, so this is
            // only defensive if that invariant changes later.
            _ => false,
        };
        (pane, effective)
    })
}

fn sizes_with_pane_size(
    sizes: [(SupportPane, PaneSize); 3],
    pane: SupportPane,
    size: PaneSize,
) -> [(SupportPane, PaneSize); 3] {
    let mut adjusted = sizes;
    if size == PaneSize::Maximized {
        for (candidate, candidate_size) in &mut adjusted {
            if *candidate != pane && *candidate_size == PaneSize::Maximized {
                *candidate_size = PaneSize::Standard;
            }
        }
    }
    for (candidate, candidate_size) in &mut adjusted {
        if *candidate == pane {
            *candidate_size = size;
        }
    }
    adjusted
}

fn sized_band(size: PaneSize, minimized_height: u16, full: u16, standard_cap: u16) -> PaneBand {
    match size {
        PaneSize::Minimized => PaneBand {
            minimum: minimized_height,
            full: minimized_height,
            cap: minimized_height,
        },
        PaneSize::Standard => PaneBand {
            minimum: PANE_MINIMUM,
            full,
            cap: standard_cap.max(PANE_MINIMUM),
        },
        PaneSize::Maximized => PaneBand {
            minimum: PANE_MINIMUM,
            full,
            cap: full.max(PANE_MINIMUM),
        },
    }
}

/// Draws the whole combined surface: Sessions, the conversation, Prompt,
/// Targets, Quota, the footer, and any modal over the top.
///
/// `chat` is the conversation on screen, or `None` when the workspace has no
/// live session. `transcript_selected` says the selection engine still owns a
/// selection on the transcript, so its row space has to stay frozen for this
/// frame.
pub fn render_combined(
    frame: &mut Frame,
    dashboard: &mut DashboardState,
    mut chat: Option<&mut ActiveChat>,
    transcript_selected: bool,
) {
    dashboard.reset_component_geometry();
    if let Some(chat) = chat.as_deref_mut() {
        chat.reset_component_geometry();
    }
    dashboard.pane_areas = None;
    dashboard.session_row_areas.clear();
    dashboard.project_heading_areas.clear();
    dashboard.pane_size_control_areas.clear();
    dashboard.frame_surfaces.clear();
    dashboard.chat_transcript_area = None;
    dashboard.chat_prompt_area = None;
    let area = frame.area();
    if area.width < MINIMUM_TERMINAL_WIDTH {
        render_terminal_too_small(
            frame,
            area,
            TerminalSizeRequirement::Width(MINIMUM_TERMINAL_WIDTH),
        );
        return;
    }
    dashboard.resume_sessions_area =
        matches!(dashboard.mode, Mode::ResumeDialog(_)).then(|| resume_sessions_pane(area));
    if dashboard.config_is_empty() {
        render_onboarding_surface(frame, dashboard);
        return;
    }

    // With no conversation the prompt band holds the two-line guidance that
    // stands in for a composer, so it asks for the rows to show both.
    let desired_prompt = chat.as_ref().map_or(EMPTY_PROMPT_HEIGHT, |chat| {
        chat.desired_prompt_height(area.width)
    });
    let sizes = [
        (
            SupportPane::Sessions,
            dashboard.pane_size(SupportPane::Sessions),
        ),
        (
            SupportPane::Targets,
            dashboard.pane_size(SupportPane::Targets),
        ),
        (SupportPane::Quota, dashboard.pane_size(SupportPane::Quota)),
    ];
    let dimensions = [
        (
            SupportPane::Sessions,
            PaneDimensions {
                minimized: minimized_grid_rows(area.height, dashboard.sessions_rows().len())
                    .saturating_add(2),
                full: sessions_content_height(dashboard, area.width).saturating_add(2),
                standard_cap: area.height / 3,
            },
        ),
        (
            SupportPane::Targets,
            PaneDimensions {
                minimized: SUMMARY_ROW,
                full: table_height(dashboard.capacity_details.len()),
                standard_cap: area.height / 4,
            },
        ),
        (
            SupportPane::Quota,
            PaneDimensions {
                minimized: SUMMARY_ROW,
                full: table_height(dashboard.config.profiles.len()),
                standard_cap: area.height / 4,
            },
        ),
    ];
    let bands: [PaneBand; 3] = std::array::from_fn(|index| {
        let (_, dimensions) = dimensions[index];
        sized_band(
            sizes[index].1,
            dimensions.minimized,
            dimensions.full,
            dimensions.standard_cap,
        )
    });
    let sessions = bands[0];
    let targets = bands[1];
    let quota = bands[2];
    dashboard.set_pane_maximize_enabled(maximized_pane_is_effective(
        area.height,
        dimensions,
        desired_prompt,
        sizes,
    ));
    let allocation =
        allocate_combined_heights(area.height, sessions, targets, quota, desired_prompt, sizes);
    let heights = match allocation {
        CombinedAllocation::Fits(heights) => heights,
        CombinedAllocation::TooSmall {
            required_frame_height,
        } => {
            render_terminal_too_small(
                frame,
                area,
                TerminalSizeRequirement::Height(required_frame_height),
            );
            return;
        }
    };

    let bands = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(heights.sessions),
            Constraint::Length(heights.transcript),
            Constraint::Length(heights.prompt),
            Constraint::Length(heights.targets),
            Constraint::Length(heights.quota),
            Constraint::Length(heights.footer),
        ])
        .split(area);
    let (sessions_area, transcript_area, prompt_area, targets_area, quota_area, footer_area) =
        (bands[0], bands[1], bands[2], bands[3], bands[4], bands[5]);
    dashboard.pane_areas = Some([sessions_area, targets_area, quota_area]);
    for (pane, pane_area) in [
        (SupportPane::Sessions, sessions_area),
        (SupportPane::Targets, targets_area),
        (SupportPane::Quota, quota_area),
    ] {
        dashboard
            .pane_size_control_areas
            .extend(pane_size_control_areas(pane_area, pane));
    }

    let rendered = render_sessions(frame, sessions_area, dashboard);
    dashboard.session_row_areas = rendered.session_row_areas;
    dashboard.project_heading_areas = rendered.project_heading_areas;
    let sessions_content = bordered_content(sessions_area);
    dashboard.frame_surfaces.push(SurfaceFrame::fixed(
        SurfaceId::DashboardPane(0),
        sessions_content,
    ));

    dashboard.chat_transcript_area = Some(transcript_area);
    dashboard.chat_prompt_area = Some(prompt_area);
    let prompt_focused = dashboard.prompt_has_focus();
    let chat_drew_footer = match chat {
        Some(chat) => {
            chat.draw_in(
                frame,
                ChatRegions {
                    transcript: transcript_area,
                    prompt: prompt_area,
                    footer: prompt_focused.then_some(footer_area),
                    overlay: area,
                },
                prompt_focused,
                transcript_selected,
            );
            // A chat-local modal may own the frame's interaction. Questions
            // deliberately leave this flag clear so the navigator and other
            // dashboard panes remain selectable beside the question area.
            if chat.frame_surfaces_exclusive() {
                dashboard.frame_surfaces.replace_with(chat.frame_surfaces());
            } else {
                dashboard.frame_surfaces.append(chat.frame_surfaces());
            }
            prompt_focused
        }
        None => {
            let reason = if dashboard.opening_session().is_some() {
                EmptyConversation::Opening
            } else if dashboard.ordered_sessions().is_empty() {
                EmptyConversation::NoLiveSession
            } else {
                EmptyConversation::NoConversationOpen
            };
            render_empty_conversation(frame, transcript_area, prompt_area, prompt_focused, reason);
            false
        }
    };

    // Targets and Quota choose their representations independently.
    if sizes[1].1 == PaneSize::Minimized {
        let focused = dashboard.focus() == Focus::Targets;
        frame.render_widget(
            Block::default()
                .borders(Borders::TOP)
                .border_type(if focused {
                    BorderType::Double
                } else {
                    BorderType::Plain
                })
                .title(minimized_targets_line(
                    dashboard,
                    pane_title_content_width(targets_area.width),
                    focused,
                ))
                .title(minimized_pane_size_controls(
                    focused,
                    dashboard.pane_maximize_enabled(SupportPane::Targets),
                )),
            targets_area,
        );
    } else {
        render_capacity(frame, targets_area, dashboard, Some(sizes[1].1));
    }
    if sizes[2].1 == PaneSize::Minimized {
        let focused = dashboard.focus() == Focus::Quota;
        frame.render_widget(
            Block::default()
                .borders(Borders::TOP)
                .border_type(if focused {
                    BorderType::Double
                } else {
                    BorderType::Plain
                })
                .title(minimized_quota_line(
                    dashboard,
                    pane_title_content_width(quota_area.width),
                    focused,
                ))
                .title(minimized_pane_size_controls(
                    focused,
                    dashboard.pane_maximize_enabled(SupportPane::Quota),
                )),
            quota_area,
        );
    } else {
        render_quotas(frame, quota_area, dashboard, Some(sizes[2].1));
    }
    let targets_content = if sizes[1].1 == PaneSize::Minimized {
        targets_area
    } else {
        bordered_content(targets_area)
    };
    let quota_content = if sizes[2].1 == PaneSize::Minimized {
        quota_area
    } else {
        bordered_content(quota_area)
    };
    dashboard.frame_surfaces.push(SurfaceFrame::fixed(
        SurfaceId::DashboardPane(1),
        targets_content,
    ));
    dashboard.frame_surfaces.push(SurfaceFrame::fixed(
        SurfaceId::DashboardPane(2),
        quota_content,
    ));

    if !chat_drew_footer {
        render_footer(frame, footer_area, dashboard);
    }
    render_modal(frame, area, dashboard);
}

/// Why the conversation band is empty, which is what decides the advice it
/// gives.
enum EmptyConversation {
    /// The workspace has no live session at all.
    NoLiveSession,
    /// The workspace has live sessions and none of them is open.
    NoConversationOpen,
    /// An attach is in flight, so a conversation is on its way.
    Opening,
}

/// The bordered chrome that stands in for a conversation when none is on
/// screen, and the thing the user can do about it.
///
/// The three reasons for an empty band need different advice: a workspace
/// with no live session needs one created or resumed, a workspace that has
/// live sessions just needs one opened, and an attach that is still running
/// needs nothing but a moment. Telling the second user there is no live
/// session would be a plain lie — the pane above is listing them.
fn render_empty_conversation(
    frame: &mut Frame,
    transcript_area: Rect,
    prompt_area: Rect,
    prompt_focused: bool,
    reason: EmptyConversation,
) {
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .title(" Conversation "),
        transcript_area,
    );
    let border = if prompt_focused {
        BorderType::Double
    } else {
        BorderType::Plain
    };
    let (title, lines) = match reason {
        EmptyConversation::NoLiveSession => (
            " Prompt (no live session) ",
            [
                "No live session in this workspace.",
                "Press Alt-N to create one, or Alt-S to resume one.",
            ],
        ),
        EmptyConversation::NoConversationOpen => (
            " Prompt (no conversation open) ",
            [
                "No conversation open.",
                "Press Tab for Sessions, then Enter on the one to open.",
            ],
        ),
        EmptyConversation::Opening => (
            " Prompt (opening session) ",
            [
                "Opening session…",
                "The conversation appears when it attaches.",
            ],
        ),
    };
    frame.render_widget(
        Paragraph::new(lines.map(Line::raw).to_vec())
            .style(Style::default().fg(Color::DarkGray))
            .wrap(Wrap { trim: true })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(border)
                    .title(title),
            ),
        prompt_area,
    );
}

/// A bordered table with a header row and `rows` data rows.
fn table_height(rows: usize) -> u16 {
    u16::try_from(rows)
        .unwrap_or(u16::MAX)
        .saturating_add(PANE_MINIMUM)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimized_grid_uses_fewest_rows_for_sparse_cell_counts() {
        assert_eq!(minimized_grid_rows(40, 0), 1);
        assert_eq!(minimized_grid_rows(40, 1), 1);
        assert_eq!(minimized_grid_rows(40, 3), 1);
        assert_eq!(minimized_grid_rows(40, 4), 2);
    }

    #[test]
    fn minimized_grid_caps_rows_at_two_short_and_five_tall() {
        assert_eq!(minimized_grid_rows(39, 6), 2);
        assert_eq!(minimized_grid_rows(39, 100), 2);
        assert_eq!(minimized_grid_rows(40, 15), 5);
        assert_eq!(minimized_grid_rows(100, 100), 5);
    }

    #[test]
    fn prompt_target_clamps_between_the_minimum_and_a_third() {
        // Below the minimum is raised to it; within range is kept; above a
        // third of the frame is capped there.
        assert_eq!(prompt_target(2, 60), PROMPT_MINIMUM);
        assert_eq!(prompt_target(5, 60), 5);
        assert_eq!(prompt_target(50, 60), 20);
    }

    #[test]
    fn sized_bands_encode_each_states_minimum_and_cap() {
        assert_eq!(
            sized_band(PaneSize::Minimized, 7, 40, 20),
            PaneBand {
                minimum: 7,
                full: 7,
                cap: 7,
            }
        );
        assert_eq!(
            sized_band(PaneSize::Standard, 1, 40, 20),
            PaneBand {
                minimum: 3,
                full: 40,
                cap: 20,
            }
        );
        assert_eq!(
            sized_band(PaneSize::Maximized, 1, 40, 20),
            PaneBand {
                minimum: 3,
                full: 40,
                cap: 40,
            }
        );
    }

    #[test]
    fn prompt_then_maximum_then_standard_panes_receive_surplus() {
        let band = |full, cap| PaneBand {
            minimum: 3,
            full,
            cap,
        };
        let sizes = [
            (SupportPane::Sessions, PaneSize::Standard),
            (SupportPane::Targets, PaneSize::Maximized),
            (SupportPane::Quota, PaneSize::Standard),
        ];
        let CombinedAllocation::Fits(heights) =
            allocate_combined_heights(40, band(12, 10), band(15, 15), band(8, 8), 6, sizes)
        else {
            panic!("40 rows should fit");
        };
        assert_eq!(heights.prompt, 6);
        assert_eq!(heights.targets, 15);
        assert_eq!(heights.sessions, 10);
        assert_eq!(heights.quota, 5);
        assert_eq!(heights.transcript, 3);
    }

    #[test]
    fn allocation_reports_the_dynamic_state_minimum() {
        let fixed = |height| PaneBand {
            minimum: height,
            full: height,
            cap: height,
        };
        let result = allocate_combined_heights(
            12,
            fixed(4),
            fixed(1),
            fixed(1),
            3,
            [
                (SupportPane::Sessions, PaneSize::Minimized),
                (SupportPane::Targets, PaneSize::Minimized),
                (SupportPane::Quota, PaneSize::Minimized),
            ],
        );
        assert_eq!(
            result,
            CombinedAllocation::TooSmall {
                required_frame_height: 13,
            }
        );
    }
}
