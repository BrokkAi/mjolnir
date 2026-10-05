use std::collections::BTreeMap;

use crossterm::event::{KeyCode, MouseButton, MouseEventKind};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::Color;

use mj_core::config::{Config, HarnessKind, TargetTemplate};
use mj_core::state::{
    MaterializedExecutionState, STATE_VERSION, SessionState, State, TranscriptBody,
};

use mj_chat::selection::SurfaceId;
use mj_client::quota::{API_LABEL, ProfileQuota, QuotaWindow};
use mj_core::targets::{DeploymentCapacityUsage, ProvisionStage};

use super::*;
use crate::test_support::*;

use crate::ingest::SessionDetail;
use crate::{DashboardAction, DashboardState, Focus, SessionOperationKind};

fn session_metadata_text(
    session: &SessionRecord,
    detail: Option<&SessionDetail>,
    operation: Option<&SessionOperationDisplay>,
    now_epoch_seconds: u64,
    config: &Config,
) -> String {
    session_activity_line(
        "",
        session,
        detail,
        None,
        false,
        false,
        crate::AttentionLevel::Idle,
        operation,
        now_epoch_seconds,
        &session_target_label(&State::default(), session, operation, config),
        session_permission_badge(session, operation, config),
        None,
        120,
        false,
    )
    .to_string()
}

fn minimize_all_panes(dashboard: &mut DashboardState) {
    for pane in [
        SupportPane::Sessions,
        SupportPane::Targets,
        SupportPane::Quota,
    ] {
        dashboard.set_pane_size(pane, PaneSize::Minimized);
    }
}

fn append_golden_state(
    output: &mut String,
    label: &str,
    width: u16,
    height: u16,
    lines: &[String],
) {
    output.push_str(&format!("=== {label} ({width}x{height}) ===\n"));
    output.push_str(&lines.join("\n"));
    output.push('\n');
}

fn append_golden_value(output: &mut String, label: &str, value: impl std::fmt::Debug) {
    output.push_str(&format!("{label}: {value:?}\n"));
}

fn append_dashboard_golden(
    output: &mut String,
    label: &str,
    width: u16,
    height: u16,
    dashboard: &mut DashboardState,
) {
    let lines = drawn(dashboard, width, height);
    append_golden_state(output, label, width, height, &lines);
}

fn clear_session_activity(dashboard: &mut DashboardState, session_id: &str) {
    let detail = dashboard
        .session_details
        .get_mut(session_id)
        .expect("the session detail is present");
    detail.current_turn_started_at = None;
    detail.activity = Default::default();
}

#[test]
fn pending_questions_mark_the_session_and_minimized_navigator() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard
        .state
        .sessions
        .get_mut("session-1")
        .unwrap()
        .session_title_override = Some("長いセッション名のテスト".repeat(4));
    let mut session = materialized_session_for("session-1", Vec::new());
    session.pending_elicitations = vec![
        mj_core::elicitation::ElicitationRequest::from_acp_params(
            "request-1",
            serde_json::json!({
                "mode": "form",
                "sessionId": "session-1",
                "message": "Choose a path",
                "requestedSchema": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"}
                    }
                }
            }),
        )
        .expect("valid test question"),
    ];
    dashboard.apply_materialized_session(&session);
    let mut foreign = running_session();
    foreign.id = "foreign-session".into();
    foreign.workspace_id = "other-workspace".into();
    let pending_elicitations = dashboard.session_details["session-1"]
        .pending_elicitations
        .clone();
    dashboard.session_details.insert(
        foreign.id.clone(),
        SessionDetail {
            pending_elicitations,
            ..SessionDetail::default()
        },
    );
    dashboard.state.sessions.insert(foreign.id.clone(), foreign);
    assert_eq!(
        dashboard.sessions_attention_summary(),
        Some((crate::AttentionLevel::Waiting, 1))
    );

    let expanded = drawn(&mut dashboard, 120, 30).join("\n");
    assert!(expanded.contains("Question"), "{expanded}");

    minimize_all_panes(&mut dashboard);
    let minimized = drawn(&mut dashboard, 120, 40).join("\n");
    assert!(minimized.contains("Question"), "{minimized}");
    assert!(
        minimized.contains("Sessions [!1]") || minimized.contains("!1"),
        "{minimized}"
    );
}

#[test]
fn drawing_the_dashboard_registers_each_pane_interior_for_selection() {
    let mut dashboard = dashboard_with_session(running_session());
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("terminal");

    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw dashboard");

    let panes = dashboard.pane_areas.expect("dashboard pane hitboxes");
    let surfaces = dashboard.frame_surfaces();
    for (index, pane) in panes.iter().enumerate() {
        let id = SurfaceId::DashboardPane(index as u8);
        let surface = surfaces
            .surface(id)
            .unwrap_or_else(|| panic!("pane {index} registered"));
        assert_eq!(surface.rect, crate::widgets::bordered_content(*pane));
        assert_eq!(
            surfaces
                .surface_at(surface.rect.x, surface.rect.y)
                .map(|surface| surface.id),
            Some(id)
        );
    }
    // The border rows and the scrollbar column stay out of every surface,
    // so a selection can never pick up their glyphs.
    assert!(surfaces.surface_at(panes[0].x, panes[0].y).is_none());
    assert!(
        surfaces
            .surface_at(panes[0].right() - 1, panes[0].y + 1)
            .is_none()
    );
}

#[test]
fn an_open_dialog_registers_its_body_and_list_above_the_panes() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.show_resume_dialog(1, Vec::new());
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("terminal");

    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw resume dialog");

    let surfaces = dashboard.frame_surfaces();
    let body = surfaces.surface(SurfaceId::ModalBody).expect("dialog body");
    let list = surfaces
        .surface(SurfaceId::ResumeList)
        .expect("session list");
    // The dialog covers the panes, and its list covers the dialog.
    assert_eq!(
        surfaces
            .surface_at(body.rect.x, body.rect.y)
            .map(|surface| surface.id),
        Some(SurfaceId::ModalBody)
    );
    assert_eq!(
        surfaces
            .surface_at(list.rect.x, list.rect.y)
            .map(|surface| surface.id),
        Some(SurfaceId::ResumeList)
    );
    // Away from the popup the panes underneath still own their cells:
    // the dialog covers them, it does not clear them.
    let sessions = surfaces
        .surface(SurfaceId::DashboardPane(0))
        .expect("sessions pane");
    assert_eq!(
        surfaces
            .surface_at(sessions.rect.x, sessions.rect.y)
            .map(|surface| surface.id),
        Some(SurfaceId::DashboardPane(0))
    );
}

#[test]
fn unanswered_user_line_stays_bright_and_shows_the_latest_agent_activity() {
    let mut dashboard = dashboard_with_session(running_session());
    let mut transcript = numbered_conversation(1);
    transcript.push(transcript_item(
        3,
        TranscriptBody::User {
            content: vec![serde_json::json!({
                "type": "text",
                "text": "unanswered follow-up"
            })],
        },
    ));
    transcript.push(thought(4, "Checking the workspace"));
    apply_materialized_transcript(&mut dashboard, transcript);
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("terminal");

    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw dashboard");
    let buffer = terminal.backend().buffer();
    let lines = buffer_lines(buffer);
    let rendered = lines.join("\n");

    assert!(rendered.contains("Checking the workspace"));
    assert!(!rendered.contains("You: unanswered follow-up"));
    assert!(!rendered.contains("answer 0"));
    let (user_row, user_line) = lines
        .iter()
        .enumerate()
        .find(|(_, line)| line.contains("Checking the workspace"))
        .expect("current activity line");
    let user_column = cell_column(user_line, "Checking the workspace");
    assert_ne!(
        buffer[(buffer.area.x + user_column, buffer.area.y + user_row as u16)].fg,
        theme::palette().muted
    );
}

/// RCL-2 (2026-09-29): a minimized Sessions pane swaps its title for the
/// attention count when the title is narrow. A filter in force must still show
/// on it: the label gives way first, and the clear chip stays.
// Hard-won: daa8f51d: With the Sessions pane minimized, the compact attention title dropped the active filter and its clear chip.
#[test]
fn a_narrow_sessions_title_with_attention_keeps_the_filter_clear_chip() {
    use crate::AttentionLevel;
    use mj_core::config::SymbolSet;

    for (symbols, close) in [(SymbolSet::Unicode, '×'), (SymbolSet::Ascii, 'x')] {
        theme::with_symbols(symbols, || {
            let badge = Some((AttentionLevel::Waiting, 1));
            let mut saw_label = false;
            for width in 14..=60u16 {
                let title = sessions_title_with_attention("/needle", width, badge, true, true);
                let text = title.line.to_string();
                let budget = usize::from(pane_title_content_width(width, true));
                if text.chars().count() > budget {
                    // Only the base title may overflow, and it is the one that
                    // truncates its own label; it still ends with the chip.
                    assert!(text.contains(close), "{symbols:?} {width}: {text:?}");
                    continue;
                }
                // The full form puts the attention suffix after the chip.
                assert!(
                    text.contains(&format!(" {close} ")),
                    "{symbols:?} {width}: {text:?}"
                );
                let chip = usize::from(title.clear_chip.expect("chip has room"));
                assert!(
                    text.chars()
                        .skip(chip)
                        .collect::<String>()
                        .starts_with(&format!(" {close} ")),
                    "{symbols:?} {width}: {text:?}"
                );
                saw_label |= text.contains("/n");
            }
            assert!(saw_label, "{symbols:?}: some width keeps part of the label");
            let none = sessions_title_with_attention("", 20, badge, true, false);
            assert!(!none.line.to_string().contains(close), "no filter, no chip");
        });
    }
}

/// The drawn Sessions pane shows the chip on its title row exactly while a
/// filter is in force, with the state label and, given the room, the hidden
/// count before it.
#[test]
fn pane_size_controls_are_styled_registered_and_clickable_without_moving_focus() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.workspace_name = "workspace".into();
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).expect("terminal");
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw controls");

    let expected_control_count = [
        SupportPane::Sessions,
        SupportPane::Targets,
        SupportPane::Quota,
    ]
    .into_iter()
    .map(|pane| {
        if dashboard.pane_maximize_enabled(pane) {
            3
        } else {
            2
        }
    })
    .sum::<usize>();
    assert_eq!(
        dashboard.pane_size_control_areas.len(),
        expected_control_count
    );
    let buffer = terminal.backend().buffer();
    for pane in [
        SupportPane::Sessions,
        SupportPane::Targets,
        SupportPane::Quota,
    ] {
        let maximize_enabled = dashboard.pane_maximize_enabled(pane);
        let expected_sizes = [PaneSize::Minimized, PaneSize::Standard, PaneSize::Maximized]
            .into_iter()
            .filter(|size| *size != PaneSize::Maximized || maximize_enabled)
            .collect::<Vec<_>>();
        let controls = dashboard
            .pane_size_control_areas
            .iter()
            .copied()
            .filter(|(candidate, _, _)| *candidate == pane)
            .collect::<Vec<_>>();
        assert_eq!(controls.len(), expected_sizes.len());
        for ((_, size, area), expected_size) in controls.into_iter().zip(expected_sizes) {
            assert_eq!(size, expected_size);
            let cell = &buffer[(area.x + 1, area.y)];
            let glyph = match size {
                PaneSize::Minimized => "▁",
                PaneSize::Standard => "▪",
                PaneSize::Maximized => "□",
            };
            assert_eq!(cell.symbol(), glyph);
            if size == PaneSize::Standard {
                assert_eq!(Some(cell.bg), theme::active_control().bg);
                assert_eq!(cell.fg, theme::palette().accent);
                assert!(cell.modifier.contains(Modifier::BOLD));
            } else {
                assert_eq!(cell.bg, theme::palette().surface);
                assert_eq!(cell.fg, theme::palette().text);
            }
        }
    }

    dashboard.set_pane_size(SupportPane::Targets, PaneSize::Minimized);
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw minimized controls");
    let buffer = terminal.backend().buffer();
    let target_controls = dashboard
        .pane_size_control_areas
        .iter()
        .copied()
        .filter(|(pane, _, _)| *pane == SupportPane::Targets)
        .collect::<Vec<_>>();
    assert_eq!(target_controls.len(), 2);
    for ((_, size, area), (glyph, expected_size)) in target_controls
        .into_iter()
        .zip([("▁", PaneSize::Minimized), ("▪", PaneSize::Standard)])
    {
        assert_eq!(size, expected_size);
        assert_eq!(buffer[(area.x + 1, area.y)].symbol(), glyph);
    }
    let targets_area = dashboard.pane_areas.expect("pane areas")[1];
    assert_eq!(
        buffer[(targets_area.right() - 1, targets_area.y)].symbol(),
        "─"
    );

    dashboard.focus = Focus::Targets;
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw focused minimized controls");
    let focused_targets = buffer_lines(terminal.backend().buffer())
        .into_iter()
        .find(|line| line.contains("Targets"))
        .expect("focused Targets row");
    assert!(focused_targets.contains("─ Targets ──"));
    assert!(focused_targets.ends_with('─'));
    assert!(!focused_targets.contains('═'));
    dashboard.focus = Focus::Sessions;
    assert!(
        dashboard
            .pane_size_control_areas
            .iter()
            .all(|(pane, size, _)| *pane != SupportPane::Targets || *size != PaneSize::Maximized)
    );
    assert_eq!(dashboard.focus(), Focus::Sessions);
}

/// The band a row draws, derived exactly as the renderer derives it: the
/// shared attention ladder first, then the live-session colours.
fn band(detail: Option<&SessionDetail>, unreachable: bool, state: SessionState) -> Color {
    let level =
        crate::dashboard_sessions::attention_level(detail, None, state, unreachable, false, false);
    session_band_color(level, detail, state)
}

