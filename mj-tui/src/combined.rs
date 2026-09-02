//! The combined conversation surface.
//!
//! One screen holds all of Hel's terminal UI: a Sessions pane, the transcript
//! of the conversation on screen, the Prompt composer, and Targets and Quota
//! summaries under it, with a shared one-row footer. There is no second screen
//! to switch to, so nothing is ever hidden behind a navigation step.

use hel::hel_chat::{ActiveChat, ChatRegions};
use hel::hel_selection::{SurfaceFrame, SurfaceId};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};

use crate::render::{
    MINIMUM_TERMINAL_WIDTH, TerminalSizeRequirement, minimized_quota_line, minimized_targets_line,
    render_capacity, render_footer, render_modal, render_onboarding_surface, render_quotas,
    render_sessions, render_terminal_too_small, sessions_content_height,
};
use crate::resume::resume_sessions_pane;
use crate::widgets::bordered_content;
use crate::{DashboardState, Focus, Mode};

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
/// taller five-row form; below it the grid falls back to two rows.
const TALL_TERMINAL_HEIGHT: u16 = 40;

/// How many content rows the minimized Sessions grid draws: five when the
/// terminal is tall enough to spare them, two on a tiny terminal (where the
/// grid also sheds its border, so two rows is all it needs).
pub(crate) fn minimized_grid_rows(frame_height: u16) -> u16 {
    if frame_height >= TALL_TERMINAL_HEIGHT {
        5
    } else {
        2
    }
}

/// Whether the minimized Sessions grid keeps its title and border. A tiny
/// terminal drops them to spend every row on sessions.
pub(crate) fn minimized_grid_bordered(frame_height: u16) -> bool {
    frame_height >= TALL_TERMINAL_HEIGHT
}

/// Whether a tiny terminal drops the Targets and Quota panes entirely. It does
/// so only once they are already collapsed (modes 2 and 3); mode 1 keeps its
/// tables and simply reports the terminal is too small if they will not fit.
fn omits_support_panes(minimized: bool, frame_height: u16) -> bool {
    minimized && frame_height < TALL_TERMINAL_HEIGHT
}

/// The height the composer settles at: its desired height, but never below
/// [`PROMPT_MINIMUM`] and never above a third of the frame. Mirrors the growth
/// target in [`allocate_combined_heights`] so mode 2 can predict the room the
/// prompt leaves for the Sessions/conversation split.
fn prompt_target(desired_prompt: u16, frame_height: u16) -> u16 {
    let ceiling = (frame_height / 3).max(PROMPT_MINIMUM);
    desired_prompt.clamp(PROMPT_MINIMUM, ceiling)
}

/// Mode 2 gives Sessions a fixed third of the room it shares with the
/// conversation — the frame less the composer, the footer, and whatever the
/// support panes still occupy (`support_rows`: two collapsed rows normally,
/// zero on a tiny terminal that drops them). The conversation takes the other
/// two thirds as the residual transcript band, so the 1:2 split holds across
/// Tab and any session count.
fn support_collapsed_sessions_height(
    frame_height: u16,
    prompt_height: u16,
    support_rows: u16,
) -> u16 {
    let shared = frame_height
        .saturating_sub(prompt_height)
        .saturating_sub(support_rows)
        .saturating_sub(FOOTER_HEIGHT);
    (shared / 3).max(PANE_MINIMUM)
}

/// How tall one band wants to be and how short it may get.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PaneBand {
    minimum: u16,
    full: u16,
    cap: u16,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CombinedAllocation {
    Fits(CombinedHeights),
    TooSmall { required_frame_height: u16 },
}

/// Divides the frame between the six bands.
///
/// Every band starts at its minimum, and the surplus is then spent in a fixed
/// order: the composer first, because it is where the user is typing; then
/// whichever pane has the keyboard; then Sessions, Targets and Quota; and the
/// transcript takes whatever is left. The caps stop any one pane from eating
/// a small screen — a project with thirty live sessions must not push the
/// conversation off it.
fn allocate_combined_heights(
    frame_height: u16,
    sessions: PaneBand,
    targets: PaneBand,
    quota: PaneBand,
    desired_prompt: u16,
    focus: Focus,
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
        desired_prompt.min((frame_height / 3).max(PROMPT_MINIMUM)),
        &mut surplus,
    );
    match focus {
        Focus::Sessions => grow(
            &mut heights.sessions,
            sessions.full.min(sessions.cap),
            &mut surplus,
        ),
        Focus::Targets => grow(
            &mut heights.targets,
            targets.full.min(targets.cap),
            &mut surplus,
        ),
        Focus::Quota => grow(&mut heights.quota, quota.full.min(quota.cap), &mut surplus),
        Focus::Prompt => {}
    }
    grow(
        &mut heights.sessions,
        sessions.full.min(sessions.cap),
        &mut surplus,
    );
    grow(
        &mut heights.targets,
        targets.full.min(targets.cap),
        &mut surplus,
    );
    grow(&mut heights.quota, quota.full.min(quota.cap), &mut surplus);
    heights.transcript = heights.transcript.saturating_add(surplus);
    CombinedAllocation::Fits(heights)
}

