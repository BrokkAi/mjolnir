//! The combined conversation surface.
//!
//! One screen holds all of Hel's terminal UI: a Sessions pane, the transcript
//! of the conversation on screen, the Prompt composer, and Targets and Quota
//! summaries under it, with a shared one-row footer. There is no second screen
//! to switch to, so nothing is ever hidden behind a navigation step.

use std::collections::BTreeMap;

use mj_chat::chat::{ActiveChat, ChatFooter, ChatRegions, ChatState};
use mj_chat::selection::{FrameSurfaces, SurfaceFrame, SurfaceId};
use mj_chat::{spinner, theme};
use mj_core::state::SessionTransitionKind;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::render::{
    MINIMUM_TERMINAL_WIDTH, NARROW_TERMINAL_WIDTH, SESSION_ACTIONS_HEIGHT, TerminalSizeRequirement,
    capacity_table_width, minimized_pane_size_controls, minimized_quota_line,
    minimized_sessions_content_height, minimized_targets_line, pane_size_control_areas,
    pane_title_content_width, quota_table_width, render_capacity, render_footer, render_modal,
    render_onboarding_surface, render_quotas, render_sessions, render_terminal_too_small,
    sessions_content_height,
};
use crate::resume::resume_sessions_pane;
use crate::tile_layout::PaneId;
use crate::widgets::bordered_content;
use crate::workspaces::render_workspace_tabs;
use crate::{DashboardState, Focus, Mode, PaneSize, SupportPane};

/// Rows the footer always keeps.
const FOOTER_HEIGHT: u16 = 1;
/// The bordered workspace list above Sessions. Its inner row is intentionally
/// short; the border gives it a stable click and focus target.
const WORKSPACE_PANE_HEIGHT: u16 = 3;
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

/// The terminal height at or above which the minimized Sessions list gets its
/// taller five-row cap; below it the list is capped at two rows. Compact
/// summaries occupy two lines, so the returned value is a content height.
const TALL_TERMINAL_HEIGHT: u16 = 40;

/// Width of the Sessions sidebar for an explicit pane size. The final half
/// width bound keeps the conversation visible on narrow terminals even when a
/// clamped size would otherwise be wider than the available screen.
fn sessions_sidebar_width(width: u16, size: PaneSize) -> u16 {
    match size {
        PaneSize::Minimized => 20,
        PaneSize::Standard => (width / 3).clamp(40, 80),
        PaneSize::Maximized => (width / 2).clamp(40, 100),
    }
    .min(width / 2)
}