#[test]
fn summary_band_colors_prioritize_attention_activity_and_lifecycle() {
    let normal = SessionDetail {
        current_turn_started_at: Some(1),
        ..SessionDetail::default()
    };
    assert_eq!(
        band(Some(&normal), false, SessionState::Running),
        theme::palette().session_activity
    );

    let unread = SessionDetail {
        current_turn_started_at: Some(1),
        unread_agent_messages: 1,
        ..SessionDetail::default()
    };
    assert_eq!(
        band(Some(&unread), false, SessionState::Running),
        theme::palette().session_activity,
        "a running turn is activity, whatever is still unread"
    );

    let unread_idle = SessionDetail {
        unread_agent_messages: 1,
        ..SessionDetail::default()
    };
    assert_eq!(
        band(Some(&unread_idle), false, SessionState::Running),
        theme::palette().session_attention,
        "a finished turn nobody has read wants a person"
    );

    let read_idle = SessionDetail::default();
    assert_eq!(
        band(Some(&read_idle), false, SessionState::Running),
        theme::palette().session_idle
    );

    let foreground = SessionDetail {
        activity: mj_client::usage_format::SessionActivity {
            pursuing_goal: Default::default(),
            foreground_tool_started_at_ms: Some(1),
            ..mj_client::usage_format::SessionActivity::default()
        },
        ..SessionDetail::default()
    };
    assert_eq!(
        band(Some(&foreground), false, SessionState::Running),
        theme::palette().session_activity,
        "foreground work is not idle"
    );

    let unread_background = SessionDetail {
        unread_agent_messages: 1,
        activity: mj_client::usage_format::SessionActivity {
            pursuing_goal: Default::default(),
            background_commands: vec![mj_core::relay::BackgroundCommand {
                id: "test-background".into(),
                started_at_ms: 1,
                command: "cargo test".into(),
                can_stop: false,
            }],
            ..mj_client::usage_format::SessionActivity::default()
        },
        ..SessionDetail::default()
    };
    assert_eq!(
        band(Some(&unread_background), false, SessionState::Running),
        theme::palette().session_activity,
        "background work is still work, not something waiting on a person"
    );
    let read_background = SessionDetail {
        activity: unread_background.activity.clone(),
        ..SessionDetail::default()
    };
    assert_eq!(
        band(Some(&read_background), false, SessionState::Running),
        theme::palette().session_activity,
        "background work is not idle after it has been read"
    );

    let interrupted_idle = SessionDetail {
        unread_interruptions: 1,
        ..SessionDetail::default()
    };
    assert_eq!(
        band(Some(&interrupted_idle), false, SessionState::Running),
        theme::palette().session_attention,
        "an unread interruption needs attention"
    );

    let interrupted_running = SessionDetail {
        current_turn_started_at: Some(1),
        unread_interruptions: 1,
        ..SessionDetail::default()
    };
    assert_eq!(
        band(Some(&interrupted_running), false, SessionState::Running),
        theme::palette().session_activity,
        "a running turn is activity, whatever is still unread"
    );

    let needs_input = SessionDetail {
        pending_elicitations: vec![
            mj_core::elicitation::ElicitationRequest::from_acp_params(
                "request-1",
                serde_json::json!({
                    "mode": "form",
                    "sessionId": "session-1",
                    "message": "Choose a path",
                    "requestedSchema": {
                        "type": "object",
                        "properties": {"path": {"type": "string"}}
                    }
                }),
            )
            .expect("valid test question"),
        ],
        ..SessionDetail::default()
    };
    assert_eq!(
        band(Some(&needs_input), false, SessionState::Running),
        theme::palette().session_attention,
        "pending input overrides the idle blue"
    );

    assert_eq!(
        band(Some(&read_idle), false, SessionState::Provisioning),
        theme::palette().session_activity,
        "provisioning is a lifecycle state, not a live idle session"
    );
    assert_eq!(
        band(Some(&read_idle), false, SessionState::Error),
        theme::palette().session_error,
        "error overrides idle"
    );
    assert_eq!(
        band(None, false, SessionState::Running),
        theme::palette().session_activity,
        "unknown detail stays at the default"
    );

    assert_eq!(
        band(Some(&unread), true, SessionState::Running),
        theme::palette().session_error,
        "an unreachable worker is red, overriding every other state"
    );
    assert_eq!(
        band(None, true, SessionState::Running),
        theme::palette().session_error
    );
}

#[test]
fn current_agent_excerpt_never_repeats_an_old_answer() {
    let waiting = SessionDetail {
        last_agent_message: Some("previous answer".into()),
        last_user_message: Some("new request".into()),
        ..SessionDetail::default()
    };
    assert_eq!(current_agent_excerpt(&waiting), None);

    let thinking = SessionDetail {
        last_agent_message: Some("previous answer".into()),
        last_user_message: Some("new request".into()),
        latest_agent_activity_after_last_user: Some("checking files".into()),
        ..SessionDetail::default()
    };
    assert_eq!(current_agent_excerpt(&thinking), Some("checking files"));

    let replying = SessionDetail {
        last_agent_message: Some("current answer".into()),
        last_user_message: Some("new request".into()),
        last_agent_message_follows_last_user: true,
        latest_agent_activity_after_last_user: Some("older thought".into()),
        ..SessionDetail::default()
    };
    assert_eq!(current_agent_excerpt(&replying), Some("current answer"));

    let old_only = SessionDetail {
        last_agent_message: Some("old answer".into()),
        ..SessionDetail::default()
    };
    assert_eq!(current_agent_excerpt(&old_only), None);
}

/// A long reply is rendered for its Sessions row once, not on every frame.
/// Later frames reuse its rows until the reply or the room for it changes,
/// and the rows are the reply's first two rows as a fresh rendering gives.
// Hard-won: 36763162: Long replies were wrapped and rendered repeatedly on every frame, slowing dashboard input as sessions accumulated.
#[test]
fn a_long_reply_is_rendered_for_its_session_row_once_across_frames() {
    let mut dashboard = dashboard_with_session(running_session());
    let id = dashboard.state.sessions.keys().next().unwrap().clone();
    let reply = |tag: &str| {
        (0..80)
            .map(|index| {
                format!(
                    "### {tag} finding {index}\n\n- the daemon publishes a revision and \
                     every waiter wakes to reload `file_{index}.rs`\n\n"
                )
            })
            .collect::<String>()
    };
    let set_reply = |dashboard: &mut DashboardState, text: &str| {
        let detail = dashboard.session_details.entry(id.clone()).or_default();
        detail.last_agent_message = Some(text.into());
        detail.last_agent_message_follows_last_user = true;
    };
    let renders =
        |dashboard: &DashboardState| dashboard.session_details[&id].output_preview.renders.get();
    // The rows a fresh rendering of `text` gives for the frame just drawn.
    let expected_rows = |dashboard: &DashboardState, text: &str| {
        let content_width = dashboard.pane_areas.expect("a drawn frame")[0].width - 2;
        let output_width = usize::from(content_width) - "  │".chars().count();
        mj_chat::chat::render_agent_message_head(&text.replace('\n', " "), output_width, 2)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
                    // A full row's ellipsis can fall past the pane's edge.
                    .trim_end_matches('…')
                    .to_owned()
            })
            .collect::<Vec<_>>()
    };

    let first = reply("first");
    set_reply(&mut dashboard, &first);
    let screen = drawn(&mut dashboard, 140, 40);
    let rows = expected_rows(&dashboard, &first);
    assert_eq!(rows.len(), 2);
    for row in &rows {
        assert!(
            screen.iter().any(|line| line.contains(row.as_str())),
            "{row:?} is on screen: {screen:#?}"
        );
    }
    for _ in 0..3 {
        assert_eq!(drawn(&mut dashboard, 140, 40), screen);
    }
    assert_eq!(renders(&dashboard), 1, "later frames reuse the rows");

    let second = reply("second");
    set_reply(&mut dashboard, &second);
    let screen = drawn(&mut dashboard, 140, 40).join("\n");
    assert_eq!(renders(&dashboard), 2, "a new reply is rendered again");
    for row in expected_rows(&dashboard, &second) {
        assert!(screen.contains(&row), "{row:?} is on screen: {screen}");
    }

    let screen = drawn(&mut dashboard, 200, 40).join("\n");
    assert_eq!(renders(&dashboard), 3, "a new width is rendered again");
    for row in expected_rows(&dashboard, &second) {
        assert!(screen.contains(&row), "{row:?} is on screen: {screen}");
    }
}

// Hard-won: 86ab9abd: A pending Muse question row showed the request_user_input tool name instead of the question text.
#[test]
fn a_pending_question_is_the_excerpt_instead_of_the_tool_name() {
    let asking = SessionDetail {
        latest_agent_activity_after_last_user: Some("request_user_input".into()),
        pending_elicitations: vec![crate::test_support::question("session-1")],
        ..SessionDetail::default()
    };
    assert_eq!(current_agent_excerpt(&asking), Some("Choose a path"));
}

/// Help is the one hint that is worth more than any other, so it survives
/// every focus and every width squeeze.
/// A hint cut in half names a key that does not exist. Narrow terminals
/// therefore lose whole hints from the right, and never part of one.
/// A half-typed chord owns the footer: the reader needs the way out of it and
/// the key that lists the rest, not the hints they are part-way through.
/// Every hint in the footer, whichever separator it sits between.
fn footer_hints(footer: &str) -> Vec<String> {
    footer
        .split(theme::footer_group_separator())
        .flat_map(|group| group.split(theme::footer_separator()))
        .filter(|hint| !hint.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

/// The prefix label rides on the first chord hint, which is also the first
/// chord a narrow row gives up. The label must outlive it: a row reading
/// `: palette · ? keys` would say `:` alone opens the palette.
// Hard-won: 22d10cb4: A narrow footer dropped the prefix label with the first chord and made the remaining shortcut look like a plain key.
#[test]
fn the_prefix_is_still_named_when_only_protected_chords_survive() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.focus_sessions();
    let footer = combined_footer_text(&dashboard, 60);
    assert_eq!(
        footer, "Enter open │ ctrl+b then : palette · ? keys",
        "{footer}"
    );
    assert!(
        !combined_footer_text(&dashboard, 200).contains("then : palette"),
        "a wide row keeps the label on the first chord"
    );
}

/// A-11: Enter on the Profiles pane opens the profile-ID rename, so the hint
/// says rename rather than promising a profile editor.
// Hard-won: 4592b7de: The Quota footer said Enter edits a profile although the action only renames its configuration ID.
#[test]
fn the_quota_footer_says_enter_renames_the_profile() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.focus = Focus::Quota;
    let footer = combined_footer_text(&dashboard, 200);
    assert!(footer.starts_with("Enter rename profile"), "{footer}");
    assert!(!footer.contains("edit profile"), "{footer}");
}

/// A-12: at every ordinary width and for every pane focus, the chord group
/// leads with the prefix, and the palette's `:` never follows a colon.
// Hard-won: 48c2a9ca: The composer footer dropped its chord-prefix label at narrower widths and presented prefix chords as plain keys.
#[test]
fn every_pane_footer_names_the_prefix_at_140_100_and_80_columns() {
    for focus in [
        Focus::Workspaces,
        Focus::Sessions,
        Focus::Quota,
        Focus::Targets,
    ] {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus = focus;
        for width in [140_u16, 100, 80] {
            let footer = combined_footer_text(&dashboard, width);
            let chord_group = footer
                .split(theme::footer_group_separator())
                .nth(1)
                .unwrap_or_else(|| panic!("{focus:?} {width}: {footer}"));
            assert!(
                chord_group.starts_with("ctrl+b then "),
                "{focus:?} {width}: {footer}"
            );
            assert!(
                footer.ends_with(": palette · ? keys"),
                "{focus:?} {width}: {footer}"
            );
            assert!(!footer.contains("then: :"), "{focus:?} {width}: {footer}");
        }
    }
}

/// The prefix chords give way before the pane's own hints, and help and palette
/// remain visible after every other hint has been dropped. The pane group is
/// the short list of keys that work right here; the chord group is the long one
/// the palette holds in full, so the long one is what a narrow row gives up.
// Hard-won: 36fbf1f1: A 140-column footer kept chord hints while dropping the focused pane hints that teach its active controls.
#[test]
fn footer_drops_chord_hints_before_pane_hints_and_keeps_help_longest() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.set_deployment_capacity_targets(vec![test_capacity_target()]);
    dashboard.focus_sessions();

    let full = combined_footer_text(&dashboard, 200);
    assert!(
        footer_hints(&full).contains(&"Enter open".to_owned()),
        "{full}"
    );

    // Narrow enough to lose every chord, still wide enough for everything the
    // focused pane answers.
    let squeezed = combined_footer_text(&dashboard, 90);
    assert_eq!(
        squeezed,
        "Enter open · / search (filter a/b/w/i/d) · Tab pane │ ctrl+b then : palette · ? keys"
    );

    // Narrower still, the pane hints give way from the right as well, and the
    // prefix label stays on the first chord left standing.
    assert_eq!(
        combined_footer_text(&dashboard, 52),
        "Enter open │ ctrl+b then : palette · ? keys"
    );

    assert_eq!(combined_footer_text(&dashboard, 20), ": palette · ? keys");
    assert_eq!(combined_footer_text(&dashboard, 6), "? keys");
    assert!(combined_footer_text(&dashboard, 5).is_empty());
    assert!(combined_footer_text(&dashboard, 0).is_empty());
}