fn support_band(full: u16, focused: bool, frame_height: u16, minimized: bool) -> PaneBand {
    if minimized {
        return PaneBand {
            minimum: SUMMARY_ROW,
            full: SUMMARY_ROW,
            cap: SUMMARY_ROW,
        };
    }
    let divisor = if focused { 3 } else { 4 };
    PaneBand {
        minimum: PANE_MINIMUM,
        full,
        cap: (frame_height / divisor).max(PANE_MINIMUM),
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
    chat: Option<&mut ActiveChat>,
    transcript_selected: bool,
) {
    dashboard.pane_areas = None;
    dashboard.session_row_areas.clear();
    dashboard.project_heading_areas.clear();
    dashboard.frame_surfaces.clear();
    dashboard.chat_transcript_area = None;
    dashboard.chat_prompt_area = None;
    let area = frame.area();
    dashboard.frame_size = Some((area.width, area.height));
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

    let layout = dashboard.pane_layout();
    let minimized = layout.support_collapsed();
    let omit_support = omits_support_panes(minimized, area.height);
    let focus = dashboard.focus();
    // With no conversation the prompt band holds the two-line guidance that
    // stands in for a composer, so it asks for the rows to show both. This is
    // computed before the Sessions band so mode 2 can carve the leftover room.
    let desired_prompt = chat.as_ref().map_or(EMPTY_PROMPT_HEIGHT, |chat| {
        chat.desired_prompt_height(area.width)
    });
    let sessions_compact = dashboard.sessions_compact();
    let sessions = if sessions_compact {
        // Minimized: a fixed-height grid — five content rows in a tall enough
        // terminal plus its border, or two borderless rows on a tiny one. It
        // neither grows nor shrinks, so minimum, full and cap are the same.
        let border = if minimized_grid_bordered(area.height) {
            2
        } else {
            0
        };
        let height = minimized_grid_rows(area.height) + border;
        PaneBand {
            minimum: height,
            full: height,
            cap: height,
        }
    } else if minimized {
        // Mode 2 (support panes collapsed): Sessions takes a fixed third of
        // the room it shares with the conversation, so the split stays 1:2
        // whatever has the keyboard and however many sessions are live. The
        // conversation takes the other two thirds as the residual band.
        let support_rows = if omit_support { 0 } else { SUMMARY_ROW * 2 };
        let height = support_collapsed_sessions_height(
            area.height,
            prompt_target(desired_prompt, area.height),
            support_rows,
        );
        PaneBand {
            minimum: height,
            full: height,
            cap: height,
        }
    } else {
        // Mode 1: content-sized, and free to reach half the height when it has
        // the keyboard so a focused list has room to work in.
        let sessions_cap = if focus == Focus::Sessions {
            area.height / 2
        } else {
            area.height / 3
        };
        PaneBand {
            minimum: PANE_MINIMUM,
            full: sessions_content_height(dashboard, area.width).saturating_add(2),
            cap: sessions_cap.max(PANE_MINIMUM),
        }
    };
    // A tiny terminal drops Targets and Quota entirely once they are collapsed,
    // so they take no rows and are not drawn.
    let no_band = PaneBand {
        minimum: 0,
        full: 0,
        cap: 0,
    };
    let (targets, quota) = if omit_support {
        (no_band, no_band)
    } else {
        (
            support_band(
                table_height(dashboard.capacity_details.len()),
                focus == Focus::Targets,
                area.height,
                minimized,
            ),
            support_band(
                table_height(dashboard.config.profiles.len()),
                focus == Focus::Quota,
                area.height,
                minimized,
            ),
        )
    };
    let allocation =
        allocate_combined_heights(area.height, sessions, targets, quota, desired_prompt, focus);
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

    let rendered = render_sessions(frame, sessions_area, dashboard);
    dashboard.session_row_areas = rendered.session_row_areas;
    dashboard.project_heading_areas = rendered.project_heading_areas;
    let sessions_content = if sessions_compact && !minimized_grid_bordered(area.height) {
        // The tiny grid deliberately has no border, so all two rows are
        // content. Applying the bordered inset here would collapse it to a
        // zero-height selection surface.
        sessions_area
    } else {
        bordered_content(sessions_area)
    };
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
            // A modal inside the conversation owns the frame's interaction, so
            // the panes behind it stop being selectable rather than staying
            // reachable underneath.
            if chat.frame_surfaces_exclusive() {
                dashboard.frame_surfaces.replace_with(chat.frame_surfaces());
            } else {
                dashboard.frame_surfaces.append(chat.frame_surfaces());
            }
            prompt_focused
        }
        None => {
            render_empty_conversation(
                frame,
                transcript_area,
                prompt_area,
                prompt_focused,
                dashboard.ordered_sessions().is_empty(),
            );
            false
        }
    };

    // Targets and Quota keep their pane numbers whether they are full tables
    // or one-row summaries, so a click resolves to the same pane either way.
    if omit_support {
        // A tiny terminal drops both panes; there is nothing to draw and no
        // hitbox to register.
    } else if minimized {
        // Drawn as the pane's own title so a collapsed pane keeps the rule the
        // full one has, rather than becoming a loose line of text.
        frame.render_widget(
            Block::default()
                .borders(Borders::TOP)
                .title(minimized_targets_line(dashboard, targets_area.width)),
            targets_area,
        );
        frame.render_widget(
            Block::default()
                .borders(Borders::TOP)
                .title(minimized_quota_line(dashboard, quota_area.width)),
            quota_area,
        );
        dashboard.frame_surfaces.push(SurfaceFrame::fixed(
            SurfaceId::DashboardPane(1),
            targets_area,
        ));
        dashboard
            .frame_surfaces
            .push(SurfaceFrame::fixed(SurfaceId::DashboardPane(2), quota_area));
    } else {
        render_capacity(frame, targets_area, dashboard);
        render_quotas(frame, quota_area, dashboard);
        dashboard.frame_surfaces.push(SurfaceFrame::fixed(
            SurfaceId::DashboardPane(1),
            bordered_content(targets_area),
        ));
        dashboard.frame_surfaces.push(SurfaceFrame::fixed(
            SurfaceId::DashboardPane(2),
            bordered_content(quota_area),
        ));
    }

    if !chat_drew_footer {
        render_footer(frame, footer_area, dashboard);
    }
    render_modal(frame, area, dashboard);
}