/// How many content lines the minimized Sessions list can show. Short and
/// tall terminals retain the existing two- and five-entry caps respectively;
/// each compact entry now occupies two lines.
pub(crate) fn minimized_session_rows(frame_height: u16, content_height: usize) -> u16 {
    let required = content_height.max(1).try_into().unwrap_or(u16::MAX);
    let cap = if frame_height >= TALL_TERMINAL_HEIGHT {
        10
    } else {
        4
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
                if index == 0 {
                    return PaneBand {
                        minimum: 0,
                        full: 0,
                        cap: 0,
                    };
                }
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

fn support_panes_fit(content_width: u16, dashboard: &DashboardState) -> bool {
    capacity_table_width(dashboard).max(quota_table_width(dashboard)) <= content_width
}

/// Draws the whole combined surface: Sessions, the conversation, Prompt,
/// Targets, Quota, the footer, and any modal over the top.
///
/// `chats` holds every warm conversation, keyed by session id; the panes of
/// the conversation layout pick the ones on screen out of it.
/// `opening_panes` says which pane is waiting for which session's attach, so
/// a pane draws nothing for a session it has not finished opening.
/// `transcript_selected` says the selection engine still owns a selection on
/// the transcript, so its row space has to stay frozen for this frame.
///
/// Reports the sessions whose conversations this frame actually drew, which
/// is what the read receipts follow.
pub fn render_combined(
    frame: &mut Frame,
    dashboard: &mut DashboardState,
    chats: &mut BTreeMap<String, ActiveChat>,
    opening_panes: &BTreeMap<PaneId, String>,
    transcript_selected: bool,
) -> Vec<String> {
    // NO_COLOR wins over the configured theme; the symbol set follows the
    // configuration or, unset, the terminal.
    let theme = theme::effective_theme(dashboard.config.theme);
    render_combined_with_theme(
        frame,
        dashboard,
        chats,
        opening_panes,
        transcript_selected,
        theme,
    )
}

#[cfg(test)]
pub(crate) fn render_combined_for_test(
    frame: &mut Frame,
    dashboard: &mut DashboardState,
    chats: &mut BTreeMap<String, ActiveChat>,
    opening_panes: &BTreeMap<PaneId, String>,
    transcript_selected: bool,
) -> Vec<String> {
    render_combined_with_theme(
        frame,
        dashboard,
        chats,
        opening_panes,
        transcript_selected,
        dashboard.config.theme,
    )
}

/// Draws the combined surface with a theme already selected by the caller.
///
/// This is useful for deterministic captures and tests. Interactive callers
/// should use [`render_combined`] so `NO_COLOR` can override configuration.
pub fn render_combined_with_theme(
    frame: &mut Frame,
    dashboard: &mut DashboardState,
    chats: &mut BTreeMap<String, ActiveChat>,
    opening_panes: &BTreeMap<PaneId, String>,
    transcript_selected: bool,
    selected_theme: theme::UiTheme,
) -> Vec<String> {
    dashboard.drawn_failures.clear();
    let symbols = theme::symbols_for(dashboard.config.advanced.symbols);
    theme::with_theme(selected_theme, || {
        theme::with_symbols(symbols, || {
            render_combined_themed(frame, dashboard, chats, opening_panes, transcript_selected)
        })
    })
}

fn render_combined_themed(
    frame: &mut Frame,
    dashboard: &mut DashboardState,
    chats: &mut BTreeMap<String, ActiveChat>,
    opening_panes: &BTreeMap<PaneId, String>,
    transcript_selected: bool,
) -> Vec<String> {
    dashboard.rebuild_palette_entries();
    dashboard.reset_component_geometry();
    dashboard.begin_surface_frame();
    for session_id in dashboard.pane_session_ids() {
        if let Some(chat) = chats.get_mut(&session_id) {
            chat.reset_component_geometry();
        }
    }
    dashboard.pane_areas = None;
    dashboard.clear_workspace_tab_areas();
    dashboard.workspace_pane_area = None;
    dashboard.session_row_areas.clear();
    dashboard.project_heading_areas.clear();
    dashboard.pane_size_control_areas.clear();
    dashboard.frame_surfaces.clear();
    dashboard.conversation_pane_areas.clear();
    dashboard.conversation_area = None;
    let mut area = frame.area();
    frame.render_widget(Block::default().style(theme::base()), area);
    if dashboard.go.is_some() {
        let lines = dashboard
            .go_context()
            .into_iter()
            .map(Line::raw)
            .collect::<Vec<_>>();
        let paragraph = Paragraph::new(lines)
            .style(theme::muted())
            .wrap(Wrap { trim: false });
        let height = (paragraph.line_count(area.width.max(1)) as u16).min(area.height);
        frame.render_widget(paragraph, Rect::new(area.x, area.y, area.width, height));
        area.y += height;
        area.height = area.height.saturating_sub(height);
    }
    if area.width < NARROW_TERMINAL_WIDTH {
        render_terminal_too_small(
            frame,
            area,
            TerminalSizeRequirement::Width(NARROW_TERMINAL_WIDTH),
        );
        dashboard.end_surface_frame();
        return Vec::new();
    }
    // Below the sidebar width the Sessions list stacks above the
    // conversation in its compact form, and the support panes go under it.
    let narrow = area.width < MINIMUM_TERMINAL_WIDTH;
    dashboard.narrow_layout.set(narrow);
    dashboard.resume_sessions_area = match &dashboard.mode {
        Mode::ResumeDialog(dialog) => Some(resume_sessions_pane(
            area,
            dialog.has_preview(dashboard.resume_rows()),
        )),
        _ => None,
    };
    if dashboard.config_is_empty() && dashboard.state.sessions.is_empty() {
        render_onboarding_surface(frame, dashboard);
        return Vec::new();
    }

    let sidebar_width = if narrow {
        area.width
    } else {
        sessions_sidebar_width(area.width, dashboard.pane_size(SupportPane::Sessions))
    };
    let sidebar_right =
        !narrow && dashboard.config.sessions_side == mj_core::config::SessionsSide::Right;
    // Stacked, the sidebar's band height is only known once the bands are
    // allocated; the content area's top is moved down then.
    let content_area = if narrow {
        area
    } else {
        Rect::new(
            area.x + if sidebar_right { 0 } else { sidebar_width },
            area.y,
            area.width.saturating_sub(sidebar_width),
            area.height,
        )
    };
    let sidebar_area = Rect::new(
        if sidebar_right {
            content_area.right()
        } else {
            area.x
        },
        area.y,
        sidebar_width,
        area.height.saturating_sub(FOOTER_HEIGHT),
    );
    let workspace_area = Rect::new(
        sidebar_area.x,
        sidebar_area.y,
        sidebar_area.width,
        WORKSPACE_PANE_HEIGHT.min(sidebar_area.height),
    );
    let sidebar_top = workspace_area.height;
    let sessions_area = Rect::new(
        sidebar_area.x,
        sidebar_area.y.saturating_add(sidebar_top),
        sidebar_area.width,
        sidebar_area.height.saturating_sub(sidebar_top),
    );
    let selected_transition = dashboard
        .current_session_id()
        .and_then(|id| dashboard.state.sessions.get(id))
        .and_then(|session| {
            dashboard
                .transition_kind(&session.id)
                .map(|kind| (session.id.clone(), kind, false))
                .or_else(|| {
                    dashboard
                        .transition_failure_kind(&session.id)
                        .map(|kind| (session.id.clone(), kind, true))
                })
        });
    // With no conversation the prompt band holds the two-line guidance that
    // stands in for a composer, so it asks for the rows to show both. A
    // retiring transition uses only the compact status panel. A
    // Starting/Resuming transition or an in-flight attach parks the real
    // composer in the band (the standby prompt), whose height grows with the
    // wrapped draft exactly like an attached chat's.
    let selected_session_id = dashboard.current_session_id().map(str::to_owned);
    // Pane widths do not depend on the band's height, so they can be measured
    // before the bands are allocated — which is what the composer heights the
    // allocation needs are measured against.
    let focused_pane = dashboard.focused_pane();
    let pane_widths = dashboard
        .conversation_panes(Rect::new(
            content_area.x,
            content_area.y,
            content_area.width,
            area.height,
        ))
        .into_iter()
        .map(|pane| (pane.id, pane.rect.width))
        .collect::<BTreeMap<_, _>>();
    let focused_width = pane_widths
        .get(&focused_pane)
        .copied()
        .unwrap_or(content_area.width);
    let focused_chat_session = focused_chat_on_screen(dashboard, chats, opening_panes);
    let standby_drawn = match &selected_transition {
        Some((_, kind, failed)) => {
            !failed
                && matches!(
                    kind,
                    SessionTransitionKind::Starting | SessionTransitionKind::Resuming
                )
        }
        None => {
            focused_chat_session.is_none()
                && selected_session_id
                    .as_deref()
                    .is_some_and(|session_id| dashboard.opening_session() == Some(session_id))
        }
    };
    // Between the new-session wizard closing and the daemon registering the
    // session there is no session to key a standby by, so the launch standby
    // fills the band instead and asks for the same room.
    let launch_standby_drawn =
        selected_transition.is_none() && dashboard.launch_standby_capturing();
    let focused_desired_prompt = if launch_standby_drawn {
        dashboard
            .launch_standby
            .as_ref()
            .map_or(PROMPT_MINIMUM, |standby| {
                standby.desired_prompt_height(focused_width)
            })
    } else if standby_drawn && let Some(session_id) = selected_session_id.as_deref() {
        dashboard
            .standby_prompt_mut(session_id)
            .desired_prompt_height(focused_width)
    } else if selected_transition.is_some() {
        PROMPT_MINIMUM
    } else {
        focused_chat_session
            .as_deref()
            .and_then(|session_id| chats.get(session_id))
            .map_or(EMPTY_PROMPT_HEIGHT, |chat| {
                chat.desired_prompt_height(focused_width)
            })
    };
    // The band has to hold the tallest composer the panes want, because every
    // pane's prompt is carved out of the one band.
    let desired_prompt = dashboard
        .pane_sessions
        .iter()
        .filter(|(pane, _)| **pane != focused_pane)
        .filter_map(|(pane, session_id)| {
            Some(
                chats
                    .get(session_id)?
                    .desired_prompt_height(*pane_widths.get(pane)?),
            )
        })
        .fold(focused_desired_prompt, u16::max);
    let sizes = [
        (
            SupportPane::Sessions,
            if narrow {
                PaneSize::Minimized
            } else {
                dashboard.pane_size(SupportPane::Sessions)
            },
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
                minimized: minimized_session_rows(
                    area.height,
                    minimized_sessions_content_height(dashboard, sidebar_width.saturating_sub(2))
                        .into(),
                )
                .saturating_add(SESSION_ACTIONS_HEIGHT)
                .saturating_add(2),
                full: sessions_content_height(dashboard, sidebar_width.saturating_sub(2))
                    .saturating_add(2),
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
                full: table_height(dashboard.config.enabled_profiles().count()),
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
    // Beside the conversation the sidebar takes no rows from it. Stacked, its
    // compact band (tabs, actions, and rows) is a fixed-height band above.
    let sessions = if narrow {
        let band = dimensions[0]
            .1
            .minimized
            .saturating_add(WORKSPACE_PANE_HEIGHT);
        PaneBand {
            minimum: band,
            full: band,
            cap: band,
        }
    } else {
        PaneBand {
            minimum: 0,
            full: 0,
            cap: 0,
        }
    };
    let targets = bands[1];
    let quota = bands[2];
    // Keep the support panes stacked together, using the space left by the
    // current sidebar size to decide whether they fit beside Sessions.
    let supports_adjacent = !narrow && support_panes_fit(content_area.width, dashboard);
    let mut maximize_enabled =
        maximized_pane_is_effective(area.height, dimensions, desired_prompt, sizes);
    maximize_enabled[0].1 = !narrow
        && sessions_sidebar_width(area.width, PaneSize::Maximized)
            > sessions_sidebar_width(area.width, PaneSize::Standard);
    dashboard.set_pane_maximize_enabled(maximize_enabled);
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
            dashboard.end_surface_frame();
            return Vec::new();
        }
    };

    let upper_content_height = heights.transcript.saturating_add(heights.prompt);
    let sessions_height = if narrow {
        heights.sessions.saturating_sub(sidebar_top)
    } else if supports_adjacent {
        area.height
            .saturating_sub(FOOTER_HEIGHT)
            .saturating_sub(sidebar_top)
    } else {
        upper_content_height.saturating_sub(sidebar_top)
    };
    let sessions_area = Rect::new(
        sessions_area.x,
        sessions_area.y,
        sessions_area.width,
        sessions_height,
    );
    // Stacked, everything below the sidebar band moves down by its height.
    let stacked_band = if narrow { heights.sessions } else { 0 };
    // The whole conversation band, before it is divided into a transcript
    // and a prompt. The tiled panes are laid out in this rectangle, so it is
    // what a split or a directional pane move is measured against.
    let conversation_area = Rect::new(
        content_area.x,
        content_area.y.saturating_add(stacked_band),
        content_area.width,
        upper_content_height,
    );
    dashboard.conversation_area = Some(conversation_area);
    let panes = dashboard.conversation_panes(conversation_area);
    // One pane keeps the band heights the allocator computed for the whole
    // frame. Several panes each carve their own leaf, because a pane's
    // composer is as tall as that pane's draft needs and no taller.
    let pane_bands = panes
        .iter()
        .map(|pane| {
            if panes.len() == 1 {
                let bands = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Length(heights.transcript),
                        Constraint::Length(heights.prompt),
                    ])
                    .split(pane.rect);
                return (pane.id, pane.rect, pane.is_focused, bands[0], bands[1]);
            }
            let desired = if pane.is_focused {
                focused_desired_prompt
            } else {
                dashboard
                    .pane_session(pane.id)
                    .and_then(|session_id| chats.get(session_id))
                    .map_or(EMPTY_PROMPT_HEIGHT, |chat| {
                        chat.desired_prompt_height(pane.rect.width)
                    })
            };
            let (transcript, prompt) = leaf_bands(pane.rect, desired);
            (pane.id, pane.rect, pane.is_focused, transcript, prompt)
        })
        .collect::<Vec<_>>();
    dashboard.conversation_pane_areas = pane_bands
        .iter()
        .map(|(id, _, _, transcript, prompt)| (*id, *transcript, *prompt))
        .collect();
    let support_area = Rect::new(
        if supports_adjacent {
            content_area.x
        } else {
            area.x
        },
        area.y
            .saturating_add(stacked_band)
            .saturating_add(upper_content_height),
        if supports_adjacent {
            content_area.width
        } else {
            area.width
        },
        heights.targets.saturating_add(heights.quota),
    );
    let support_bands = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(heights.targets),
            Constraint::Length(heights.quota),
        ])
        .split(support_area);
    let (targets_area, quota_area) = (support_bands[0], support_bands[1]);
    render_workspace_tabs(frame, workspace_area, dashboard);
    let footer_area = Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1);
    dashboard.pane_areas = Some([sessions_area, targets_area, quota_area]);
    for (pane, pane_area) in [
        (SupportPane::Sessions, sessions_area),
        (SupportPane::Targets, targets_area),
        (SupportPane::Quota, quota_area),
    ] {
        let maximize_enabled = dashboard.pane_maximize_enabled(pane);
        dashboard
            .pane_size_control_areas
            .extend(pane_size_control_areas(pane_area, pane, maximize_enabled));
    }

    let rendered = render_sessions(frame, sessions_area, dashboard);
    dashboard.session_row_areas = rendered.session_row_areas;
    dashboard.project_heading_areas = rendered.project_heading_areas;
    crate::surface_controls::render_session_row_actions(frame, dashboard);
    let sessions_content = bordered_content(sessions_area);
    dashboard.frame_surfaces.push(SurfaceFrame::fixed(
        SurfaceId::DashboardPane(0),
        sessions_content,
    ));

    // Draw support panes before chat: chat modals can cover the whole screen.
    // Targets and Quota choose their representations independently.
    if sizes[1].1 == PaneSize::Minimized {
        let focused = dashboard.focus() == Focus::Targets;
        frame.render_widget(
            theme::panel(focused)
                .borders(Borders::TOP)
                .title(minimized_targets_line(
                    dashboard,
                    pane_title_content_width(
                        targets_area.width,
                        dashboard.pane_maximize_enabled(SupportPane::Targets),
                    ),
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
            theme::panel(focused)
                .borders(Borders::TOP)
                .title(minimized_quota_line(
                    dashboard,
                    pane_title_content_width(
                        quota_area.width,
                        dashboard.pane_maximize_enabled(SupportPane::Quota),
                    ),
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

    let prompt_focused = dashboard.prompt_has_focus();
    // A focused border only says anything when there is another pane to tell
    // it apart from, so the single-pane surface draws as it always has.
    let focus_borders = pane_bands.len() > 1;
    let mut drawn_sessions = Vec::new();
    let mut chat_drew_footer = false;
    // The focused pane draws last so its focus border wins where two panes
    // meet. Its conversation's dialogs stay inside its own rectangle.
    for (pane_id, pane_rect, pane_focused, transcript_area, prompt_area) in pane_bands
        .iter()
        .filter(|(_, _, focused, _, _)| !focused)
        .chain(pane_bands.iter().filter(|(_, _, focused, _, _)| *focused))
        .copied()
        .collect::<Vec<_>>()
    {
        let native_id = dashboard
            .pane_session(pane_id)
            .filter(|id| dashboard.is_native_agent(id))
            .map(str::to_owned);
        if let Some(id) = native_id {
            dashboard.render_native_agent(frame, &id, transcript_area, prompt_area);
            continue;
        }
        if !pane_focused {
            // Every pane but the last carries a close chip, and an unfocused
            // pane only exists while there are several.
            let close_chip = true;
            // An unfocused pane answers only for the session it holds: the
            // Sessions selection and the launch standby belong to the pane
            // with the keyboard.
            let pane_session = dashboard.pane_session(pane_id).map(str::to_owned);
            let pane_transition = pane_session.as_deref().and_then(|session_id| {
                dashboard
                    .transition_kind(session_id)
                    .map(|kind| (session_id.to_owned(), kind, false))
                    .or_else(|| {
                        dashboard
                            .transition_failure_kind(session_id)
                            .map(|kind| (session_id.to_owned(), kind, true))
                    })
            });
            let opening = opening_panes.get(&pane_id).map(String::as_str);
            let chat = pane_session
                .as_deref()
                .filter(|session_id| {
                    pane_transition.is_none() && chat_shows_in_pane(opening, session_id)
                })
                .and_then(|session_id| chats.get_mut(session_id));
            let pane_failed = pane_session
                .as_deref()
                .filter(|session_id| opening.is_none() && dashboard.session_failed(session_id))
                .map(str::to_owned);
            match (chat, pane_transition) {
                (_, None) if pane_failed.is_some() => {
                    let session_id = pane_failed.unwrap_or_default();
                    if transcript_area.height > 3 {
                        dashboard.failure_drawn(&session_id);
                    }
                    render_empty_transcript(
                        frame,
                        transcript_area,
                        EmptyConversation::Failed,
                        dashboard.config.spinner,
                        Some(dashboard.session_failure_text(&session_id)),
                        crate::pane_controls::pane_title_reserve(
                            dashboard,
                            transcript_area.width,
                            title_controls(close_chip),
                        ),
                        false,
                        false,
                    );
                    render_empty_prompt_advice(
                        frame,
                        prompt_area,
                        false,
                        EmptyConversation::Failed,
                        dashboard,
                    );
                }
                (_, Some((session_id, transition, failed))) => render_transition_surface(
                    frame,
                    transcript_area,
                    prompt_area,
                    dashboard,
                    &session_id,
                    TransitionSurface {
                        transition,
                        failed,
                        pane_focused: false,
                        title_lead: crate::pane_controls::pane_chrome_width(
                            dashboard,
                            pane_id,
                            transcript_area.width,
                        ),
                    },
                ),
                (Some(chat), None) => {
                    drawn_sessions.push(chat.session_id().to_owned());
                    chat.draw_in(
                        frame,
                        ChatRegions {
                            transcript: transcript_area,
                            prompt: prompt_area,
                            footer: None,
                            overlay: pane_rect,
                            title_controls: crate::pane_controls::pane_title_reserve(
                                dashboard,
                                transcript_area.width,
                                crate::pane_controls::pane_title_reserve(
                                    dashboard,
                                    transcript_area.width,
                                    title_controls(close_chip),
                                ),
                            ),
                            title_lead: crate::pane_controls::pane_chrome_width(
                                dashboard,
                                pane_id,
                                transcript_area.width,
                            ),
                            pane_focused: false,
                        },
                        false,
                        false,
                    );
                }
                (None, None) => {
                    let reason = if opening.is_some() {
                        EmptyConversation::Opening
                    } else {
                        EmptyConversation::NoConversationOpen
                    };
                    render_empty_transcript(
                        frame,
                        transcript_area,
                        reason,
                        dashboard.config.spinner,
                        None,
                        crate::pane_controls::pane_title_reserve(
                            dashboard,
                            transcript_area.width,
                            title_controls(close_chip),
                        ),
                        false,
                        dashboard.pane_shows_pin_hint(pane_id),
                    );
                    render_empty_prompt_advice(frame, prompt_area, false, reason, dashboard);
                }
            }
            if close_chip {
                crate::surface_controls::render_pane_close_control(
                    frame,
                    dashboard,
                    transcript_area,
                    pane_id,
                );
            }
            continue;
        }
        // The lone pane offers a close only while it holds a conversation:
        // closing it empties it, which is nothing to ask for when it is
        // already empty.
        let zoom_chip = dashboard.conversation_zoomed();
        let close_chip = pane_bands.len() > 1 || dashboard.pane_session(pane_id).is_some();
        chat_drew_footer = if launch_standby_drawn {
            render_launch_standby_surface(
                frame,
                transcript_area,
                prompt_area,
                dashboard,
                focus_borders,
                crate::pane_controls::pane_chrome_width(dashboard, pane_id, transcript_area.width),
            );
            false
        } else if let Some((session_id, transition, failed)) = selected_transition.clone() {
            render_transition_surface(
                frame,
                transcript_area,
                prompt_area,
                dashboard,
                &session_id,
                TransitionSurface {
                    transition,
                    failed,
                    pane_focused: focus_borders,
                    title_lead: crate::pane_controls::pane_chrome_width(
                        dashboard,
                        pane_id,
                        transcript_area.width,
                    ),
                },
            );
            false
        } else {
            match focused_chat_session
                .as_deref()
                .and_then(|session_id| chats.get_mut(session_id))
            {
                Some(chat) => {
                    if !dashboard.launch_standby_capturing() {
                        drawn_sessions.push(chat.session_id().to_owned());
                    }
                    let chords = crate::render::footer_commands(
                        dashboard,
                        crate::actions::FooterGroup::Chord,
                    );
                    let chord_prefix = crate::render::chord_prefix(dashboard);
                    let commands = chords.clone();
                    let chords = chords
                        .iter()
                        .map(|(_, text)| text.as_str())
                        .collect::<Vec<_>>();
                    let banner = dashboard
                        .prefix_pending()
                        .then(|| crate::render::prefix_banner_line(dashboard))
                        .or_else(|| {
                            dashboard
                                .resize_mode_active()
                                .then(crate::render::resize_banner_line)
                        });
                    chat.draw_in(
                        frame,
                        ChatRegions {
                            transcript: transcript_area,
                            prompt: prompt_area,
                            footer: prompt_focused.then_some(ChatFooter {
                                area: footer_area,
                                chords: &chords,
                                chord_prefix: &chord_prefix,
                                functions: &[],
                                banner: banner.as_ref(),
                            }),
                            overlay: pane_rect,
                            title_controls: crate::pane_controls::pane_title_reserve(
                                dashboard,
                                transcript_area.width,
                                title_controls(close_chip) + zoom_title_controls(zoom_chip),
                            ),
                            title_lead: crate::pane_controls::pane_chrome_width(
                                dashboard,
                                pane_id,
                                transcript_area.width,
                            ),
                            pane_focused: focus_borders,
                        },
                        prompt_focused,
                        transcript_selected,
                    );
                    if prompt_focused {
                        // Draw the text the chat fitted into each area, not
                        // the host's hint: the first chord carries the prefix.
                        for (index, command_area, text) in chat.footer_command_areas() {
                            if let Some((id, _)) = commands.get(index) {
                                crate::surface_controls::render_footer_command(
                                    frame,
                                    command_area,
                                    dashboard,
                                    *id,
                                    &text,
                                );
                            }
                        }
                    }
                    // A chat's dialogs live inside its own pane, so its
                    // surfaces join the frame's rather than replacing them:
                    // the navigator and the other panes stay selectable.
                    dashboard.frame_surfaces.append(chat.frame_surfaces());
                    prompt_focused
                }
                None => {
                    let opening = dashboard.opening_session().is_some();
                    // A selected session whose target failed has no worker to
                    // attach to; say so and how to get out of it.
                    let failed = (!opening)
                        .then(|| selected_session_id.clone())
                        .flatten()
                        .filter(|session_id| dashboard.session_failed(session_id));
                    let reason = if opening {
                        EmptyConversation::Opening
                    } else if failed.is_some() {
                        EmptyConversation::Failed
                    } else if dashboard.ordered_sessions().is_empty() {
                        EmptyConversation::NoLiveSession
                    } else {
                        EmptyConversation::NoConversationOpen
                    };
                    if let Some(session_id) = failed.as_deref()
                        && transcript_area.height > 3
                    {
                        dashboard.failure_drawn(session_id);
                    }
                    render_empty_transcript(
                        frame,
                        transcript_area,
                        reason,
                        dashboard.config.spinner,
                        failed
                            .as_deref()
                            .map(|session_id| dashboard.session_failure_text(session_id)),
                        crate::pane_controls::pane_title_reserve(
                            dashboard,
                            transcript_area.width,
                            title_controls(close_chip) + zoom_title_controls(zoom_chip),
                        ),
                        focus_borders,
                        dashboard.pane_shows_pin_hint(pane_id),
                    );
                    if opening {
                        // The real composer parks in the prompt band while the
                        // attach runs, so anything typed lands in the chat that
                        // opens.
                        if let Some(session_id) = selected_session_id.as_deref() {
                            draw_standby_prompt(frame, prompt_area, dashboard, session_id, None);
                        }
                    } else {
                        render_empty_prompt_advice(
                            frame,
                            prompt_area,
                            prompt_focused,
                            reason,
                            dashboard,
                        );
                    }
                    false
                }
            }
        };
        if close_chip {
            crate::surface_controls::render_pane_close_control(
                frame,
                dashboard,
                transcript_area,
                pane_id,
            );
        }
        if zoom_chip {
            crate::surface_controls::render_pane_zoom_control(frame, dashboard, transcript_area);
        }
    }

    if !chat_drew_footer {
        render_footer(frame, footer_area, dashboard);
    }
    crate::pane_controls::render_pane_chrome(frame, dashboard);
    dashboard.end_surface_frame();
    render_modal(frame, area, dashboard);
    crate::pane_controls::render_pane_menu(frame, area, dashboard);
    drawn_sessions
}

/// The columns a pane's title has to leave clear for the close chip.
fn title_controls(close_chip: bool) -> u16 {
    if close_chip {
        crate::surface_controls::PANE_CLOSE_CONTROL_RESERVE
    } else {
        0
    }
}

/// The further columns the zoom chip takes, left of the close chip.
fn zoom_title_controls(zoom_chip: bool) -> u16 {
    if zoom_chip {
        crate::surface_controls::PANE_ZOOM_CONTROL_RESERVE
    } else {
        0
    }
}

/// How one pane's leaf divides into a transcript and a composer. The composer
/// takes what its draft asks for, within the same bounds the single band uses:
/// never under [`PROMPT_MINIMUM`] and never over a third of the pane.
fn leaf_bands(rect: Rect, desired_prompt: u16) -> (Rect, Rect) {
    let prompt = prompt_target(desired_prompt, rect.height).min(rect.height);
    let transcript_height = rect.height.saturating_sub(prompt);
    (
        Rect::new(rect.x, rect.y, rect.width, transcript_height),
        Rect::new(
            rect.x,
            rect.y.saturating_add(transcript_height),
            rect.width,
            prompt,
        ),
    )
}

/// Whether a pane shows the conversation it holds, or hides it because the
/// pane is still attaching to a different session.
fn chat_shows_in_pane(opening: Option<&str>, session_id: &str) -> bool {
    !matches!(opening, Some(opening) if opening != session_id)
}

/// The session the focused pane's conversation is drawn for, if it has one on
/// screen. A conversation stays off screen while its pane is attaching to
/// another session, while the session it belongs to is in a transition, and
/// while the Sessions selection names a different row: the transcript must
/// never belong to a row other than the highlighted one.
fn focused_chat_on_screen(
    dashboard: &DashboardState,
    chats: &BTreeMap<String, ActiveChat>,
    opening_panes: &BTreeMap<PaneId, String>,
) -> Option<String> {
    let pane = dashboard.focused_pane();
    let session_id = dashboard.pane_session(pane)?;
    let opening = opening_panes.get(&pane).map(String::as_str);
    (chats.contains_key(session_id)
        && chat_shows_in_pane(opening, session_id)
        && dashboard.transition_kind(session_id).is_none()
        && dashboard.transition_failure_kind(session_id).is_none())
    .then(|| session_id.to_owned())
}

/// Why the conversation band is empty, which is what decides the advice it
/// gives.
#[derive(Clone, Copy)]
enum EmptyConversation {
    /// The workspace has no live session at all.
    NoLiveSession,
    /// The workspace has live sessions and none of them is open.
    NoConversationOpen,
    /// An attach is in flight, so a conversation is on its way.
    Opening,
    /// The selected session's target failed: nothing to attach to until it
    /// is recovered or its transcript is opened on purpose.
    Failed,
}

/// The bordered chrome that stands in for a conversation when none is on
/// screen. The three reasons for an empty band need different advice: a
/// workspace with no live session needs one created or resumed, a workspace
/// that has live sessions just needs one opened, and an attach that is still
/// running needs nothing but a moment. Telling the second user there is no
/// live session would be a plain lie — the pane above is listing them.
#[allow(clippy::too_many_arguments)]
fn render_empty_transcript(
    frame: &mut Frame,
    transcript_area: Rect,
    reason: EmptyConversation,
    spinner_style: spinner::SpinnerStyle,
    failure: Option<String>,
    title_controls: u16,
    pane_focused: bool,
    pin_hint: bool,
) {
    if matches!(reason, EmptyConversation::Failed) {
        let panel = theme::panel(pane_focused)
            .title(" Session failed ")
            .title_style(
                Style::default()
                    .fg(theme::palette().error)
                    .add_modifier(ratatui::style::Modifier::BOLD),
            );
        let inner = panel.inner(transcript_area);
        frame.render_widget(panel, transcript_area);
        let body = Rect::new(
            inner.x.saturating_add(1),
            inner.y.saturating_add(1),
            inner.width.saturating_sub(2),
            inner.height.saturating_sub(1),
        );
        let mut lines = vec![
            Line::styled(
                "The session's target failed, so there is no conversation to open.",
                Style::default().fg(theme::palette().error),
            ),
            Line::default(),
        ];
        lines.extend(
            failure
                .unwrap_or_default()
                .lines()
                .map(|line| Line::styled(line.to_owned(), theme::muted())),
        );
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), body);
        return;
    }
    let mut panel = theme::panel(pane_focused).title(" Conversation ");
    if matches!(reason, EmptyConversation::Opening) {
        panel = panel.title(
            Line::from(vec![
                Span::raw(" "),
                spinner::compact_span(spinner_style, spinner::elapsed_ms()),
                Span::raw(" "),
                // Clear of whatever the host draws at the right of the row.
                Span::raw(" ".repeat(usize::from(title_controls))),
            ])
            .right_aligned(),
        );
    }
    frame.render_widget(panel, transcript_area);
    // The pane chrome puts "Pin selected here" on the first inside row of
    // an empty pane; the splash starts below it, and is left out when the
    // pane is too short for both.
    let first_row = if pin_hint { 2 } else { 1 };
    let top = (transcript_area.height.saturating_sub(5) / 2).max(first_row);
    if transcript_area.height >= 7 && top + 3 < transcript_area.height {
        let hero = Rect::new(
            transcript_area.x.saturating_add(1),
            transcript_area.y + top,
            transcript_area.width.saturating_sub(2),
            (transcript_area.height - 1 - top).min(4),
        );
        let invitation = match reason {
            EmptyConversation::NoLiveSession => {
                "A little spark. Something extraordinary.".to_owned()
            }
            EmptyConversation::NoConversationOpen => "Your next idea starts here.".to_owned(),
            EmptyConversation::Opening => format!(
                "Bringing your conversation into focus{}",
                theme::glyphs().ellipsis
            ),
            EmptyConversation::Failed => String::new(),
        };
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled(
                    format!("{}  M J O L N I R", theme::glyphs().spark),
                    theme::title(true),
                ),
                Line::default(),
                Line::styled(invitation, theme::muted()),
            ])
            .alignment(ratatui::layout::Alignment::Center),
            hero,
        );
    }
}