/// The footer is generated from the same table the keyboard reads, so
/// pressing what it names must do what it says. This is the test that
/// makes the registry worth having.
// Hard-won: 7bd8ab16: Hand-written key, footer, and help mappings drifted, so the displayed command could disagree with dispatched behavior.
#[test]
fn every_footer_hint_dispatches_the_command_it_names() {
    for focus in [Focus::Sessions, Focus::Targets, Focus::Quota] {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.set_deployment_capacity_targets(vec![test_capacity_target()]);
        dashboard.begin_session_operation_at(
            "session-1".into(),
            SessionOperationKind::Launching,
            None,
            1_000,
        );
        dashboard.focus = focus;

        for id in crate::actions::available(&dashboard, None) {
            let spec = crate::actions::spec(id);
            let Some(word) = (spec.footer)(&dashboard) else {
                continue;
            };
            let Some(key_label) = dashboard.footer_key(id) else {
                continue;
            };
            let footer = combined_footer_text(&dashboard, 400);
            assert!(
                footer.contains(&format!("{key_label} {word}")),
                "{focus:?}: {footer} omits {:?}",
                spec.label
            );

            // Pressing the advertised key and dispatching the command it
            // names have to leave the surface in the same place.
            let mut pressed = dashboard_with_session(running_session());
            pressed.set_deployment_capacity_targets(vec![test_capacity_target()]);
            pressed.begin_session_operation_at(
                "session-1".into(),
                SessionOperationKind::Launching,
                None,
                1_000,
            );
            pressed.focus = focus;
            let mut dispatched = dashboard_with_session(running_session());
            dispatched.set_deployment_capacity_targets(vec![test_capacity_target()]);
            dispatched.begin_session_operation_at(
                "session-1".into(),
                SessionOperationKind::Launching,
                None,
                1_000,
            );
            dispatched.focus = focus;

            let keys = match spec.footer_group {
                crate::actions::FooterGroup::Chord => chord_keys(&pressed, id),
                crate::actions::FooterGroup::Pane => {
                    vec![key(spec
                        .pane_keys
                        .first()
                        .expect("a pane hint has a key")
                        .code)]
                }
            };
            let by_key = route(&mut pressed, &keys);
            let by_dispatch = dispatched.dispatch_command(id);
            assert_eq!(by_key, by_dispatch, "{focus:?}: {:?}", spec.label);
            assert_eq!(
                std::mem::discriminant(&pressed.mode),
                std::mem::discriminant(&dispatched.mode),
                "{focus:?}: {:?}",
                spec.label
            );
            assert_eq!(
                pressed.focus, dispatched.focus,
                "{focus:?}: {:?}",
                spec.label
            );
        }
    }
}

/// The empty band has two causes and they need different advice. Telling
/// someone there is no live session while the pane above lists one is a
/// plain lie.
// Hard-won: 465c6478: A live session existed but was not open, yet the empty prompt falsely said there was no live session.
#[test]
fn the_empty_prompt_distinguishes_no_session_from_no_conversation() {
    let mut empty = DashboardState::new(config(), State::default(), BTreeMap::new());
    let lines = drawn(&mut empty, 120, 44).join("\n");
    assert!(lines.contains("No live session"), "{lines}");
    assert!(
        lines.contains("ctrl+b c to create a session or ctrl+b g to find one"),
        "{lines}"
    );

    // A live session that simply is not open says so instead.
    let mut live = dashboard_with_session(running_session());
    let lines = drawn(&mut live, 120, 44).join("\n");
    assert!(lines.contains("No conversation open"), "{lines}");
    assert!(lines.contains("Enter on the one to open"), "{lines}");
    assert!(!lines.contains("No live session"), "{lines}");
    assert!(!lines.contains("Opening session"), "{lines}");
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn host_usage(cpu_percent: u8) -> mj_core::targets::DeploymentCapacityUsage {
    mj_core::targets::DeploymentCapacityUsage {
        cpu_percent: Some(cpu_percent),
        memory_used_bytes: 1,
        memory_total_bytes: 4,
        logical_cores: 8,
        disk_total_bytes: Some(64),
        storage: Vec::new(),
    }
}

/// A profile quota with `remaining` percent of its weekly window left.
fn weekly_quota(profile_id: &str, remaining: u8) -> ProfileQuota {
    ProfileQuota {
        banked_resets: None,
        profile_id: profile_id.into(),
        harness: HarnessKind::Claude,
        windows: vec![QuotaWindow {
            label: "weekly".into(),
            remaining_percent: Some(remaining),
            used: None,
            limit: None,
            resets: None,
            resets_at_epoch_seconds: None,
        }],
        extra: None,
        error: None,
        refreshed_at_epoch_seconds: now_seconds(),
        rate_limited_until_epoch_seconds: None,
    }
}

/// A profile quota reporting both windows, the way the subscription
/// harnesses do.
fn weekly_and_five_hour_quota(profile_id: &str, weekly: u8, five_hour: u8) -> ProfileQuota {
    let mut quota = weekly_quota(profile_id, weekly);
    quota.windows.push(QuotaWindow {
        label: "5h".into(),
        remaining_percent: Some(five_hour),
        used: None,
        limit: None,
        resets: None,
        resets_at_epoch_seconds: None,
    });
    quota
}

/// What a usage-priced harness reports: no window at all, and the API
/// label in place of one.
fn api_quota(profile_id: &str) -> ProfileQuota {
    ProfileQuota {
        banked_resets: None,
        profile_id: profile_id.into(),
        harness: HarnessKind::Codex,
        windows: Vec::new(),
        extra: Some(API_LABEL.into()),
        error: None,
        refreshed_at_epoch_seconds: now_seconds(),
        rate_limited_until_epoch_seconds: None,
    }
}

/// An agent that is idle but left a command running says so, in the wide
/// rows and in the minimized grid, from the one fact the daemon forwards.
// Hard-won: db2e99a5: Sessions that left a command running in the background were shown as idle across Mjolnir surfaces.
#[test]
fn background_work_reaches_both_session_row_forms() {
    let started_at_ms = i64::try_from(mj_core::clock::epoch_seconds()).unwrap() * 1_000 - 2_616_000;
    let activity = mj_client::usage_format::SessionActivity {
        pursuing_goal: Default::default(),
        checking_response: false,
        quota_recovery: None,
        capacity_retry: None,
        activity_turn_started_at_ms: None,
        prompt_in_flight: false,
        idle_since_ms: None,
        execution: None,
        harness_turn_started_at_ms: None,
        state: None,
        foreground_tool_started_at_ms: None,
        background_commands: vec![mj_core::relay::BackgroundCommand {
            id: "test-background".into(),
            started_at_ms,
            command: "cargo test".into(),
            can_stop: false,
        }],
        active_user_shells: Vec::new(),
    };

    let mut dashboard = dashboard_with_session(running_session());
    dashboard.focus_sessions();
    assert!(
        drawn(&mut dashboard, 120, 44)
            .iter()
            .any(|line| line.contains("No messages yet")),
        "an idle session with no current output keeps the output block stable"
    );

    dashboard.set_session_activity("session-1", activity.clone());
    dashboard
        .session_details
        .get_mut("session-1")
        .expect("session detail")
        .current_turn_started_at = None;
    let expanded = drawn(&mut dashboard, 120, 44);
    assert!(
        expanded
            .iter()
            .any(|line| line.contains("1 task") && line.contains("43m3")),
        "the expanded row: {expanded:?}"
    );

    let mut minimized = dashboard_with_session(running_session());
    minimized.set_session_activity("session-1", activity);
    minimize_all_panes(&mut minimized);
    let summary = drawn(&mut minimized, 120, 44).join("\n");
    assert!(summary.contains("1 task"), "{summary}");
    assert!(!summary.contains("You:"), "{summary}");
    assert!(!summary.contains("Agent:"), "{summary}");
}

/// Every expanded session is the same height, so the layout can be
/// computed from a count and rows never jitter as messages arrive. A
/// session with nothing to show still draws its two agent rows.
/// `projects` projects, `per_project` live sessions in each, laid out so
/// the minimized list has headings and enough sessions to scroll. Project
/// directories are zero-padded so they sort in the obvious order.
fn minimized_sessions_dashboard(projects: usize, per_project: usize) -> DashboardState {
    let mut sessions = mj_core::snapshot_map::SnapshotMap::new();
    let mut index = 0;
    for project in 0..projects {
        for _ in 0..per_project {
            let mut session = running_session();
            session.id = format!("session-{index:02}");
            session.created_at = format!("2026-08-{:02}T00:00:00Z", index + 1);
            session.project_directory = Some(format!("/projects/proj{project:02}").into());
            sessions.insert(session.id.clone(), session);
            index += 1;
        }
    }
    let mut dashboard = DashboardState::new(
        config(),
        State {
            last_subagent_policy: Default::default(),
            subagents: Default::default(),
            version: STATE_VERSION,
            sessions,
            mount_history: Default::default(),
            container_sizes: Default::default(),
        },
        BTreeMap::new(),
    );
    // One turn of the dial reaches the minimized list.
    minimize_all_panes(&mut dashboard);
    dashboard
}

/// The content rows of the Sessions pane (inside its border) for a
/// minimized list at the given terminal size.
fn minimized_content_rows(dashboard: &mut DashboardState, width: u16, height: u16) -> Vec<String> {
    let lines = drawn(dashboard, width, height);
    let pane = dashboard.pane_areas.unwrap()[0];
    lines[usize::from(pane.y + 1)..usize::from(pane.bottom() - 1)]
        .iter()
        .map(|line| {
            line.chars()
                .skip(usize::from(pane.x) + 1)
                .take(usize::from(pane.width) - 2)
                .collect()
        })
        .collect()
}

/// Minimized Sessions keeps the familiar vertical list but drops every
/// transcript preview beneath the session's summary line.
#[test]
fn a_starting_session_keeps_one_preview_across_incomplete_state_publications() {
    let mut session = running_session();
    session.state = SessionState::Provisioning;
    session.session_title_override = Some("Startup".into());
    let id = session.id.clone();
    let mut dashboard = dashboard_with_session(session);
    dashboard.config.advanced.session_order = mj_core::config::SessionOrder::Priority;
    dashboard.select_active_session(&id);
    dashboard.begin_session_operation(id.clone(), SessionOperationKind::Launching, None);
    let pane = dashboard.browse_pane();
    dashboard.set_pane_session(pane, Some(&id));
    for omit in [false, true] {
        if omit {
            let mut publication = dashboard.state.clone();
            publication.sessions.remove(&id);
            dashboard.set_state(publication);
        }
        let rendered = drawn(&mut dashboard, 140, 40).join("\n");
        assert!(rendered.contains("Startup"), "{rendered}");
        assert_eq!(dashboard.selected_session_id(), Some(id.as_str()));
        assert_eq!(dashboard.pane_session(pane), Some(id.as_str()));
        let rows = drawn_session_rows(&dashboard, 60);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].content_height(),
            1,
            "startup remains a single preview line"
        );
    }

    let mut ready = dashboard.state.sessions[&id].clone();
    ready.state = SessionState::Running;
    ready.session_title_override = Some("Ready session".into());
    let mut publication = dashboard.state.clone();
    publication.sessions.insert(id.clone(), ready);
    dashboard.set_state(publication);
    assert_eq!(drawn_session_rows(&dashboard, 60)[0].content_height(), 1);
    dashboard.finish_session_operation(&id);
    assert!(drawn_session_rows(&dashboard, 60)[0].content_height() > 1);
    let rendered = drawn(&mut dashboard, 140, 40).join("\n");
    assert!(rendered.contains("Ready session"), "{rendered}");
    assert_eq!(dashboard.selected_session_id(), Some(id.as_str()));
}

#[test]
fn finishing_a_launch_removes_a_preview_absent_from_authoritative_state() {
    let session = running_session();
    let id = session.id.clone();
    let mut dashboard = dashboard_with_session(session);
    dashboard.begin_session_operation(id.clone(), SessionOperationKind::Launching, None);
    let mut publication = dashboard.state.clone();
    publication.sessions.remove(&id);
    dashboard.set_state(publication);
    assert!(dashboard.state.sessions.contains_key(&id));
    dashboard.finish_session_operation(&id);
    assert!(!dashboard.state.sessions.contains_key(&id));
    assert!(dashboard.ordered_sessions().is_empty());
}

/// A session row is coloured by the same state rule the expanded rows
/// use: idle is blue, active work yellow, and a failed session red.
#[test]
fn the_minimized_list_colours_a_session_row_by_state() {
    let colour_of = |mut dashboard: DashboardState| {
        let mut terminal = Terminal::new(TestBackend::new(120, 44)).expect("terminal");
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw the minimized list");
        let buffer = terminal.backend().buffer();
        let lines = buffer_lines(buffer);
        let row = lines
            .iter()
            .position(|line| line.contains("ACP pretty"))
            .expect("a session row");
        buffer[(cell_column(&lines[row], "ACP pretty"), row as u16)].fg
    };

    let healthy = minimized_sessions_dashboard(1, 1);
    assert_eq!(colour_of(healthy), theme::palette().session_idle);

    let mut busy = minimized_sessions_dashboard(1, 1);
    busy.session_details
        .get_mut("session-00")
        .expect("the session detail")
        .current_turn_started_at = Some(1);
    assert_eq!(colour_of(busy), theme::palette().session_activity);

    let mut failed = minimized_sessions_dashboard(1, 1);
    {
        let session = failed
            .state
            .sessions
            .get_mut("session-00")
            .expect("the session");
        session.state = SessionState::Error;
    }
    assert_eq!(colour_of(failed), theme::palette().session_error);
}