/// The bordered chrome that stands in for a conversation when none is open,
/// and the thing the user can do about it.
///
/// There are two reasons for an empty band, and they need different advice:
/// a workspace with no live session needs one created or resumed, while a
/// workspace that has live sessions just needs one opened. Telling the second
/// user there is no live session would be a plain lie — the pane above is
/// listing them.
fn render_empty_conversation(
    frame: &mut Frame,
    transcript_area: Rect,
    prompt_area: Rect,
    prompt_focused: bool,
    no_live_session: bool,
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
    let (title, lines) = if no_live_session {
        (
            " Prompt (no live session) ",
            [
                "No live session in this workspace.",
                "Press Alt-N to create one, or Alt-S to resume one.",
            ],
        )
    } else {
        (
            " Prompt (no conversation open) ",
            [
                "No conversation open.",
                "Press Tab for Sessions, then Enter on the one to open.",
            ],
        )
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
    fn minimized_grid_takes_five_bordered_rows_when_tall_and_two_bare_rows_when_tiny() {
        assert_eq!(minimized_grid_rows(40), 5);
        assert_eq!(minimized_grid_rows(100), 5);
        assert!(minimized_grid_bordered(40));
        assert_eq!(minimized_grid_rows(39), 2);
        assert_eq!(minimized_grid_rows(10), 2);
        assert!(!minimized_grid_bordered(39));
    }

    #[test]
    fn support_panes_drop_only_once_collapsed_and_only_on_a_tiny_terminal() {
        assert!(omits_support_panes(true, 39));
        assert!(!omits_support_panes(true, 40));
        // Mode 1 keeps its tables however short the terminal.
        assert!(!omits_support_panes(false, 10));
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
    fn support_collapsed_sessions_take_a_third_of_the_shared_room() {
        // 60 tall, composer 3, two collapsed rows: shared = 60 - 3 - 2 - 1 =
        // 54, a third is 18.
        assert_eq!(support_collapsed_sessions_height(60, 3, 2), 18);
        // With the support panes dropped, the two rows come back to the split.
        assert_eq!(support_collapsed_sessions_height(60, 3, 0), 18);
        // Never below the pane minimum, even when there is almost no room.
        assert_eq!(support_collapsed_sessions_height(10, 6, 0), PANE_MINIMUM);
    }

    /// Mode 2 holds the Sessions/conversation split at 1:2 whether Sessions or
    /// the composer has the keyboard, so Tab never resizes either pane.
    #[test]
    fn mode_two_split_stays_put_across_tab() {
        let frame = 60;
        let prompt = prompt_target(3, frame);
        let height = support_collapsed_sessions_height(frame, prompt, SUMMARY_ROW * 2);
        let sessions = PaneBand {
            minimum: height,
            full: height,
            cap: height,
        };
        let collapsed = PaneBand {
            minimum: SUMMARY_ROW,
            full: SUMMARY_ROW,
            cap: SUMMARY_ROW,
        };
        let heights_for = |focus| match allocate_combined_heights(
            frame, sessions, collapsed, collapsed, 3, focus,
        ) {
            CombinedAllocation::Fits(heights) => heights,
            CombinedAllocation::TooSmall { .. } => panic!("60 rows should fit"),
        };

        let focused = heights_for(Focus::Sessions);
        let unfocused = heights_for(Focus::Prompt);
        assert_eq!(
            focused.sessions, unfocused.sessions,
            "Sessions must not resize on Tab"
        );
        assert_eq!(
            focused.transcript, unfocused.transcript,
            "the conversation must not resize on Tab"
        );
        // A third for Sessions, two thirds for the conversation.
        assert_eq!(focused.sessions, height);
        assert_eq!(focused.transcript, 2 * height);
    }
}