/// The prompt-band advice that accompanies [`render_empty_transcript`] for
/// the two reasons where there is nothing to type toward yet. While an attach
/// runs the standby composer fills the band instead, so `Opening` never
/// reaches here.
fn render_empty_prompt_advice(
    frame: &mut Frame,
    prompt_area: Rect,
    prompt_focused: bool,
    reason: EmptyConversation,
    dashboard: &DashboardState,
) {
    let (title, lines) = match reason {
        EmptyConversation::NoLiveSession => (
            " No live session ",
            [
                "No live session in this workspace.".to_owned(),
                match (
                    dashboard.first_key_label(crate::CommandId::NewSessionWizard),
                    dashboard.first_key_label(crate::CommandId::ResumeDialog),
                ) {
                    // `prefix+g` opens the session list, which shows running
                    // sessions first and stopped ones a tab away, so telling
                    // someone it resumes a session misses most of what it does.
                    (Some(create), Some(sessions)) => {
                        format!("Press {create} to create a session or {sessions} to find one.")
                    }
                    _ => "Create a session, or find an existing one, from the buttons above."
                        .to_owned(),
                },
            ],
        ),
        EmptyConversation::NoConversationOpen => (
            " No conversation open ",
            [
                "No conversation open.".to_owned(),
                "Press Tab for Sessions, then Enter on the one to open.".to_owned(),
            ],
        ),
        EmptyConversation::Opening => (
            " Opening session ",
            [format!("Opening session{}", theme::glyphs().ellipsis), {
                let separator = theme::glyphs().footer_separator;
                match dashboard.first_key_label(crate::CommandId::QuitDetach) {
                    Some(detach) => format!(
                        "Esc cancels{separator}select another session to switch{separator}{detach} quits"
                    ),
                    None => format!("Esc cancels{separator}select another session to switch"),
                }
            }],
        ),
        EmptyConversation::Failed => (
            " Session failed ",
            [
                "Enter in Sessions opens its transcript or recovers it on a fresh target."
                    .to_owned(),
                match dashboard.first_key_label(crate::CommandId::Palette) {
                    Some(palette) => {
                        format!("{palette} then Delete session removes it for good.")
                    }
                    None => "Delete session in the command palette removes it for good.".to_owned(),
                },
            ],
        ),
    };
    frame.render_widget(
        Paragraph::new(lines.into_iter().map(Line::raw).collect::<Vec<_>>())
            .style(theme::muted())
            .wrap(Wrap { trim: true })
            .block(theme::panel(prompt_focused).title(title)),
        prompt_area,
    );
}