/// The minimized list is a viewport: selecting a session past the visible
/// rows scrolls the window so it shows, and earlier rows leave view.
#[test]
fn the_minimized_list_scrolls_to_keep_the_selection_visible() {
    let mut dashboard = minimized_sessions_dashboard(12, 1);

    // Selecting the first session keeps the window at the start.
    dashboard.select_active_session("session-00");
    let rows = minimized_content_rows(&mut dashboard, 120, 20);
    assert!(
        rows.iter().any(|line| line.contains("proj00")),
        "first project visible: {rows:?}"
    );
    assert!(
        !rows.iter().any(|line| line.contains("proj11")),
        "last project not yet visible: {rows:?}"
    );

    // Selecting the last session scrolls it into view and the first out.
    dashboard.select_active_session("session-11");
    let rows = minimized_content_rows(&mut dashboard, 120, 20);
    assert!(
        rows.iter().any(|line| line.contains("proj11")),
        "last project scrolled into view: {rows:?}"
    );
    assert!(
        !rows.iter().any(|line| line.contains("proj00")),
        "first project scrolled out: {rows:?}"
    );
}

#[test]
fn the_minimized_list_follows_the_terminal_height_each_frame() {
    let mut dashboard = minimized_sessions_dashboard(3, 2);

    let tall = drawn(&mut dashboard, 120, 44);
    let tall_pane = dashboard.pane_areas.expect("tall panes")[0];
    assert!(
        tall[usize::from(tall_pane.y)].contains('╭')
            && tall[usize::from(tall_pane.y)].contains("Sessi")
    );
    assert_eq!(dashboard.pane_areas.expect("tall panes")[0].height, 40);

    let short = drawn(&mut dashboard, 120, 20);
    let short_pane = dashboard.pane_areas.expect("short panes")[0];
    assert!(
        short[usize::from(short_pane.y)].contains('╭')
            && short[usize::from(short_pane.y)].contains("Sessi")
    );
    assert_eq!(dashboard.pane_areas.expect("short panes")[0].height, 16);

    drawn(&mut dashboard, 120, 44);
    assert_eq!(
        dashboard.pane_areas.expect("tall panes again")[0].height,
        40
    );
}

// Hard-won: 0cb06579: Independent daemon and dashboard quota pollers could disagree; the pane now follows the daemon report and replaces stale snapshots.
#[test]
fn the_profiles_pane_shows_the_daemons_report_and_probing_set_and_nothing_older() {
    let mut dashboard = dashboard_with_session(running_session());
    minimize_all_panes(&mut dashboard);
    let profiles_row = |dashboard: &mut DashboardState| {
        drawn(dashboard, 120, 44)
            .into_iter()
            .find(|line| line.contains("─ Profiles ──"))
            .expect("the minimized Profiles row")
    };

    dashboard.set_quota_snapshot(mj_client::quota::QuotaSnapshot {
        reports: BTreeMap::from([
            ("claude-1".to_owned(), weekly_quota("claude-1", 63)),
            ("codex-1".to_owned(), weekly_quota("codex-1", 10)),
        ]),
        probing: std::collections::BTreeSet::new(),
        cycles: 1,
    });
    let row = profiles_row(&mut dashboard);
    assert!(row.contains("claude-1 63%, codex-1 10%"), "{row:?}");

    // The daemon is asking about one profile: only that row says so.
    dashboard.set_quota_snapshot(mj_client::quota::QuotaSnapshot {
        reports: BTreeMap::from([
            ("claude-1".to_owned(), weekly_quota("claude-1", 63)),
            ("codex-1".to_owned(), weekly_quota("codex-1", 10)),
        ]),
        probing: ["codex-1".to_owned()].into(),
        cycles: 1,
    });
    let row = profiles_row(&mut dashboard);
    assert!(row.contains("claude-1 63%"), "{row:?}");
    assert!(row.contains("codex-1 refreshing"), "{row:?}");

    // The snapshot replaces what the dashboard held; it does not add to it.
    dashboard.set_quota_snapshot(mj_client::quota::QuotaSnapshot {
        reports: BTreeMap::from([("codex-1".to_owned(), weekly_quota("codex-1", 12))]),
        ..Default::default()
    });
    let row = profiles_row(&mut dashboard);
    assert!(!row.contains("63%"), "{row:?}");
    assert!(row.contains("codex-1 12%"), "{row:?}");
}

#[test]
fn a_rate_limited_profile_reads_the_retry_time_not_unavailable() {
    let mut held = weekly_quota("claude-1", 63);
    held.rate_limited_until_epoch_seconds = Some(now_seconds() + 5 * 60);
    // No good reading behind the hold: the report itself is the rate limit.
    let mut only_limited = weekly_quota("codex-1", 0);
    only_limited.windows.clear();
    only_limited.error = Some("rate limited".into());
    only_limited.rate_limited_until_epoch_seconds = Some(now_seconds() + 5 * 60);

    let mut dashboard = dashboard_with_session(running_session());
    dashboard.set_quota_snapshot(mj_client::quota::QuotaSnapshot {
        reports: BTreeMap::from([
            ("claude-1".to_owned(), held),
            ("codex-1".to_owned(), only_limited),
        ]),
        ..Default::default()
    });
    let lines = drawn(&mut dashboard, 120, 44);
    for id in ["claude-1", "codex-1"] {
        let row = lines
            .iter()
            .find(|line| line.contains(id) && line.contains("rate limited"))
            .unwrap_or_else(|| panic!("a rate limited row for {id}: {lines:#?}"));
        assert!(row.contains("rate limited · retry in"), "{row:?}");
        assert!(!row.contains("unavailable"), "{row:?}");
    }

    minimize_all_panes(&mut dashboard);
    let row = drawn(&mut dashboard, 120, 44)
        .into_iter()
        .find(|line| line.contains("─ Profiles ──"))
        .expect("the minimized Profiles row");
    assert!(row.contains("claude-1 rate limited"), "{row:?}");
    assert!(row.contains("codex-1 rate limited"), "{row:?}");
}

/// An exhausted profile reads 0%, the same as the open pane's bar. Showing
/// how much has been *used* would read 100% there, which looks like a
/// profile in the best possible shape rather than one with nothing left.
#[test]
fn an_exhausted_quota_reads_zero_in_the_minimized_row() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.apply_quota(weekly_quota("claude-1", 0));
    minimize_all_panes(&mut dashboard);

    let quota = drawn(&mut dashboard, 120, 44)
        .into_iter()
        .find(|line| line.contains("─ Profiles ──"))
        .expect("the minimized Profiles row");
    assert!(quota.contains("claude-1 0%"), "{quota:?}");
    assert!(!quota.contains("claude-1 100%"), "{quota:?}");
}

/// A reading that cannot be trusted has to say so. A number that is
/// actually missing, stale or inapplicable is worse than no number.
#[test]
fn the_minimized_rows_stay_explicit_about_readings_they_do_not_have() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.set_deployment_capacity_targets(vec![test_capacity_target()]);
    minimize_all_panes(&mut dashboard);

    // No sample at all.
    let lines = drawn(&mut dashboard, 120, 44);
    let targets = |lines: &[String]| {
        lines
            .iter()
            .find(|line| line.contains("─ Targets ──"))
            .expect("the minimized Targets row")
            .clone()
    };
    assert!(targets(&lines).contains("local unavailable"));

    // A probe in flight.
    dashboard.begin_capacity_refresh();
    assert!(targets(&drawn(&mut dashboard, 120, 44)).contains("local refreshing…"));

    // A sample too old to trust.
    dashboard.apply_deployment_capacity(
        "local",
        Ok(Some(host_usage(7))),
        now_seconds() - CAPACITY_SAMPLE_STALE_AFTER_SECONDS - 60,
    );
    assert!(
        targets(&drawn(&mut dashboard, 120, 44)).contains("local 7% (stale)"),
        "{:?}",
        targets(&drawn(&mut dashboard, 120, 44))
    );

    // A quota that failed to refresh.
    let quota = drawn(&mut dashboard, 120, 44)
        .into_iter()
        .find(|line| line.contains("─ Profiles ──"))
        .expect("the minimized Profiles row");
    assert!(quota.contains("claude-1 unavailable"), "{quota:?}");
}

/// A failed session used to render identically to a healthy one, so the
/// only thing that told you it had failed was pressing Enter on it. Red
/// is the same signal an unreachable relay carries: this row needs
/// attention rather than reading.
#[test]
fn a_failed_session_draws_a_red_summary_at_both_pane_sizes() {
    for size in [PaneSize::Standard, PaneSize::Minimized] {
        let mut healthy = dashboard_with_session(running_session());
        healthy.set_pane_size(SupportPane::Sessions, size);
        let mut failed = {
            let mut session = running_session();
            session.state = SessionState::Error;
            session.last_error = Some("worker bootstrap failed".into());
            dashboard_with_session(session)
        };
        failed.set_pane_size(SupportPane::Sessions, size);

        let row_colour = |dashboard: &mut DashboardState| {
            let mut terminal = Terminal::new(TestBackend::new(120, 44)).expect("terminal");
            terminal
                .draw(|frame| render(frame, dashboard))
                .expect("draw the session list");
            let buffer = terminal.backend().buffer();
            let lines = buffer_lines(buffer);
            let status = if dashboard
                .state
                .sessions
                .values()
                .any(|session| session.state == SessionState::Error)
            {
                "Error"
            } else {
                "Idle"
            };
            let row = lines
                .iter()
                .position(|line| line.contains(status))
                .unwrap_or_else(|| panic!("the session's row ({status}): {lines:?}"));
            let column = cell_column(&lines[row], status);
            buffer[(column, row as u16)].fg
        };

        assert_eq!(row_colour(&mut failed), theme::palette().error, "{size:?}");
        assert_ne!(
            row_colour(&mut healthy),
            theme::palette().error,
            "{size:?}: only a session that needs attention is red"
        );
    }
}

/// A week with headroom left is no comfort while the next five hours are
/// spent, so a profile that has dipped into its week reports both figures.
/// An untouched week says everything there is to say on its own, and a
/// profile with no five-hour window has nothing more to add.
#[test]
fn read_idle_session_distinguishes_status_from_identity_in_expanded_and_collapsed_rows() {
    let mut session = stopped_session();
    session.state = SessionState::Running;
    // The detach cursor sits past the only agent message, so nothing is
    // unread; a truly idle live session is still blue.
    session.viewed_through_event_ordinal = 1;
    for collapsed in [false, true] {
        let mut dashboard = dashboard_with_session(session.clone());
        dashboard.focus = Focus::Quota;
        let mut materialized =
            materialized_session_for("session-1", vec![agent_message(1, "seen response")]);
        materialized.execution = MaterializedExecutionState::Idle;
        dashboard.apply_materialized_session(&materialized);
        if collapsed {
            dashboard.focus_sessions();
            dashboard.handle_key(crate::test_support::key(KeyCode::Char('1')));
            dashboard.focus = Focus::Quota;
        }

        let mut terminal = Terminal::new(TestBackend::new(140, 28)).expect("terminal");
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw dashboard");
        let buffer = terminal.backend().buffer();
        let status_y = (buffer.area.y..buffer.area.bottom())
            .find(|y| {
                let row = (buffer.area.x..buffer.area.right())
                    .map(|x| buffer[(x, *y)].symbol())
                    .collect::<String>();
                row.contains("podman")
            })
            .expect("the session's summary row");
        let status = (buffer.area.x..buffer.area.right())
            .map(|x| buffer[(x, status_y)].symbol())
            .collect::<String>();
        assert!(!status.contains("unread"));
        let pane = dashboard.pane_areas.expect("pane areas")[0];
        let identity_x = cell_column(&status, "podman");
        assert!(
            (pane.x + 1..identity_x)
                .filter(|x| summary_text_cell(&buffer[(*x, status_y)]))
                .all(|x| buffer[(x, status_y)].fg == theme::palette().session_idle),
            "{collapsed}: {status}"
        );
        assert_eq!(buffer[(identity_x, status_y)].fg, theme::palette().muted);
    }
}

/// A session the harness has not named and nobody renamed is listed by the
/// title it was created with ("project via fake"), not its id, which the row
/// would otherwise repeat (launch finding R2-8).
// Hard-won: e7eb288e: An unnamed dashboard-created session fell back to its hexadecimal ID instead of the created project/profile title.
#[test]
fn session_name_prefers_override_then_acp_title_then_created_title() {
    let mut session = stopped_session();
    assert_eq!(session_name(&session), "ACP pretty name");

    session.acp_session_title = None;
    assert_eq!(session_name(&session), session.title);

    session.session_title_override = Some("My name".into());
    assert_eq!(session_name(&session), "My name");

    session.session_title_override = None;
    session.title = " ".into();
    assert_eq!(session_name(&session), "session-1");
}

/// The standard local container targets exist whether or not an engine is
/// installed. The Targets pane checks each one in the background, the same
/// check the new-session wizard uses, and marks one whose engine is missing
/// rather than listing it as if it could run.
// Hard-won: 0b408d01: Launch finding J-7 showed the Targets pane omitted whether a listed local container engine was unavailable.
#[test]
fn targets_pane_marks_a_local_container_target_whose_engine_is_missing() {
    let mut dashboard = DashboardState::new(config(), State::default(), BTreeMap::new());
    dashboard.set_deployment_capacity_targets(vec![test_capacity_target()]);
    let Some(crate::DashboardAction::CheckTargetReadiness {
        generation,
        target_ids,
    }) = dashboard.take_target_availability_check()
    else {
        panic!("the Targets pane must check its local container targets");
    };
    assert_eq!(target_ids, vec!["podman".to_owned()]);
    assert!(
        dashboard.take_target_availability_check().is_none(),
        "a check in flight is not repeated"
    );
    let render_text = |dashboard: &mut DashboardState| {
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).expect("terminal");
        terminal
            .draw(|frame| render(frame, dashboard))
            .expect("draw dashboard");
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
    };
    assert!(!render_text(&mut dashboard).contains("podman (unavailable)"));

    dashboard.apply_target_readiness(
        generation,
        "podman".into(),
        Err("Podman preflight failed: podman: not found".into()),
    );

    let rendered = render_text(&mut dashboard);
    assert!(rendered.contains("podman (unavailable)"), "{rendered}");
}

/// A default candidate whose engine is missing (the user has no Docker and
/// wrote no `docker` target) is not listed in the host row. A target the
/// user wrote whose engine is missing stays, marked unavailable.
/// The config the dashboard installs comes off the runtime feed, which does
/// not serialize `Config::default_targets`. The origin travels beside the
/// config, so a host without Docker lists no `docker` the user never wrote,
/// while a `[targets.docker]` the user wrote stays, marked unavailable.
// Hard-won: 9a81ac2b: The runtime-fed dashboard could not distinguish a synthesized Docker candidate and showed it as unavailable on hosts without Docker.
#[test]
fn feed_installed_config_hides_default_docker_but_keeps_a_user_written_one() {
    let docker_missing_row = |user_wrote_docker: bool| {
        let mut daemon_config = Config::default();
        if user_wrote_docker {
            let mut docker = Config::default().with_local_targets().targets["docker"].clone();
            if let TargetTemplate::LocalDocker { container } = &mut docker {
                container.image = "example.test/own:latest".into();
            }
            daemon_config.targets.insert("docker".into(), docker);
        }
        let daemon_config = daemon_config.with_local_targets();
        let sent = mj_client::runtime_feed::RuntimeMetadata {
            default_targets: daemon_config.default_targets.clone(),
            config: daemon_config,
            ..Default::default()
        };
        let received: mj_client::runtime_feed::RuntimeMetadata =
            serde_json::from_str(&serde_json::to_string(&sent).unwrap()).unwrap();
        let mut dashboard = DashboardState::new(
            received.installed_config(),
            State::default(),
            BTreeMap::new(),
        );
        let mut target = test_capacity_target();
        target.target_ids = vec!["docker".into(), "localhost".into()];
        dashboard.set_deployment_capacity_targets(vec![target]);
        let Some(crate::DashboardAction::CheckTargetReadiness {
            generation,
            target_ids,
        }) = dashboard.take_target_availability_check()
        else {
            panic!("the Targets pane must check its local container targets");
        };
        assert!(target_ids.contains(&"docker".to_owned()));
        dashboard.apply_target_runtime_missing(
            generation,
            "docker".into(),
            "docker: not found".into(),
        );
        drawn_dashboard(&mut dashboard, 160)
    };

    let default_only = docker_missing_row(false);
    assert!(!default_only.contains("docker"), "{default_only}");
    let user_written = docker_missing_row(true);
    assert!(
        user_written.contains("docker (unavailable)"),
        "{user_written}"
    );
}

/// precision-3260: an unreachable session on a full disk read as plain
/// "Unreachable". Its row now says "Disk full", and the Targets row shows the
/// host as full, both from the daemon's one storage verdict.
// Hard-won: 540c9202: A real full-disk incident made every worker appear merely unreachable and caused recovery to retry writes on the same full filesystem.
#[test]
fn an_unreachable_session_on_a_full_disk_says_disk_full() {
    let session = precision_session();
    let mut dashboard = dashboard_with_session(session.clone());
    dashboard.set_session_connectivity(&session.id, false);
    dashboard.set_deployment_capacity_targets(vec![mj_core::targets::DeploymentCapacityTarget {
        id: "ssh:precision-3260".into(),
        host: "precision-3260".into(),
        target_ids: vec!["precision".into()],
        kind: mj_core::targets::DeploymentCapacityKind::Host,
        local: false,
        probes: Vec::new(),
        probe_error: None,
    }]);
    let draw = |dashboard: &mut DashboardState| {
        let mut terminal = Terminal::new(TestBackend::new(160, 40)).expect("terminal");
        terminal
            .draw(|frame| render(frame, dashboard))
            .expect("draw dashboard");
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
    };
    let rendered = draw(&mut dashboard);
    assert!(rendered.contains("Unreachable"), "{rendered}");
    assert!(!rendered.contains("Disk full"));

    // A full filesystem the session does not write to changes nothing.
    let mut elsewhere = precision_storage(40 << 30, 40 << 30);
    elsewhere.filesystems[1].space.paths = vec!["/srv".into()];
    elsewhere.filesystems[1].condition = mj_core::targets::storage::StorageCondition::Full;
    dashboard.set_target_storage(vec![elsewhere]);
    assert!(!draw(&mut dashboard).contains("Disk full"));

    // Its managed clone's filesystem is full: the row and the Targets pane
    // say so.
    dashboard.set_target_storage(vec![precision_storage(40 << 30, 0)]);
    let rendered = draw(&mut dashboard);
    assert!(rendered.contains("Disk full"), "{rendered}");
    assert!(
        rendered.contains("/home/jonathan/Projects 0B free full"),
        "the Targets row: {rendered}"
    );
}

/// A capacity sample the poller keeps refreshing carries no clock column
/// and no staleness marker: the number on screen is the current one.
/// `symbols = "ascii"` must reach the Targets pane's own summary text: the
/// "% CPU · % RAM" join was a literal Unicode dot, so it survived the ASCII
/// set while every other glyph in the row correctly swapped. The symbol set
/// is read from the dashboard's own configuration, the way a running session
/// selects it, rather than through the thread-local override the render
/// pipeline itself already scopes to that configuration.
#[test]
fn ascii_symbols_reach_the_capacity_panes_cpu_and_ram_join() {
    let mut ascii_config = config();
    ascii_config.advanced.symbols = Some(mj_core::config::SymbolSet::Ascii);
    let mut dashboard = DashboardState::new(ascii_config, State::default(), BTreeMap::new());
    dashboard.set_deployment_capacity_targets(vec![test_capacity_target()]);
    dashboard.apply_deployment_capacity(
        "local",
        Ok(Some(host_capacity_usage())),
        now_epoch_seconds(),
    );
    let rendered = drawn_dashboard(&mut dashboard, 200);
    let cpu_ram_line = rendered
        .lines()
        .find(|line| line.contains("% CPU"))
        .expect("capacity row");
    assert!(cpu_ram_line.is_ascii(), "{cpu_ram_line:?}");
    assert!(
        cpu_ram_line.contains("37% CPU - 75% RAM"),
        "{cpu_ram_line:?}"
    );
}

fn now_epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn host_capacity_usage() -> DeploymentCapacityUsage {
    DeploymentCapacityUsage {
        cpu_percent: Some(37),
        memory_used_bytes: 3,
        memory_total_bytes: 4,
        logical_cores: 8,
        disk_total_bytes: None,
        storage: Vec::new(),
    }
}

#[test]
fn capacity_pane_never_presents_missing_readings_as_zero_resource_use() {
    let mut dashboard = DashboardState::new(config(), State::default(), BTreeMap::new());
    dashboard.set_deployment_capacity_targets(vec![test_capacity_target()]);
    dashboard.apply_deployment_capacity(
        "local",
        Ok(Some(DeploymentCapacityUsage {
            cpu_percent: None,
            memory_used_bytes: 0,
            memory_total_bytes: 0,
            logical_cores: 8,
            disk_total_bytes: None,
            storage: Vec::new(),
        })),
        now_epoch_seconds(),
    );

    let rendered = drawn_dashboard(&mut dashboard, 200);
    assert!(
        rendered.contains("CPU unavailable · RAM unavailable"),
        "{rendered}"
    );
    assert!(!rendered.contains("0% CPU"), "{rendered}");
    assert!(!rendered.contains("0% RAM"), "{rendered}");
}

fn drawn_dashboard(dashboard: &mut DashboardState, width: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, 40)).expect("terminal");
    terminal
        .draw(|frame| render(frame, dashboard))
        .expect("draw dashboard");
    buffer_lines(terminal.backend().buffer()).join("\n")
}

/// A probe that failed and a reading that stopped refreshing both keep the
/// last numbers on screen and say why they cannot be trusted, instead of
/// rendering exactly like a reading taken a moment ago.
#[test]
fn capacity_rows_mark_a_failed_probe_and_a_sample_that_stopped_refreshing() {
    let mut failed = DashboardState::new(config(), State::default(), BTreeMap::new());
    failed.set_deployment_capacity_targets(vec![test_capacity_target()]);
    failed.apply_deployment_capacity(
        "local",
        Ok(Some(host_capacity_usage())),
        now_epoch_seconds(),
    );
    failed.apply_deployment_capacity("local", Err("probe timed out".into()), now_epoch_seconds());
    let rendered = drawn_dashboard(&mut failed, 200);
    assert!(rendered.contains("37% CPU · 75% RAM"), "{rendered}");
    assert!(rendered.contains("stale: probe timed out"), "{rendered}");

    let mut aged = DashboardState::new(config(), State::default(), BTreeMap::new());
    aged.set_deployment_capacity_targets(vec![test_capacity_target()]);
    aged.apply_deployment_capacity(
        "local",
        Ok(Some(host_capacity_usage())),
        now_epoch_seconds().saturating_sub(3_600),
    );
    let rendered = drawn_dashboard(&mut aged, 200);
    assert!(rendered.contains("stale: sampled 1h ago"), "{rendered}");

    let mut never_sampled = DashboardState::new(config(), State::default(), BTreeMap::new());
    never_sampled.set_deployment_capacity_targets(vec![test_capacity_target()]);
    never_sampled.apply_deployment_capacity(
        "local",
        Err("probe timed out".into()),
        now_epoch_seconds(),
    );
    let rendered = drawn_dashboard(&mut never_sampled, 200);
    assert!(rendered.contains("unavailable"), "{rendered}");
    assert!(!rendered.contains("stale"), "{rendered}");
}

// Hard-won: 6dec78fe: An in-flight Resuming row kept showing its source profile and target after the operation had recorded its destination.
#[test]
fn resuming_row_shows_the_destination_profile_and_target_not_the_stale_record() {
    // The controller updates the session's own last_profile/target as
    // soon as a resume starts, but the dashboard's local session
    // snapshot only refreshes once the operation finishes. The in-flight
    // row must show where the resume is going, not where it came from.
    let session = stopped_session();
    assert_eq!(session.last_profile, "codex-1");
    assert_eq!(session.target_template_id, "podman");
    let mut resuming = operation(SessionOperationKind::Resuming, None);
    resuming.resume_destination = Some(("grok-1".into(), "localhost".into()));

    let text = session_metadata_text(&session, None, Some(&resuming), 1_012, &config());

    assert!(text.contains("grok-1"), "{text}");
    assert!(text.contains("localhost"), "{text}");
}

#[test]
fn move_panel_elapsed_counts_the_operation_across_stage_changes() {
    let mut dashboard = dashboard_with_session(running_session());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    dashboard.begin_session_operation_at(
        "session-1".into(),
        SessionOperationKind::Moving,
        None,
        now - 125,
    );
    dashboard
        .session_operations
        .get_mut("session-1")
        .unwrap()
        .active_stages
        .insert(ProvisionStage::Restoring, now);
    let mut terminal = Terminal::new(TestBackend::new(160, 40)).unwrap();
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .unwrap();
    let text = buffer_lines(terminal.backend().buffer()).join("\n");
    assert!(text.contains("Current stage: Restore"), "{text}");
    assert!(text.contains("Elapsed: 2m"), "{text}");
}

// Hard-won: 90c11161: The launch clock paired each new stage name with elapsed time from the whole operation instead of that stage.
#[test]
fn stage_clock_counts_from_when_the_stage_began_not_the_operation() {
    let session = stopped_session();
    let mut operation = operation(
        SessionOperationKind::Launching,
        Some(ProvisionStage::Booting),
    );
    // The operation started at 1_000 but the stage only began at 1_040;
    // the clock must count from the stage, not the whole operation.
    operation
        .active_stages
        .insert(ProvisionStage::Booting, 1_040);

    let text = session_metadata_text(&session, None, Some(&operation), 1_052, &config());
    assert!(text.contains("Boot 12s"), "{text}");
}

#[test]
fn launch_clock_names_concurrent_stages_in_lifecycle_order() {
    let session = stopped_session();
    let mut operation = operation(SessionOperationKind::Launching, None);
    operation
        .active_stages
        .insert(ProvisionStage::Syncing, 1_003);
    operation
        .active_stages
        .insert(ProvisionStage::Cloning, 1_002);

    let text = session_metadata_text(&session, None, Some(&operation), 1_012, &config());
    assert!(text.contains("Clone, Sync 10s"), "{text}");
}

#[test]
fn focused_panes_use_quiet_rounded_borders_and_accent_titles_without_focus_labels() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.set_deployment_capacity_targets(vec![test_capacity_target()]);
    let backend = TestBackend::new(120, 40);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw dashboard");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("╭ Sessions "));
    assert!(rendered.contains("Targets"));
    assert!(!rendered.contains("[focused]"));

    for (focus, rounded) in [(Focus::Quota, "╭ Profiles"), (Focus::Targets, "╭ Targets")] {
        dashboard.focus = focus;
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw dashboard");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains(rounded), "{focus:?}: {rendered:?}");
        assert!(!rendered.contains("[focused]"));
        let pane_index = if focus == Focus::Quota { 2 } else { 1 };
        let area = dashboard.pane_areas.expect("pane areas")[pane_index];
        let border = &terminal.backend().buffer()[(area.x, area.y)];
        assert_eq!(Some(border.fg), theme::border(true).fg);
        assert_eq!(border.modifier, theme::border(true).add_modifier);
        assert_eq!(
            terminal.backend().buffer()[(area.x + 2, area.y)].fg,
            theme::palette().accent,
        );
    }
}