/// Draws the session's standby composer — the real chat prompt — into the
/// prompt band, and merges its surfaces into the dashboard's so clicks and
/// selections inside the band behave like an attached chat's. `note` adds a
/// left-aligned line to the band's bottom border.
fn draw_standby_prompt(
    frame: &mut Frame,
    prompt_area: Rect,
    dashboard: &mut DashboardState,
    session_id: &str,
    note: Option<Line<'static>>,
) {
    let focused = dashboard.prompt_has_focus();
    let surfaces = {
        let standby = dashboard.standby_prompt_mut(session_id);
        draw_standby_band(frame, prompt_area, standby, focused, note)
    };
    dashboard.frame_surfaces.append(&surfaces);
}

/// Draws one standby composer into the prompt band and reports its surfaces,
/// shared by the per-session standby and the launch standby.
fn draw_standby_band(
    frame: &mut Frame,
    prompt_area: Rect,
    standby: &mut ChatState,
    focused: bool,
    note: Option<Line<'static>>,
) -> FrameSurfaces {
    standby.draw_prompt_band(frame, prompt_area, focused, note);
    standby.frame_surfaces().clone()
}

/// Draws the surface shown between the new-session wizard closing and the
/// daemon registering the session: the launch has no session record yet, so
/// the panel reports the stage in general terms and the launch standby holds
/// whatever is typed until the new session's standby adopts it.
fn render_launch_standby_surface(
    frame: &mut Frame,
    transcript_area: Rect,
    prompt_area: Rect,
    dashboard: &mut DashboardState,
    pane_focused: bool,
    title_lead: u16,
) {
    let panel = theme::panel(pane_focused)
        .title(transition_title(
            SessionTransitionKind::Starting,
            title_lead,
        ))
        .title(
            Line::from(vec![
                Span::raw(" "),
                spinner::compact_span(dashboard.config.spinner, spinner::elapsed_ms()),
                Span::raw(" "),
            ])
            .right_aligned(),
        );
    let details = vec![
        Line::raw(format!(
            "Current stage: Preparing session launch{}",
            theme::glyphs().ellipsis
        )),
        Line::default(),
        Line::styled(
            "Type the first message now; it is sent when the session is live.",
            theme::muted(),
        ),
    ];
    frame.render_widget(
        Paragraph::new(details)
            .wrap(Wrap { trim: true })
            .block(panel),
        transcript_area,
    );
    let focused = dashboard.prompt_has_focus();
    let Some(surfaces) = dashboard
        .launch_standby
        .as_mut()
        .map(|standby| draw_standby_band(frame, prompt_area, standby, focused, None))
    else {
        return;
    };
    dashboard.frame_surfaces.append(&surfaces);
}