#[test]
fn quota_bars_show_fractional_remaining_capacity_and_blank_missing_windows() {
    let window = QuotaWindow {
        label: "Week".into(),
        remaining_percent: Some(73),
        used: None,
        limit: None,
        resets: None,
        resets_at_epoch_seconds: None,
    };

    let bar = quota_bar(Some(&window));
    assert_eq!(
        bar.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>(),
        "███████▎  ▏73%"
    );
    assert_eq!(bar.spans[0].style.fg, Some(theme::palette().success));
    assert_eq!(bar.spans[2].style.fg, None);
    assert!(
        bar.spans[..4]
            .iter()
            .all(|span| span.style.bg == Some(theme::palette().background))
    );
    assert_eq!(bar.spans[3].style.fg, Some(theme::palette().muted));
    let chart = quota_chart(bar, true);
    assert_eq!(chart.to_string(), "▕███████▎  ▏73%");
    assert_eq!(chart.spans[0].style, quota_chart_border_style());
    assert!(quota_bar(None).spans.is_empty());
}

/// One session whose title is `title`, measured at `recent` permille.
fn dashboard_with_measured_session(title: &str, recent: u16) -> DashboardState {
    let mut session = running_session();
    session.title = title.into();
    session.acp_session_title = None;
    let id = session.id.clone();
    let mut dashboard = dashboard_with_session(session);
    set_cpu(&mut dashboard, &[(&id, recent, recent)]);
    dashboard
}

fn row_texts(dashboard: &DashboardState, width: u16, minimized: bool) -> Vec<String> {
    let options = if minimized {
        SessionRowsRenderOptions::MINIMIZED
    } else {
        SessionRowsRenderOptions::DASHBOARD
    };
    drawn_session_rows_with_options(dashboard, width, options)
        .iter()
        .flat_map(|row| row.lines.iter().map(ToString::to_string))
        .collect()
}

/// The rows from the session's name line on, past the project heading.
fn name_and_following(dashboard: &DashboardState, width: u16, minimized: bool) -> Vec<String> {
    let lines = row_texts(dashboard, width, minimized);
    let start = lines
        .iter()
        .position(|line| line.starts_with('›'))
        .unwrap_or(0);
    lines[start..].to_vec()
}

#[test]
fn a_narrow_session_row_truncates_the_name_before_dropping_cpu() {
    let title = "Refactor the parser to stream tokens";
    let dashboard = dashboard_with_measured_session(title, 230);
    let narrow = name_and_following(&dashboard, 30, false);
    assert!(narrow[0].ends_with("23%"), "{narrow:#?}");
    assert!(!narrow[0].contains(title), "{narrow:#?}");
    assert!(narrow[0].contains("Refactor"), "{narrow:#?}");
    assert!(narrow[0].contains(theme::glyphs().ellipsis), "{narrow:#?}");
    // Too narrow for even a minimal name beside the figure: the figure goes.
    let tiny = name_and_following(&dashboard, 16, false);
    assert!(tiny.iter().all(|line| !line.contains('%')), "{tiny:#?}");
    assert!(tiny[0].contains("Refac"), "{tiny:#?}");
}

#[test]
fn session_cpu_report_groups_sorts_and_refreshes_while_open() {
    use mj_client::runtime_feed::SessionCpuView;
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.state.sessions.clear();
    let mut cpu = mj_core::snapshot_map::SnapshotMap::new();
    for (id, machine, hourly) in [
        ("slow", "machine-a", Some(100)),
        ("fast", "machine-a", Some(300)),
        ("other", "machine-b", Some(200)),
        ("missing", "machine-b", None),
        ("failed", "machine-a", None),
    ] {
        let mut session = running_session();
        session.id = id.into();
        session.title = id.into();
        session.acp_session_title = None;
        session.target_template_id = machine.into();
        dashboard.state.sessions.insert(id.into(), session);
        if let Some(hourly) = hourly {
            cpu.insert(
                id.into(),
                SessionCpuView::Measured {
                    usage: mj_core::cpu_usage::SessionCpuUsage {
                        recent_permille: 230,
                        hourly_permille: hourly,
                        hourly_covered_secs: 840,
                        online_cpus: 8,
                    },
                },
            );
        }
    }
    cpu.insert(
        "failed".into(),
        SessionCpuView::Unavailable {
            reason: "permission denied".into(),
        },
    );
    dashboard.set_session_cpu(cpu.clone());
    dashboard.set_deployment_capacity_targets(
        ["machine-a", "machine-b"]
            .into_iter()
            .map(|host| mj_core::targets::DeploymentCapacityTarget {
                id: host.into(),
                host: host.into(),
                target_ids: vec![host.into()],
                kind: DeploymentCapacityKind::Host,
                local: true,
                probes: Vec::new(),
                probe_error: None,
            })
            .collect(),
    );
    dashboard.dispatch_command(crate::CommandId::SessionCpuReport);
    let mut terminal = Terminal::new(TestBackend::new(160, 40)).unwrap();
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .unwrap();
    let text = buffer_lines(terminal.backend().buffer()).join("\n");
    assert!(
        text.find("machine-a ·").unwrap() < text.find("machine-b ·").unwrap(),
        "{text}"
    );
    assert!(text.contains("machine-a · 40% hourly"), "{text}");
    assert!(text.contains("machine-b · 20% hourly"), "{text}");
    assert!(
        text.find("fast  [codex-1]").unwrap() < text.find("slow  [codex-1]").unwrap(),
        "{text}"
    );
    assert!(
        text.find("slow  [codex-1]").unwrap() < text.find("failed  [codex-1]").unwrap(),
        "{text}"
    );
    assert!(text.contains("permission denied"));
    assert!(text.contains("(14m)"));
    assert!(
        !text.contains("other  [codex-1]"),
        "the second machine is a tab:\n{text}"
    );

    // Tabs stay alphabetically ordered, and Right displays the next machine.
    dashboard.handle_key(key(KeyCode::Right));
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .unwrap();
    let text = buffer_lines(terminal.backend().buffer()).join("\n");
    assert!(text.contains("other  [codex-1]"), "{text}");
    assert!(text.contains("no CPU data yet"), "{text}");
    assert!(!text.contains("fast  [codex-1]"), "{text}");

    // A click on a tab selects it without changing the alphabetic tab order.
    let tab = point(
        &text.lines().map(str::to_owned).collect::<Vec<_>>(),
        "machine-a ·",
    );
    click_cpu_report(&mut dashboard, tab);
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .unwrap();
    let text = buffer_lines(terminal.backend().buffer()).join("\n");
    assert!(text.contains("fast  [codex-1]"), "{text}");
    assert!(
        text.find("machine-a ·").unwrap() < text.find("machine-b ·").unwrap(),
        "{text}"
    );
    dashboard.acknowledge_render();
    cpu.insert(
        "other".into(),
        SessionCpuView::Measured {
            usage: mj_core::cpu_usage::SessionCpuUsage {
                recent_permille: 230,
                hourly_permille: 900,
                hourly_covered_secs: 3600,
                online_cpus: 8,
            },
        },
    );
    dashboard.set_session_cpu(cpu);
    assert!(
        dashboard.clock_changed(),
        "an open report must be invalidated by a CPU sample"
    );
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .unwrap();
    let text = buffer_lines(terminal.backend().buffer()).join("\n");
    assert!(
        text.find("machine-a ·").unwrap() < text.find("machine-b ·").unwrap(),
        "{text}"
    );
}

fn click_cpu_report(dashboard: &mut DashboardState, position: (u16, u16)) -> DashboardAction {
    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        let action = dashboard.handle_mouse(mouse_at(kind, position));
        if kind == MouseEventKind::Up(MouseButton::Left) {
            return action;
        }
    }
    unreachable!()
}

#[test]
fn session_cpu_report_opens_on_the_selected_sessions_machine() {
    let mut dashboard = dashboard_with_session(running_session());
    let mut first = running_session();
    first.id = "machine-a-session".into();
    first.title = first.id.clone();
    first.acp_session_title = None;
    first.target_template_id = "machine-a".into();
    let mut selected = running_session();
    selected.id = "machine-b-session".into();
    selected.title = selected.id.clone();
    selected.acp_session_title = None;
    selected.target_template_id = "machine-b".into();
    let mut state = dashboard.state.clone();
    state.sessions.clear();
    state.sessions.insert(first.id.clone(), first);
    state.sessions.insert(selected.id.clone(), selected.clone());
    dashboard.set_state(state);
    dashboard.set_deployment_capacity_targets(
        ["machine-a", "machine-b"]
            .into_iter()
            .map(|host| mj_core::targets::DeploymentCapacityTarget {
                id: host.into(),
                host: host.into(),
                target_ids: vec![host.into()],
                kind: DeploymentCapacityKind::Host,
                local: true,
                probes: Vec::new(),
                probe_error: None,
            })
            .collect(),
    );
    dashboard.select_active_session(&selected.id);
    dashboard.dispatch_command(crate::CommandId::SessionCpuReport);

    let text = drawn(&mut dashboard, 120, 32).join("\n");
    assert!(text.contains("machine-b-session  [codex-1]"), "{text}");
    assert!(!text.contains("machine-a-session  [codex-1]"), "{text}");
}

fn measured_cpu(recent: u16, hourly: u16) -> mj_client::runtime_feed::SessionCpuView {
    mj_client::runtime_feed::SessionCpuView::Measured {
        usage: mj_core::cpu_usage::SessionCpuUsage {
            recent_permille: recent,
            hourly_permille: hourly,
            hourly_covered_secs: 3600,
            online_cpus: 8,
        },
    }
}

/// A parent with the given running sub-agents, each named by its id.
fn dashboard_with_subagents(children: &[&str]) -> (DashboardState, String) {
    let (mut dashboard, parent) = crate::test_support::dashboard_with_one_subagent();
    let mut state = dashboard.state.clone();
    let template = state.sessions["child-session"].clone();
    let relation = state.subagents["child-session"].clone();
    state.sessions.remove("child-session");
    state.subagents.remove("child-session");
    for id in children {
        let mut child = template.clone();
        child.id = (*id).into();
        child.title = (*id).into();
        child.acp_session_title = None;
        let mut link = relation.clone();
        link.child_session_id = (*id).into();
        link.request_key = format!("request-{id}");
        state.sessions.insert(child.id.clone(), child);
        state.subagents.insert((*id).into(), link);
    }
    dashboard.set_state(state);
    (dashboard, parent)
}

fn set_cpu(dashboard: &mut DashboardState, entries: &[(&str, u16, u16)]) {
    let mut cpu = mj_core::snapshot_map::SnapshotMap::new();
    for (id, recent, hourly) in entries {
        cpu.insert((*id).into(), measured_cpu(*recent, *hourly));
    }
    dashboard.set_session_cpu(cpu);
}

#[test]
fn session_cpu_report_excludes_parked_stopped_and_failed_subagents() {
    let (mut dashboard, _) = dashboard_with_subagents(&[
        "parked-child",
        "stopped-child",
        "error-child",
        "active-child",
    ]);
    let mut state = dashboard.state.clone();
    state.sessions.get_mut("parked-child").unwrap().state = SessionState::Parked;
    state.sessions.get_mut("stopped-child").unwrap().state = SessionState::Stopped;
    state.sessions.get_mut("error-child").unwrap().state = SessionState::Error;
    dashboard.set_state(state);

    let ids = crate::session_cpu_report::report_groups(&dashboard)
        .into_iter()
        .flat_map(|group| group.session_ids)
        .collect::<Vec<_>>();
    assert!(
        ids.contains(&"session-1".to_owned()),
        "parent is active: {ids:?}"
    );
    assert!(
        ids.contains(&"active-child".to_owned()),
        "active child: {ids:?}"
    );
    assert!(!ids.contains(&"parked-child".to_owned()), "{ids:?}");
    assert!(!ids.contains(&"stopped-child".to_owned()), "{ids:?}");
    assert!(!ids.contains(&"error-child".to_owned()), "{ids:?}");
}

#[test]
fn session_cpu_report_selection_stays_with_its_session_after_cpu_resort() {
    let mut dashboard = dashboard_with_session(running_session());
    let mut state = dashboard.state.clone();
    state.sessions.clear();
    for id in ["fast", "middle", "slow"] {
        let mut session = running_session();
        session.id = id.into();
        session.title = id.into();
        session.acp_session_title = None;
        session.target_template_id = "shared-machine".into();
        state.sessions.insert(id.into(), session);
    }
    dashboard.set_state(state);
    set_cpu(
        &mut dashboard,
        &[("fast", 300, 300), ("middle", 200, 200), ("slow", 100, 100)],
    );
    dashboard.dispatch_command(crate::CommandId::SessionCpuReport);
    dashboard.handle_key(key(KeyCode::End));
    let crate::Mode::SessionCpuReport(dialog) = &dashboard.mode else {
        panic!("CPU report is open");
    };
    assert_eq!(dialog.selected_session_id.borrow().as_deref(), Some("slow"));

    set_cpu(
        &mut dashboard,
        &[("fast", 50, 50), ("middle", 200, 200), ("slow", 500, 500)],
    );
    let _ = drawn(&mut dashboard, 120, 30);
    let crate::Mode::SessionCpuReport(dialog) = &dashboard.mode else {
        panic!("CPU report remains open");
    };
    assert_eq!(
        dialog.selected_session_id.borrow().as_deref(),
        Some("slow"),
        "the cursor follows the selected id through re-sorting"
    );
    assert_eq!(dialog.selected_row.get(), 0);
}