/// Draws the conversation replacement shown while a lifecycle owns the
/// selected session. The warm chat remains alive off-screen so its draft,
/// read cursor, and history survive the operation, but the transcript cannot
/// be mistaken for a session that is being retired.
///
/// Starting and Resuming keep the real composer on screen: the standby prompt
/// below the transition panel is the chat's own composer, so the first
/// message can be drafted with the usual readline keys before the session is
/// live (Enter holds the draft and explains), and the draft carries into the
/// conversation when the chat opens. Retiring and failed transitions have no
/// conversation to type toward, so they keep the plain status panel.
/// What one pane's transition panel reports: the operation under way, whether
/// it failed, and whether the pane holding it has the keyboard.
#[derive(Clone, Copy)]
struct TransitionSurface {
    transition: SessionTransitionKind,
    failed: bool,
    pane_focused: bool,
    /// Columns the pane chrome draws its label into at the left of the
    /// title row; the title starts after them.
    title_lead: u16,
}

/// A transition panel title that starts after the pane chrome label, the
/// same way the conversation title does, so the label does not cover the
/// state word (launch findings B-2 and D-1: "Conversation g" for
/// Suspending).
fn transition_title(transition: SessionTransitionKind, lead: u16) -> String {
    format!(
        "{} Transition · {} ",
        " ".repeat(usize::from(lead)),
        transition.label()
    )
}

fn render_transition_surface(
    frame: &mut Frame,
    transcript_area: Rect,
    prompt_area: Rect,
    dashboard: &mut DashboardState,
    session_id: &str,
    surface: TransitionSurface,
) {
    let TransitionSurface {
        transition,
        failed,
        pane_focused,
        title_lead,
    } = surface;
    if failed && transcript_area.height > 3 {
        dashboard.failure_drawn(session_id);
    }
    let Some(session) = dashboard.state.sessions.get(session_id) else {
        return;
    };
    let operation = dashboard.session_operations.get(session_id);
    let stages = operation
        .map(|operation| {
            operation
                .active_stages
                .keys()
                .map(|stage| stage.label())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .filter(|stages| !stages.is_empty())
        .unwrap_or_else(|| "waiting".to_owned());
    let started_at = operation
        .map(|operation| {
            operation
                .active_stages
                .values()
                .copied()
                .min()
                .unwrap_or(operation.started_at_epoch_seconds)
        })
        .unwrap_or_else(|| {
            chrono::DateTime::parse_from_rfc3339(&session.updated_at)
                .ok()
                .and_then(|time| u64::try_from(time.timestamp()).ok())
                .unwrap_or_default()
        });
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let target_id = operation
        .and_then(|operation| operation.resume_destination.as_ref())
        .map(|(_, target)| target.as_str())
        .unwrap_or(&session.target_template_id);
    let target = dashboard
        .state
        .project_identity_session(session)
        .project_target(&dashboard.config, target_id);
    let profile = operation
        .and_then(|operation| operation.resume_destination.as_ref())
        .map(|(profile, _)| profile.as_str())
        .unwrap_or(&session.last_profile);
    let mut panel = theme::panel(pane_focused).title(transition_title(transition, title_lead));
    if !failed {
        panel = panel.title(
            Line::from(vec![
                Span::raw(" "),
                spinner::compact_span(dashboard.config.spinner, spinner::elapsed_ms()),
                Span::raw(" "),
            ])
            .right_aligned(),
        );
    }
    let type_ahead = !failed
        && matches!(
            transition,
            SessionTransitionKind::Starting | SessionTransitionKind::Resuming
        );
    let mut details = vec![
        Line::styled(
            format!("{} · {}", target, session.display_title()),
            Style::default().add_modifier(ratatui::style::Modifier::BOLD),
        ),
        Line::raw(format!("Operation: {}", transition.label())),
        Line::raw(format!("Current stage: {stages}")),
        Line::raw(format!(
            "Elapsed: {} · Profile: {profile}",
            mj_client::usage_format::format_clock(now.saturating_sub(started_at))
        )),
    ];
    if type_ahead {
        details.push(Line::default());
        details.push(Line::styled(
            "Select another session to keep working.",
            theme::muted(),
        ));
    }
    frame.render_widget(
        Paragraph::new(details)
            .wrap(Wrap { trim: true })
            .block(panel),
        transcript_area,
    );
    if type_ahead {
        let note = operation
            .is_some_and(|operation| operation.cancellable)
            .then(|| {
                Line::styled(
                    format!(
                        " {} to cancel {} ",
                        dashboard
                            .first_key_label(crate::CommandId::CancelOperation)
                            .unwrap_or_else(|| "the cancel key".to_owned()),
                        transition.label().to_lowercase()
                    ),
                    theme::muted(),
                )
                .left_aligned()
            });
        draw_standby_prompt(frame, prompt_area, dashboard, session_id, note);
        return;
    }
    let cancel_line = if !failed && operation.is_some_and(|operation| operation.cancellable) {
        format!(
            "{} to cancel {}",
            dashboard
                .first_key_label(crate::CommandId::CancelOperation)
                .unwrap_or_else(|| "The cancel key".to_owned()),
            transition.label().to_lowercase()
        )
    } else if failed {
        format!(
            "Operation failed: {}",
            session.last_error.as_deref().unwrap_or("recovery required")
        )
    } else if operation.is_some() {
        "This operation is at its commit boundary.".to_owned()
    } else {
        "This transition is owned by the daemon; select another session.".to_owned()
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::styled(
                cancel_line,
                Style::default().add_modifier(ratatui::style::Modifier::BOLD),
            ),
            Line::raw(if failed {
                "Press Enter for recovery, or select another session."
            } else {
                "Select another session to keep working."
            }),
        ])
        .style(theme::muted())
        .wrap(Wrap { trim: true })
        .block(theme::panel(false).title(" Status ")),
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
    use crate::test_support::{buffer_lines, dashboard_with_session, key, running_session};
    use crate::{DashboardAction, SessionOperationKind};
    use crossterm::event::KeyCode;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// Launch finding E-8 (right edge): a conversation title too long for its
    /// row stopped at the pane chips with no ellipsis ("End to follo ◇").
    /// The title must end in an ellipsis before the chips.
    #[tokio::test]
    async fn a_long_conversation_title_ends_in_an_ellipsis_before_the_pane_chips() {
        let session = running_session();
        let session_id = session.id.clone();
        for width in [140_u16, 100, 80] {
            let mut dashboard = dashboard_with_session(session.clone());
            let fixture = mj_client::session::replacement_session_test_fixture(&session_id, 1);
            let chat = ActiveChat::open(
                fixture.stopped,
                "hel",
                None,
                fixture.control,
                mj_chat::chat::SessionHeaderIdentity {
                    title: "a very long session title ".repeat(8),
                    ..Default::default()
                },
                String::new(),
                mj_chat::chat::Notices::default(),
            );
            let mut chats = BTreeMap::from([(session_id.clone(), chat)]);
            let mut terminal = Terminal::new(TestBackend::new(width, 40)).unwrap();
            terminal
                .draw(|frame| {
                    render_combined_for_test(
                        frame,
                        &mut dashboard,
                        &mut chats,
                        &BTreeMap::new(),
                        false,
                    );
                })
                .unwrap();
            let lines = buffer_lines(terminal.backend().buffer());
            let row = &lines[0];
            let title = row
                .split_once("| Conversation")
                .map(|(_, title)| title)
                .unwrap_or_else(|| panic!("{width}: no pane title in {row}"));
            let before_chips = title.split(theme::glyphs().pin).next().unwrap();
            assert!(
                before_chips.trim_end_matches([' ', '─']).ends_with('…'),
                "{width}: {row}"
            );
        }
    }

    /// Launch finding A-12 / R1-2: the composer footer drew the host's
    /// unlabeled first chord over the "ctrl+b then " label, so the row read
    /// "g sessionsn g sessions". The label and the chord after it must read
    /// as one intact hint at every width the campaign captured.
    #[tokio::test]
    async fn composer_footer_keeps_the_chord_label_and_first_chord_apart() {
        let session = running_session();
        let session_id = session.id.clone();
        for width in [140_u16, 100, 80] {
            let mut dashboard = dashboard_with_session(session.clone());
            dashboard.focus = Focus::Prompt;
            let fixture = mj_client::session::replacement_session_test_fixture(&session_id, 1);
            let chat = ActiveChat::open(
                fixture.stopped,
                "hel",
                None,
                fixture.control,
                mj_chat::chat::SessionHeaderIdentity::default(),
                String::new(),
                mj_chat::chat::Notices::default(),
            );
            let mut chats = BTreeMap::from([(session_id.clone(), chat)]);
            let mut terminal = Terminal::new(TestBackend::new(width, 40)).unwrap();
            terminal
                .draw(|frame| {
                    render_combined_for_test(
                        frame,
                        &mut dashboard,
                        &mut chats,
                        &BTreeMap::new(),
                        false,
                    );
                })
                .unwrap();
            let lines = buffer_lines(terminal.backend().buffer());
            let footer = lines.last().unwrap();
            let chord_group = footer
                .split(theme::footer_group_separator())
                .nth(1)
                .unwrap_or_else(|| panic!("{width}: no chord group in {footer:?}"));
            let label = chord_group
                .trim_start()
                .strip_prefix("ctrl+b then ")
                .unwrap_or_else(|| panic!("{width}: label overdrawn in {footer:?}"));
            let first = label.split(theme::footer_separator()).next().unwrap();
            let chords =
                crate::render::footer_commands(&dashboard, crate::actions::FooterGroup::Chord);
            assert!(
                chords
                    .iter()
                    .any(|(_, text)| text.as_str() == first.trim_end()),
                "{width}: {first:?} is not a whole chord in {footer:?}"
            );
        }
    }

    /// Launch campaign finding A-13: in an empty pane the "Pin selected
    /// here" hint on the first inside row was drawn over the splash at 79
    /// and 60 columns. The splash keeps clear of that row, and is dropped
    /// when the pane is too short for both.
    #[test]
    fn the_splash_keeps_clear_of_the_pin_hint_row() {
        for height in 3..=16 {
            let mut terminal = Terminal::new(TestBackend::new(34, height)).unwrap();
            terminal
                .draw(|frame| {
                    render_empty_transcript(
                        frame,
                        frame.area(),
                        EmptyConversation::NoConversationOpen,
                        spinner::SpinnerStyle::default(),
                        None,
                        0,
                        false,
                        true,
                    );
                })
                .unwrap();
            let lines = buffer_lines(terminal.backend().buffer());
            assert!(
                !lines[1].contains("M J O") && !lines[1].contains("Your next"),
                "height {height}: {lines:#?}"
            );
            if height >= 8 {
                assert!(
                    lines.iter().any(|line| line.contains("M J O L N I R")),
                    "height {height}: {lines:#?}"
                );
            }
        }
    }

    /// Launch campaign finding A-15: the "Opening session" advice kept a
    /// hard-coded "·" in ASCII symbol mode.
    #[test]
    fn the_opening_advice_uses_the_symbol_set_in_force() {
        let dashboard = dashboard_with_session(running_session());
        let lines = theme::with_symbols(theme::SymbolSet::Ascii, || {
            let mut terminal = Terminal::new(TestBackend::new(100, 6)).unwrap();
            terminal
                .draw(|frame| {
                    render_empty_prompt_advice(
                        frame,
                        frame.area(),
                        false,
                        EmptyConversation::Opening,
                        &dashboard,
                    );
                })
                .unwrap();
            buffer_lines(terminal.backend().buffer())
        });
        let text = lines.join("\n");
        assert!(text.contains("Esc cancels"), "{text}");
        assert!(text.is_ascii(), "{text}");
    }

    #[test]
    fn minimized_sessions_use_two_content_lines_per_visible_item() {
        assert_eq!(minimized_session_rows(40, 0), 1);
        assert_eq!(minimized_session_rows(40, 1), 1);
        assert_eq!(minimized_session_rows(40, 3), 3);
        assert_eq!(minimized_session_rows(40, 4), 4);
    }

    #[test]
    fn minimized_sessions_cap_content_lines_at_four_short_and_ten_tall() {
        assert_eq!(minimized_session_rows(39, 6), 4);
        assert_eq!(minimized_session_rows(39, 100), 4);
        assert_eq!(minimized_session_rows(40, 15), 10);
        assert_eq!(minimized_session_rows(100, 100), 10);
    }

    #[test]
    fn prompt_target_clamps_between_the_minimum_and_a_third() {
        // Below the minimum is raised to it; within range is kept; above a
        // third of the frame is capped there.
        assert_eq!(prompt_target(2, 60), PROMPT_MINIMUM);
        assert_eq!(prompt_target(5, 60), 5);
        assert_eq!(prompt_target(50, 60), 20);
    }

    /// A Starting transition turns the prompt band into the standby composer —
    /// the real chat prompt: the draft is on screen, the cancel chord moved
    /// onto the pane's bottom border, and the old status panel is gone.
    #[test]
    fn a_starting_transition_draws_the_standby_composer_with_the_cancel_chord_at_its_bottom() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.begin_session_operation(
            "session-1".into(),
            SessionOperationKind::Launching,
            None,
        );
        dashboard.focus_prompt();
        for character in "first message ahead".chars() {
            dashboard.handle_key(key(KeyCode::Char(character)));
        }
        let mut terminal = Terminal::new(TestBackend::new(100, 40)).unwrap();
        terminal
            .draw(|frame| {
                crate::render::render(frame, &mut dashboard);
            })
            .unwrap();

        let prompt = dashboard
            .focused_prompt_area()
            .expect("prompt pane rendered");
        let lines = buffer_lines(terminal.backend().buffer());
        // The draft is editable text inside the pane...
        let content = &lines[prompt.y as usize + 1..prompt.bottom() as usize - 1];
        assert!(
            content
                .iter()
                .any(|line| line.contains("first message ahead")),
            "draft missing from {content:?}"
        );
        // ...and the cancel chord sits on the pane's bottom border row.
        let border = &lines[prompt.bottom() as usize - 1];
        assert!(
            border.contains("ctrl+b shift+c to cancel starting"),
            "cancel chord missing from {border:?}"
        );
        assert!(
            lines.iter().all(|line| !line.contains(" Status ")),
            "the status panel should not replace the composer: {:?}",
            lines
        );
        assert_eq!(
            dashboard.take_standby_prompt_draft("session-1").as_deref(),
            Some("first message ahead")
        );
        // The keystrokes were input, not actions.
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::None
        );
    }

    /// A launch that has not registered yet draws the same pair of panels a
    /// Starting transition does, with the launch standby in the band, so the
    /// typing has somewhere visible to go before the session exists.
    #[test]
    fn a_launch_being_prepared_draws_the_launch_standby() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.begin_launch_standby(mj_chat::chat::SessionHeaderIdentity {
            target: "/tmp/project".into(),
            profile: "profile-1".into(),
            title: String::new(),
            harness_kind: None,
            subagent_count: 0,
        });
        for character in "first message ahead".chars() {
            dashboard.handle_key(key(KeyCode::Char(character)));
        }
        let mut terminal = Terminal::new(TestBackend::new(100, 40)).unwrap();
        terminal
            .draw(|frame| {
                crate::render::render(frame, &mut dashboard);
            })
            .unwrap();

        let prompt = dashboard
            .focused_prompt_area()
            .expect("prompt pane rendered");
        let lines = buffer_lines(terminal.backend().buffer());
        assert!(
            lines
                .iter()
                .any(|line| line.contains("Preparing session launch")),
            "launch stage missing from {lines:?}"
        );
        let content = &lines[prompt.y as usize + 1..prompt.bottom() as usize - 1];
        assert!(
            content
                .iter()
                .any(|line| line.contains("first message ahead")),
            "draft missing from {content:?}"
        );
    }

    /// An empty standby composer still shows a composer, with the placeholder
    /// that says what typing ahead means.
    #[test]
    fn an_empty_standby_composer_shows_the_placeholder() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.begin_session_operation("session-1".into(), SessionOperationKind::Resuming, None);
        dashboard.focus_prompt();
        let mut terminal = Terminal::new(TestBackend::new(100, 40)).unwrap();
        terminal
            .draw(|frame| {
                crate::render::render(frame, &mut dashboard);
            })
            .unwrap();

        let prompt = dashboard
            .focused_prompt_area()
            .expect("prompt pane rendered");
        let lines = buffer_lines(terminal.backend().buffer());
        let content = &lines[prompt.y as usize + 1..prompt.bottom() as usize - 1];
        assert!(
            content
                .iter()
                .any(|line| line.contains("Type a draft · sending opens when the session is live")),
            "placeholder missing from {content:?}"
        );
    }

    /// Retiring transitions keep the status panel: there is no conversation
    /// to type toward while the session is being stopped.
    #[test]
    fn a_suspending_transition_keeps_the_status_panel() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.begin_session_operation(
            "session-1".into(),
            SessionOperationKind::Suspending,
            None,
        );
        let mut terminal = Terminal::new(TestBackend::new(100, 40)).unwrap();
        terminal
            .draw(|frame| {
                crate::render::render(frame, &mut dashboard);
            })
            .unwrap();

        let lines = buffer_lines(terminal.backend().buffer());
        assert!(
            lines.iter().any(|line| line.contains(" Status ")),
            "status panel missing: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("ctrl+b shift+c to cancel suspending")),
            "cancel chord missing: {lines:?}"
        );
    }

    /// Launch findings B-2 / D-1 for Suspending: the transition panel's title
    /// started under the pane chrome label, so the row read
    /// "Conversation g". It starts after the label, as the chat title does.
    #[test]
    fn the_pane_chrome_does_not_cover_the_transition_title() {
        for (kind, word) in [
            (SessionOperationKind::Suspending, "Suspending"),
            (SessionOperationKind::Launching, "Starting"),
        ] {
            let mut dashboard = dashboard_with_session(running_session());
            dashboard.begin_session_operation("session-1".into(), kind, None);
            for width in [140_u16, 100] {
                let mut terminal = Terminal::new(TestBackend::new(width, 40)).unwrap();
                terminal
                    .draw(|frame| crate::render::render(frame, &mut dashboard))
                    .unwrap();
                let lines = buffer_lines(terminal.backend().buffer());
                assert!(
                    lines
                        .iter()
                        .any(|line| line.contains(&format!("Transition · {word}"))),
                    "{width}: {word} title covered: {lines:?}"
                );
            }
        }
    }

    /// The standby composer keeps the empty chat composer's floor and grows
    /// with the wrapped draft, and retiring transitions ask for none of it.
    #[test]
    fn standby_prompt_height_follows_the_wrapped_draft() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.begin_session_operation(
            "session-1".into(),
            SessionOperationKind::Launching,
            None,
        );
        assert_eq!(
            dashboard
                .standby_prompts
                .get("session-1")
                .map(|standby| standby.desired_prompt_height(100)),
            None
        );

        dashboard.seed_standby_prompt("session-1", "a\nb\nc\nd\ne\nf\ng".into());
        assert_eq!(
            dashboard
                .standby_prompts
                .get("session-1")
                .map(|standby| standby.draft()),
            Some("a\nb\nc\nd\ne\nf\ng".into())
        );
        assert_eq!(
            dashboard
                .standby_prompts
                .get("session-1")
                .map(|standby| standby.desired_prompt_height(100)),
            Some(9)
        );

        dashboard.finish_session_operation("session-1");
        dashboard.begin_session_operation(
            "session-1".into(),
            SessionOperationKind::Suspending,
            None,
        );
        assert_eq!(dashboard.standby_prompt_session(), None);
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

    #[test]
    fn support_layout_moves_both_panes_at_the_measured_threshold_for_every_sidebar_size() {
        use mj_core::config::SessionsSide;
        use ratatui::{Terminal, backend::TestBackend};
        for side in [SessionsSide::Left, SessionsSide::Right] {
            for size in [PaneSize::Minimized, PaneSize::Standard, PaneSize::Maximized] {
                let mut dashboard = dashboard_with_session(running_session());
                dashboard.config.sessions_side = side;
                dashboard.set_pane_size(SupportPane::Sessions, size);
                let required = capacity_table_width(&dashboard).max(quota_table_width(&dashboard));
                let threshold = (MINIMUM_TERMINAL_WIDTH..=480)
                    .find(|&width| width - sessions_sidebar_width(width, size) >= required)
                    .expect("support panes fit in a wide terminal");
                for width in [
                    threshold.saturating_sub(1).max(MINIMUM_TERMINAL_WIDTH),
                    threshold,
                    80,
                    480,
                ] {
                    let mut terminal = Terminal::new(TestBackend::new(width, 40)).unwrap();
                    terminal
                        .draw(|frame| {
                            crate::render::render(frame, &mut dashboard);
                        })
                        .unwrap();
                    let [sessions, targets, quota] = dashboard.pane_areas.expect("rendered panes");
                    assert_eq!(targets.x, quota.x);
                    assert_eq!(targets.width, quota.width);
                    assert_eq!(targets.bottom(), quota.y);
                    assert_eq!(sessions.y, 3, "workspace pane occupies three rows");
                    if width < threshold {
                        assert_eq!(targets.width, width);
                        assert_eq!(targets.y, sessions.bottom());
                    } else {
                        assert_eq!(targets.width, width - sessions.width);
                        assert_eq!(sessions.bottom(), 39);
                    }
                }
            }
        }
    }

    #[test]
    fn resizing_sidebar_relayouts_support_panes_without_resizing_terminal() {
        use mj_core::config::SessionsSide;
        use ratatui::{Terminal, backend::TestBackend};

        for side in [SessionsSide::Left, SessionsSide::Right] {
            let mut dashboard = dashboard_with_session(running_session());
            dashboard.config.sessions_side = side;
            let required = capacity_table_width(&dashboard).max(quota_table_width(&dashboard));
            let width = (MINIMUM_TERMINAL_WIDTH..=480)
                .find(|&width| {
                    width - sessions_sidebar_width(width, PaneSize::Minimized) >= required
                        && width - sessions_sidebar_width(width, PaneSize::Maximized) < required
                })
                .expect("sidebar sizes straddle the support layout threshold");
            let mut terminal = Terminal::new(TestBackend::new(width, 40)).unwrap();
            for size in [
                PaneSize::Maximized,
                PaneSize::Minimized,
                PaneSize::Maximized,
            ] {
                dashboard.set_pane_size(SupportPane::Sessions, size);
                terminal
                    .draw(|frame| {
                        crate::render::render(frame, &mut dashboard);
                    })
                    .unwrap();
                let [sessions, targets, quota] = dashboard.pane_areas.unwrap();
                assert_eq!(targets.x, quota.x);
                assert_eq!(targets.width, quota.width);
                assert_eq!(targets.bottom(), quota.y);
                if size == PaneSize::Minimized {
                    assert_eq!(targets.width, width - sessions.width);
                    assert_eq!(sessions.bottom(), 39);
                    assert_eq!(
                        targets.x,
                        if side == SessionsSide::Left {
                            sessions.right()
                        } else {
                            0
                        }
                    );
                } else {
                    assert_eq!(targets.width, width);
                    assert_eq!(targets.y, sessions.bottom());
                }
            }
        }
    }
}