#[test]
fn enter_on_a_cpu_report_top_level_row_opens_its_session() {
    let mut dashboard = dashboard_with_session(running_session());
    let session_id = dashboard.selected_session_id().unwrap().to_owned();
    dashboard.dispatch_command(crate::CommandId::SessionCpuReport);

    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::Open {
            session_id: session_id.clone()
        }
    );
    assert!(matches!(dashboard.mode, crate::Mode::Dashboard));
    assert_eq!(dashboard.selected_session_id(), Some(session_id.as_str()));
}

#[test]
fn enter_on_a_cpu_report_subagent_opens_its_parent_view_and_conversation() {
    let (mut dashboard, parent) = dashboard_with_subagents(&["kid-a"]);
    // A filter that would hide the child is cleared by going to it.
    *dashboard.sessions_filter = Some(crate::SessionsFilter {
        query: mj_chat::text_input::TextInput::from_value("matches nothing"),
        state: None,
        editing: false,
    });
    dashboard.dispatch_command(crate::CommandId::SessionCpuReport);
    dashboard.handle_key(key(KeyCode::Down));

    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::Open {
            session_id: "kid-a".into()
        }
    );
    assert_eq!(dashboard.subagent_parent_id(), Some(parent.as_str()));
    assert_eq!(dashboard.selected_session_id(), Some("kid-a"));
    assert!(dashboard.sessions_filter.is_none());
}

#[test]
fn double_click_on_a_cpu_report_subagent_opens_its_parent_view() {
    let (mut dashboard, parent) = dashboard_with_subagents(&["kid-a"]);
    dashboard.dispatch_command(crate::CommandId::SessionCpuReport);
    let first = drawn(&mut dashboard, 120, 35);
    let child_row = point(&first, "kid-a");
    assert_eq!(
        click_cpu_report(&mut dashboard, child_row),
        DashboardAction::None
    );
    let crate::Mode::SessionCpuReport(dialog) = &dashboard.mode else {
        panic!("first click keeps the CPU report open");
    };
    assert_eq!(
        dialog.selected_session_id.borrow().as_deref(),
        Some("kid-a")
    );
    let _ = drawn(&mut dashboard, 120, 35);

    assert_eq!(
        click_cpu_report(&mut dashboard, child_row),
        DashboardAction::Open {
            session_id: "kid-a".into()
        }
    );
    assert_eq!(dashboard.subagent_parent_id(), Some(parent.as_str()));
    assert_eq!(dashboard.selected_session_id(), Some("kid-a"));
}

#[test]
fn cpu_report_subagent_navigation_restores_the_parent_view_after_workspace_switch() {
    let (mut dashboard, parent) = dashboard_with_subagents(&["kid-a"]);
    let mut state = dashboard.state.clone();
    state.sessions.get_mut(&parent).unwrap().workspace_id = "other".into();
    state.sessions.get_mut("kid-a").unwrap().workspace_id = "other".into();
    dashboard.set_state(state);
    dashboard.workspace_order.push("other".into());
    dashboard
        .workspace_names
        .insert("other".into(), "Other".into());
    dashboard.dispatch_command(crate::CommandId::SessionCpuReport);
    dashboard.handle_key(key(KeyCode::Down));

    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::SelectWorkspace {
            workspace_id: "other".into()
        }
    );
    assert_eq!(dashboard.navigation_session.as_deref(), Some("kid-a"));
    assert_eq!(
        dashboard.navigation_subagent_parent.as_deref(),
        Some(parent.as_str())
    );

    dashboard.set_active_workspace(Some("other".into()));
    assert_eq!(dashboard.subagent_parent_id(), Some(parent.as_str()));
    assert_eq!(dashboard.selected_session_id(), Some("kid-a"));
    assert_eq!(
        dashboard.take_navigation_session().as_deref(),
        Some("kid-a")
    );
}

#[test]
fn parent_cpu_is_its_own_plus_every_live_subagent() {
    let (mut dashboard, parent) = dashboard_with_subagents(&["kid-a", "kid-b"]);
    set_cpu(
        &mut dashboard,
        &[(&parent, 30, 20), ("kid-a", 400, 300), ("kid-b", 250, 100)],
    );
    let share = dashboard.cpu_share(&parent).unwrap();
    assert_eq!((share.permille, share.partial), (680, false));
    assert_eq!(dashboard.cpu_rollup(&parent).hourly_permille, 420);
    // A child's own figure is its own, not folded into anything.
    let child = dashboard.cpu_share("kid-a").unwrap();
    assert_eq!((child.permille, child.partial), (400, false));
}

#[test]
fn parent_cpu_is_marked_partial_when_a_subagent_is_unmeasured() {
    let (mut dashboard, parent) = dashboard_with_subagents(&["kid-a", "kid-b"]);
    set_cpu(&mut dashboard, &[(&parent, 30, 20), ("kid-a", 400, 300)]);
    let share = dashboard.cpu_share(&parent).unwrap();
    assert_eq!((share.permille, share.partial), (430, true));
    assert_eq!(share.label(), "43%+");
    let rollup = dashboard.cpu_rollup(&parent);
    assert_eq!((rollup.measured, rollup.members), (2, 3));
    // Nothing measured anywhere in the tree shows nothing.
    set_cpu(&mut dashboard, &[]);
    assert_eq!(dashboard.cpu_share(&parent), None);
}

#[test]
fn cpu_rollup_is_derived_only_when_samples_or_the_tree_change() {
    let (mut dashboard, parent) = dashboard_with_subagents(&["kid-a"]);
    set_cpu(&mut dashboard, &[(&parent, 30, 20), ("kid-a", 400, 300)]);
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .unwrap();
    dashboard.clock_changed();
    let settled = dashboard.cpu_rollup_derivations();
    for _ in 0..3 {
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .unwrap();
        dashboard.clock_changed();
    }
    assert_eq!(dashboard.cpu_rollup_derivations(), settled);
    set_cpu(&mut dashboard, &[(&parent, 30, 20), ("kid-a", 500, 300)]);
    assert_eq!(dashboard.cpu_share(&parent).unwrap().permille, 530);
    assert_eq!(dashboard.cpu_rollup_derivations(), settled + 1);
    let mut state = dashboard.state.clone();
    state.subagents.remove("kid-a");
    dashboard.set_state(state);
    assert_eq!(dashboard.cpu_share(&parent).unwrap().permille, 30);
    assert_eq!(dashboard.cpu_rollup_derivations(), settled + 2);
}

#[test]
fn session_cpu_report_nests_subagents_under_their_parent_with_the_tree_total() {
    let (mut dashboard, parent) = dashboard_with_subagents(&["kid-a", "kid-b"]);
    set_cpu(&mut dashboard, &[(&parent, 30, 20), ("kid-a", 400, 300)]);
    dashboard.dispatch_command(crate::CommandId::SessionCpuReport);
    let mut terminal = Terminal::new(TestBackend::new(140, 30)).unwrap();
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .unwrap();
    let text = buffer_lines(terminal.backend().buffer()).join("\n");
    println!("{text}");
    let lines = crate::session_cpu_report::report_lines(&dashboard)
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>();
    let parent_line = lines
        .iter()
        .position(|line| line.contains("tree of 3"))
        .unwrap_or_else(|| panic!("{lines:#?}"));
    assert!(
        lines[parent_line].contains("32% hourly+") || lines[parent_line].contains("32%+ hourly"),
        "{lines:#?}"
    );
    assert!(
        lines[parent_line].contains("own 3.0%, 2 of 3 measured"),
        "{lines:#?}"
    );
    assert!(
        lines[parent_line + 1].starts_with("    kid-a"),
        "{lines:#?}"
    );
    assert!(
        lines[parent_line + 2].starts_with("    kid-b"),
        "{lines:#?}"
    );
    assert!(lines[parent_line + 2].contains("no CPU data yet"));
}

// Hard-won: c17d3225: A stale table offset hid fitting session rows after a list shrank.
#[test]
fn sessions_pane_shows_every_row_from_the_top_after_the_list_shrank() {
    let (mut dashboard, _parent) = dashboard_with_subagents(&["worker-a", "worker-b"]);
    // An earlier, longer list had scrolled the pane down.
    dashboard.sessions_scroll.set(3);
    let text = crate::test_support::drawn(&mut dashboard, 120, 50).join("\n");
    assert!(text.contains("ACP pretty name"), "{text}");
    assert_eq!(dashboard.sessions_scroll.get(), 0);
}

/// The Sub-agents view is the Sessions pane listing only a parent's children.
/// A render that kept an offset from a longer list hid the first children
/// although all of them fit.
// Hard-won: c17d3225: A stale parent-list offset hid children and made a running child look parked after switching to Sub-agents.
#[test]
fn subagents_pane_shows_every_child_from_the_top_with_a_stale_offset() {
    let (mut dashboard, parent) = dashboard_with_subagents(&["worker-a", "worker-b", "worker-c"]);
    dashboard.open_subagent_workspace(parent);
    dashboard.select_active_session("worker-c");
    dashboard.sessions_scroll.set(2);
    let text = crate::test_support::drawn(&mut dashboard, 120, 60).join("\n");
    for id in ["worker-a", "worker-b", "worker-c"] {
        assert!(text.contains(id), "{id} hidden:\n{text}");
    }
    assert_eq!(dashboard.sessions_scroll.get(), 0);
}

/// The screen row of the line that draws `title` in the Sessions pane.
fn row_of_title(lines: &[String], title: &str) -> Option<usize> {
    lines.iter().position(|line| line.contains(title))
}

#[test]
fn sessions_pane_keeps_the_keyboard_selection_centred_and_pins_the_ends() {
    let names = (0..30).map(|n| format!("child-{n:02}")).collect::<Vec<_>>();
    let refs = names.iter().map(String::as_str).collect::<Vec<_>>();
    let (mut dashboard, parent) = dashboard_with_subagents(&refs);
    dashboard.open_subagent_workspace(parent);
    dashboard.focus = Focus::Sessions;
    let lines = crate::test_support::drawn(&mut dashboard, 120, 40);
    let first = row_of_title(&lines, "child-00").expect("first child drawn at the top");
    assert_eq!(dashboard.sessions_scroll.get(), 0);

    // Walk down one key at a time. Once the selection passes the middle it
    // holds one screen row until the list pins to its end.
    let mut rows = Vec::new();
    let mut offsets = Vec::new();
    for _ in 0..29 {
        dashboard.move_selection(1);
        let lines = crate::test_support::drawn(&mut dashboard, 120, 40);
        offsets.push(dashboard.sessions_scroll.get());
        let selected = dashboard.selected_session_id().expect("selection");
        let title = dashboard.state.sessions[selected].title.clone();
        rows.push(row_of_title(&lines, &title).expect("selected child stays visible"));
    }
    assert!(
        offsets.windows(2).all(|pair| pair[0] <= pair[1]),
        "{offsets:?}"
    );
    let middle = *rows
        .iter()
        .max_by_key(|candidate| rows.iter().filter(|row| row == candidate).count())
        .expect("rows");
    let held = rows.iter().filter(|row| **row == middle).count();
    assert!(held >= 10, "selection holds the centre row: {rows:?}");
    assert!(middle > first + 3, "{rows:?}");
    // At the end the last child is the last row, so the selection has moved
    // below the centre.
    assert!(*rows.last().unwrap() > middle, "{rows:?}");
    // Moving back up to the first child returns the list to its top.
    for _ in 0..29 {
        dashboard.move_selection(-1);
        let _ = crate::test_support::drawn(&mut dashboard, 120, 40);
    }
    assert_eq!(dashboard.sessions_scroll.get(), 0);
}

#[test]
fn golden_dashboard_session_render() {
    let mut output = String::new();

    let mut dashboard = dashboard_with_session(running_session());
    apply_materialized_transcript(&mut dashboard, numbered_conversation(2));
    clear_session_activity(&mut dashboard, "session-1");
    dashboard
        .session_details
        .get_mut("session-1")
        .unwrap()
        .queued_prompts
        .push(mj_core::relay::QueuedPrompt {
            id: "queued-1".into(),
            text: "later".into(),
            attachments: Vec::new(),
            created_at_ms: 1,
        });
    append_dashboard_golden(
        &mut output,
        "grouped session summary",
        120,
        30,
        &mut dashboard,
    );

    for (symbols, state_name) in [
        (mj_core::config::SymbolSet::Unicode, "Unicode"),
        (mj_core::config::SymbolSet::Ascii, "ASCII"),
    ] {
        let close = if symbols == mj_core::config::SymbolSet::Unicode {
            '×'
        } else {
            'x'
        };
        let dot = if symbols == mj_core::config::SymbolSet::Unicode {
            '·'
        } else {
            '-'
        };
        let mut dashboard = dashboard_with_session(running_session());
        let mut config = dashboard.config.clone();
        config.advanced.symbols = Some(symbols);
        dashboard.set_config(config);
        clear_session_activity(&mut dashboard, "session-1");
        dashboard.focus_sessions();
        append_dashboard_golden(
            &mut output,
            &format!("{state_name} unfiltered Sessions title"),
            120,
            40,
            &mut dashboard,
        );
        dashboard.handle_key(key(KeyCode::Char('w')));
        append_dashboard_golden(
            &mut output,
            &format!("{state_name} working filter"),
            120,
            40,
            &mut dashboard,
        );
        dashboard.handle_key(key(KeyCode::Char('b')));
        append_dashboard_golden(
            &mut output,
            &format!("{state_name} blocked filter at wide size"),
            240,
            40,
            &mut dashboard,
        );
        dashboard.handle_key(key(KeyCode::Esc));
        append_dashboard_golden(
            &mut output,
            &format!("{state_name} cleared filter"),
            120,
            40,
            &mut dashboard,
        );
        append_golden_value(
            &mut output,
            &format!("{state_name} title markers"),
            (close, dot),
        );
    }

    for (width, side, label) in [
        (80, mj_core::config::SessionsSide::Left, "80 left"),
        (80, mj_core::config::SessionsSide::Right, "80 right"),
        (120, mj_core::config::SessionsSide::Left, "120 left"),
        (120, mj_core::config::SessionsSide::Right, "120 right"),
        (180, mj_core::config::SessionsSide::Left, "180 left"),
        (180, mj_core::config::SessionsSide::Right, "180 right"),
    ] {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.config.sessions_side = side;
        dashboard
            .session_details
            .get_mut("session-1")
            .expect("fixture detail")
            .queued_prompts
            .push(mj_core::relay::QueuedPrompt {
                id: "queued".into(),
                text: "follow-up".into(),
                attachments: Vec::new(),
                created_at_ms: 1,
            });
        append_dashboard_golden(
            &mut output,
            &format!("expanded rows, {label}"),
            width,
            40,
            &mut dashboard,
        );
    }
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.set_pane_size(SupportPane::Sessions, PaneSize::Minimized);
    append_dashboard_golden(
        &mut output,
        "minimized rows at 80 columns",
        80,
        40,
        &mut dashboard,
    );

    let mut dashboard = dashboard_with_session(running_session());
    dashboard.focus_sessions();
    append_dashboard_golden(&mut output, "expanded session row", 120, 44, &mut dashboard);

    let mut dashboard = minimized_sessions_dashboard(3, 2);
    append_dashboard_golden(
        &mut output,
        "minimized session summaries",
        120,
        44,
        &mut dashboard,
    );

    let mut dashboard = dashboard_with_session(running_session());
    dashboard.set_deployment_capacity_targets(vec![test_capacity_target()]);
    dashboard.apply_deployment_capacity("local", Ok(Some(host_usage(42))), now_seconds());
    dashboard.apply_quota(weekly_quota("claude-1", 63));
    minimize_all_panes(&mut dashboard);
    append_dashboard_golden(
        &mut output,
        "minimized CPU and weekly quota",
        120,
        44,
        &mut dashboard,
    );

    let mut dashboard = dashboard_with_session(running_session());
    dashboard.apply_quota(weekly_and_five_hour_quota("claude-1", 96, 40));
    dashboard.apply_quota(weekly_and_five_hour_quota("codex-1", 100, 40));
    dashboard.apply_quota(weekly_quota("codex-2", 63));
    minimize_all_panes(&mut dashboard);
    append_dashboard_golden(
        &mut output,
        "paired weekly and five-hour quota",
        160,
        44,
        &mut dashboard,
    );

    mj_core::golden::assert_golden(
        env!("CARGO_MANIFEST_DIR"),
        "dashboard-session-render",
        &output,
    );
}

#[test]
fn golden_dashboard_layout_render() {
    let mut output = String::new();

    let mut dashboard = dashboard_with_session(running_session());
    dashboard.set_workspace_name("UNDERLYING DASHBOARD SENTINEL".into());
    dashboard.focus_sessions();
    open_palette(&mut dashboard);
    for character in "rename".chars() {
        dashboard.handle_key(key(KeyCode::Char(character)));
    }
    let action = dashboard.handle_key(key(KeyCode::Enter));
    append_golden_value(&mut output, "open Rename action", action);
    append_dashboard_golden(
        &mut output,
        "Rename modal over dashboard",
        120,
        30,
        &mut dashboard,
    );

    for height in [32, 44] {
        let mut dashboard = minimized_sessions_dashboard(3, 2);
        dashboard
            .restore_pane_sizes(crate::PaneSizes::default())
            .unwrap();
        dashboard.focus_sessions();
        append_dashboard_golden(
            &mut output,
            &format!("standard panes at height {height}"),
            120,
            height,
            &mut dashboard,
        );
        append_golden_value(
            &mut output,
            &format!("standard transcript area at height {height}"),
            dashboard.focused_transcript_area().unwrap(),
        );
        let action = chord(&mut dashboard, crate::CommandId::TogglePanePreset);
        append_golden_value(&mut output, "compact pane preset action", action);
        append_dashboard_golden(
            &mut output,
            &format!("compact panes at height {height}"),
            120,
            height,
            &mut dashboard,
        );
        append_golden_value(
            &mut output,
            &format!("compact transcript area at height {height}"),
            dashboard.focused_transcript_area().unwrap(),
        );
        let action = chord(&mut dashboard, crate::CommandId::TogglePanePreset);
        append_golden_value(&mut output, "restore pane preset action", action);
        append_dashboard_golden(
            &mut output,
            &format!("restored panes at height {height}"),
            120,
            height,
            &mut dashboard,
        );
        append_golden_value(
            &mut output,
            &format!("restored transcript area at height {height}"),
            dashboard.focused_transcript_area().unwrap(),
        );
    }

    let mut dashboard = dashboard_with_session(running_session());
    dashboard.set_deployment_capacity_targets(vec![test_capacity_target()]);
    for width in [59, 70, 80] {
        append_dashboard_golden(
            &mut output,
            &format!("dashboard at {width} columns"),
            width,
            30,
            &mut dashboard,
        );
        append_golden_value(
            &mut output,
            &format!("dashboard geometry at {width} columns"),
            (
                dashboard.pane_areas,
                dashboard.conversation_area,
                dashboard.sessions_minimized(),
            ),
        );
    }

    mj_core::golden::assert_golden(
        env!("CARGO_MANIFEST_DIR"),
        "dashboard-layout-render",
        &output,
    );
}

#[test]
fn golden_dashboard_footer_render() {
    let mut output = String::new();

    let mut dashboard = DashboardState::new(config(), State::default(), BTreeMap::new());
    dashboard.set_workspace_name("personal".into());
    dashboard.set_notice("Transient dashboard message");
    append_dashboard_golden(
        &mut output,
        "notice takes over the one-row footer",
        120,
        24,
        &mut dashboard,
    );
    dashboard.notices.clear();
    append_dashboard_golden(
        &mut output,
        "footer hints after notice",
        120,
        24,
        &mut dashboard,
    );

    for focus in [Focus::Sessions, Focus::Targets, Focus::Quota, Focus::Prompt] {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.set_deployment_capacity_targets(vec![test_capacity_target()]);
        dashboard.focus = focus;
        append_dashboard_golden(
            &mut output,
            &format!("footer at {focus:?} focus"),
            200,
            24,
            &mut dashboard,
        );
    }

    let mut dashboard = dashboard_with_session(running_session());
    dashboard.set_deployment_capacity_targets(vec![test_capacity_target()]);
    dashboard.focus_sessions();
    for width in 0_u16..=80 {
        append_dashboard_golden(
            &mut output,
            &format!("footer at {width} columns"),
            width,
            30,
            &mut dashboard,
        );
    }

    for focus in [Focus::Sessions, Focus::Prompt] {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus = focus;
        let action = dashboard.route_bound_key(&crate::test_support::prefix_key());
        append_golden_value(&mut output, &format!("{focus:?} prefix action"), action);
        append_dashboard_golden(
            &mut output,
            &format!("{focus:?} pending chord"),
            120,
            40,
            &mut dashboard,
        );
        dashboard.cancel_prefix();
        append_dashboard_golden(
            &mut output,
            &format!("{focus:?} after cancelling chord"),
            120,
            40,
            &mut dashboard,
        );
    }

    mj_core::golden::assert_golden(
        env!("CARGO_MANIFEST_DIR"),
        "dashboard-footer-render",
        &output,
    );
}

#[test]
fn golden_dashboard_resource_panels() {
    let mut output = String::new();

    let mut target_config = Config::default();
    let mut sandbox = Config::default().with_local_targets().targets["docker"].clone();
    if let TargetTemplate::LocalDocker { container } = &mut sandbox {
        container.image = "example.test/own:latest".into();
    }
    target_config.targets.insert("sandbox".into(), sandbox);
    let mut dashboard = DashboardState::new(
        target_config.with_local_targets(),
        State::default(),
        BTreeMap::new(),
    );
    let mut target = test_capacity_target();
    target.target_ids = vec![
        "docker".into(),
        "localhost".into(),
        "podman".into(),
        "sandbox".into(),
    ];
    dashboard.set_deployment_capacity_targets(vec![target]);
    let readiness = dashboard.take_target_availability_check();
    append_golden_value(&mut output, "target readiness request", &readiness);
    if let Some(crate::DashboardAction::CheckTargetReadiness {
        generation,
        target_ids,
    }) = readiness
    {
        for id in ["docker", "sandbox"] {
            dashboard.apply_target_runtime_missing(
                generation,
                id.into(),
                "docker: not found".into(),
            );
        }
        append_golden_value(&mut output, "checked target ids", target_ids);
    }
    append_dashboard_golden(
        &mut output,
        "Targets hides unavailable defaults",
        160,
        40,
        &mut dashboard,
    );

    let mut dashboard = DashboardState::new(config(), State::default(), BTreeMap::new());
    let mut target = test_capacity_target();
    target.target_ids = vec!["podman".into(), "mac-container".into()];
    dashboard.set_deployment_capacity_targets(vec![target]);
    dashboard.apply_deployment_capacity(
        "local",
        Ok(Some(DeploymentCapacityUsage {
            cpu_percent: Some(37),
            memory_used_bytes: 3,
            memory_total_bytes: 4,
            logical_cores: 8,
            disk_total_bytes: None,
            storage: Vec::new(),
        })),
        now_epoch_seconds(),
    );
    append_dashboard_golden(
        &mut output,
        "grouped host load without sample clock",
        120,
        40,
        &mut dashboard,
    );

    let mut dashboard = DashboardState::new(
        config(),
        State::default(),
        BTreeMap::from([(
            "codex-1".into(),
            ProfileQuota {
                banked_resets: None,
                profile_id: "codex-1".into(),
                harness: HarnessKind::Codex,
                windows: vec![],
                extra: None,
                error: Some("offline".into()),
                refreshed_at_epoch_seconds: 0,
                rate_limited_until_epoch_seconds: None,
            },
        )]),
    );
    append_dashboard_golden(
        &mut output,
        "quota error with refresh age",
        120,
        28,
        &mut dashboard,
    );

    let mut dashboard = DashboardState::new(
        config(),
        State::default(),
        BTreeMap::from([(
            "claude-1".into(),
            ProfileQuota {
                banked_resets: None,
                profile_id: "claude-1".into(),
                harness: HarnessKind::Claude,
                windows: vec![],
                extra: None,
                error: Some("login expired".into()),
                refreshed_at_epoch_seconds: 0,
                rate_limited_until_epoch_seconds: None,
            },
        )]),
    );
    append_dashboard_golden(&mut output, "expired login quota", 120, 28, &mut dashboard);

    let mut dashboard = DashboardState::new(config(), State::default(), BTreeMap::new());
    let mut quota = api_quota("codex-1");
    quota.refreshed_at_epoch_seconds = 0;
    dashboard.apply_quota(quota);
    append_dashboard_golden(
        &mut output,
        "usage-priced API quota",
        120,
        28,
        &mut dashboard,
    );

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let now = i64::try_from(now).unwrap();
    let quota = ProfileQuota {
        banked_resets: None,
        profile_id: "codex-1".into(),
        harness: HarnessKind::Codex,
        windows: vec![
            QuotaWindow {
                label: "Week".into(),
                remaining_percent: Some(73),
                used: None,
                limit: None,
                resets: Some("09:00 Aug 20".into()),
                resets_at_epoch_seconds: Some(now + 2 * 24 * 60 * 60 + 30),
            },
            QuotaWindow {
                label: "5H".into(),
                remaining_percent: Some(70),
                used: None,
                limit: None,
                resets: Some("14:00 Aug 13".into()),
                resets_at_epoch_seconds: Some(now + 60 * 60 + 5 * 60 + 30),
            },
        ],
        extra: None,
        error: None,
        refreshed_at_epoch_seconds: 0,
        rate_limited_until_epoch_seconds: None,
    };
    let mut dashboard = DashboardState::new(
        config(),
        State::default(),
        BTreeMap::from([("codex-1".into(), quota)]),
    );
    append_dashboard_golden(
        &mut output,
        "weekly and five-hour quota columns",
        140,
        28,
        &mut dashboard,
    );

    let quota = ProfileQuota {
        banked_resets: Some(1),
        profile_id: "codex-1".into(),
        harness: HarnessKind::Codex,
        windows: vec![
            QuotaWindow {
                label: "Week".into(),
                remaining_percent: Some(73),
                used: None,
                limit: None,
                resets: None,
                resets_at_epoch_seconds: Some(now + 2 * 24 * 60 * 60 + 30),
            },
            QuotaWindow {
                label: "5H".into(),
                remaining_percent: Some(70),
                used: None,
                limit: None,
                resets: None,
                resets_at_epoch_seconds: Some(now + 60 * 60 + 5 * 60 + 30),
            },
        ],
        extra: None,
        error: None,
        refreshed_at_epoch_seconds: 0,
        rate_limited_until_epoch_seconds: None,
    };
    let mut dashboard = DashboardState::new(
        config(),
        State::default(),
        BTreeMap::from([("codex-1".into(), quota)]),
    );
    append_dashboard_golden(
        &mut output,
        "weekly and five-hour quota at 80 columns",
        80,
        28,
        &mut dashboard,
    );

    mj_core::golden::assert_golden(
        env!("CARGO_MANIFEST_DIR"),
        "dashboard-resource-panels",
        &output,
    );
}
