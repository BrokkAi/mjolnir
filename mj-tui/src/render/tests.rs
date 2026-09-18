use std::collections::BTreeMap;

use crossterm::event::{KeyCode, MouseButton, MouseEventKind};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::Color;

use mj_core::config::{Config, HarnessKind};
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

#[test]
fn scrollbar_thumb_reaches_both_ends_of_the_viewport() {
    let mut terminal = Terminal::new(TestBackend::new(1, 10)).unwrap();
    for (position, thumb_row) in [(0, 1), (90, 8)] {
        terminal
            .draw(|frame| {
                render_session_scrollbar(frame, Rect::new(0, 0, 1, 10), 100, position, 10);
            })
            .unwrap();
        assert_eq!(terminal.backend().buffer()[(0, thumb_row)].symbol(), "▐");
    }
    terminal
        .draw(|frame| {
            render_session_scrollbar(frame, Rect::new(0, 0, 1, 10), 10, 0, 10);
        })
        .unwrap();
    assert!((0..10).all(|row| terminal.backend().buffer()[(0, row)].symbol() == " "));
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

#[test]
fn directional_lookahead_reserves_an_adjacent_variable_height_row() {
    let heights = [2, 3, 4, 2];

    assert_eq!(
        offset_with_directional_lookahead(0, 1, SelectionDirection::Down, &heights, 7),
        1,
        "the row after the selection is brought fully into view"
    );
    assert_eq!(
        offset_with_directional_lookahead(2, 2, SelectionDirection::Up, &heights, 7),
        1,
        "the row before the selection is brought fully into view"
    );
    assert_eq!(
        offset_with_directional_lookahead(0, 1, SelectionDirection::Down, &heights, 6),
        0,
        "an impossible two-row margin does not displace the selected row"
    );
}

#[test]
fn grouped_dashboard_has_no_column_header_and_uses_fixed_session_summaries() {
    let mut dashboard = dashboard_with_session(running_session());
    apply_materialized_transcript(&mut dashboard, numbered_conversation(2));
    dashboard
        .session_details
        .get_mut("session-1")
        .unwrap()
        .current_turn_started_at = Some(now_seconds().saturating_sub(10));
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
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("terminal");
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw dashboard");
    let rendered = buffer_lines(terminal.backend().buffer()).join("\n");

    assert!(rendered.contains("hel"));
    assert!(!rendered.contains("[1] hel"));
    assert!(!rendered.contains("Turn clock"));
    assert!(!rendered.contains("Session name"));
    assert!(rendered.contains("podman"), "{rendered}");
    assert!(rendered.contains("codex-1"), "{rendered}");
    assert!(rendered.contains("[Q 1]"));
    assert!(rendered.contains("Sessions"));
    assert!(!rendered.contains("Turn=time"));
    assert!(!rendered.contains("Step=time"));
    assert!(rendered.contains("codex-1"));
    assert!(!rendered.contains("queued]"));
    assert!(rendered.contains("answer 1"));
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
    dashboard.session_details.insert(
        foreign.id.clone(),
        SessionDetail {
            pending_elicitations: dashboard.session_details["session-1"]
                .pending_elicitations
                .clone(),
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
fn a_modal_overlays_the_dashboard_instead_of_replacing_it() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.set_workspace_name("UNDERLYING DASHBOARD SENTINEL".into());
    // The rename editor is reached through the command palette now.
    dashboard.focus_sessions();
    open_palette(&mut dashboard);
    for character in "rename".chars() {
        dashboard.handle_key(crate::test_support::key(KeyCode::Char(character)));
    }
    assert_eq!(
        dashboard.handle_key(crate::test_support::key(KeyCode::Enter)),
        DashboardAction::None
    );
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("terminal");

    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw rename dialog");
    let lines = buffer_lines(terminal.backend().buffer());

    let row_of = |needle: &str| {
        lines
            .iter()
            .position(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("missing {needle} in {lines:#?}"))
    };
    let popup_top = row_of("Rename session");
    // The dashboard underneath still shows through every row the modal's
    // centred popup does not cover.
    assert!(row_of("Sessions") < popup_top);
    assert!(
        row_of("podman") < popup_top,
        "the session row behind the popup still shows"
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

#[test]
fn the_sessions_title_prioritizes_workspace_and_controls_at_minimum_width() {
    let wide = sessions_title("a-workspace", 120, true).to_string();
    assert_eq!(wide, " Sessions · a-workspace ");
    assert!(!wide.contains("Turn"));
    assert!(!wide.contains("Step"));

    let narrow = sessions_title("a-rather-long-workspace-name", 32, true).to_string();
    assert!(narrow.starts_with(" S · "), "{narrow:?}");
    assert!(narrow.contains('…'), "{narrow:?}");
    assert!(narrow.chars().count() <= usize::from(pane_title_content_width(32, true)));
}

#[test]
fn actual_sessions_renderer_keeps_actions_and_row_shapes_across_widths() {
    use mj_core::config::SessionsSide;

    for (width, expected_sidebar) in [(80, 40), (120, 40), (180, 60)] {
        for side in [SessionsSide::Left, SessionsSide::Right] {
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
            let rendered = drawn(&mut dashboard, width, 40).join("\n");
            let sessions = dashboard.pane_areas.expect("dashboard panes")[0];
            assert_eq!(sessions.width, expected_sidebar);
            assert!(rendered.contains("Create"), "{rendered}");
            assert!(rendered.contains("Resume"), "{rendered}");
            assert!(rendered.contains("Q"), "{rendered}");
            assert!(
                dashboard
                    .session_row_areas
                    .iter()
                    .all(|(_, area)| area.height == 4)
            );
        }
    }

    let mut minimized = dashboard_with_session(running_session());
    minimized.set_pane_size(SupportPane::Sessions, PaneSize::Minimized);
    let rendered = drawn(&mut minimized, 80, 40).join("\n");
    let sessions = minimized.pane_areas.expect("minimized panes")[0];
    assert_eq!(sessions.width, 20);
    assert!(rendered.contains("Create"), "{rendered}");
    assert!(rendered.contains("Resume"), "{rendered}");
    assert!(
        minimized
            .session_row_areas
            .iter()
            .all(|(_, area)| area.height == 2)
    );
}

#[test]
fn minimized_running_clocks_leave_the_queue_count_visible() {
    for elapsed in [45, 6_000, 172_800] {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.set_pane_size(SupportPane::Sessions, PaneSize::Minimized);
        let detail = dashboard.session_details.get_mut("session-1").unwrap();
        detail.current_turn_started_at = Some(now_seconds().saturating_sub(elapsed));
        detail.pending_elicitations.clear();
        detail.queued_prompts.push(mj_core::relay::QueuedPrompt {
            id: "queued-1".into(),
            text: "next task".into(),
            attachments: Vec::new(),
            created_at_ms: 1,
        });
        let mut terminal = Terminal::new(TestBackend::new(80, 30)).unwrap();
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .unwrap();
        let (_, row) = dashboard.session_row_areas[0];
        let status = (row.x..row.right())
            .map(|x| terminal.backend().buffer()[(x, row.y + 1)].symbol())
            .collect::<String>();
        assert!(status.contains("Working "), "{status:?}");
        assert!(status.contains(" Q1"), "{status:?}");
        assert!(!status.contains('…'), "{status:?}");
    }
}

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
                assert_eq!(cell.bg, theme::palette().surface_raised);
                assert_eq!(cell.fg, theme::palette().accent);
                assert!(cell.modifier.contains(Modifier::BOLD));
            } else {
                assert_eq!(cell.bg, theme::palette().surface);
                assert_eq!(cell.fg, theme::palette().muted);
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

#[test]
fn unavailable_sessions_maximum_is_hidden_and_returns_on_resize() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.set_pane_size(SupportPane::Sessions, PaneSize::Maximized);

    let mut narrow = Terminal::new(TestBackend::new(80, 40)).expect("narrow terminal");
    narrow
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw narrow dashboard");
    assert_eq!(
        dashboard.pane_size(SupportPane::Sessions),
        PaneSize::Maximized
    );
    assert!(!dashboard.pane_maximize_enabled(SupportPane::Sessions));
    assert_eq!(dashboard.pane_areas.expect("pane areas")[0].width, 40);
    let session_controls = dashboard
        .pane_size_control_areas
        .iter()
        .copied()
        .filter(|(pane, _, _)| *pane == SupportPane::Sessions)
        .collect::<Vec<_>>();
    assert_eq!(session_controls.len(), 2);
    assert!(
        session_controls
            .iter()
            .all(|(_, size, _)| *size != PaneSize::Maximized)
    );
    let sessions_area = dashboard.pane_areas.expect("pane areas")[0];
    let session_title = (sessions_area.x..sessions_area.right())
        .map(|x| narrow.backend().buffer()[(x, sessions_area.y)].symbol())
        .collect::<String>();
    assert!(!session_title.contains('□'), "{session_title:?}");
    assert_eq!(session_controls[1].2.x, session_controls[0].2.x + 4);
    assert_eq!(session_controls[1].2.right(), sessions_area.right() - 1);
    let standard = session_controls
        .iter()
        .find(|(_, size, _)| *size == PaneSize::Standard)
        .map(|(_, _, area)| *area)
        .expect("visible Standard control");
    let cell = &narrow.backend().buffer()[(standard.x + 1, standard.y)];
    assert_eq!(cell.bg, theme::palette().surface_raised);
    assert_eq!(cell.fg, theme::palette().accent);
    assert!(cell.modifier.contains(Modifier::BOLD));

    dashboard.focus_sessions();
    dashboard.set_pane_size(SupportPane::Sessions, PaneSize::Standard);
    chord(&mut dashboard, crate::CommandId::CycleFocusedPaneSize);
    assert_eq!(
        dashboard.pane_size(SupportPane::Sessions),
        PaneSize::Minimized,
        "the pane-size chord skips unavailable Maximized from Standard"
    );

    dashboard.set_pane_size(SupportPane::Sessions, PaneSize::Maximized);
    let mut wide = Terminal::new(TestBackend::new(120, 40)).expect("wide terminal");
    wide.draw(|frame| render(frame, &mut dashboard))
        .expect("draw wide dashboard");
    assert_eq!(
        dashboard.pane_size(SupportPane::Sessions),
        PaneSize::Maximized
    );
    assert!(dashboard.pane_maximize_enabled(SupportPane::Sessions));
    assert_eq!(dashboard.pane_areas.expect("pane areas")[0].width, 60);
    let maximum = dashboard
        .pane_size_control_areas
        .iter()
        .find(|(pane, size, _)| *pane == SupportPane::Sessions && *size == PaneSize::Maximized)
        .map(|(_, _, area)| *area)
        .expect("visible Maximized control");
    let sessions_area = dashboard.pane_areas.expect("pane areas")[0];
    assert_eq!(maximum.right(), sessions_area.right() - 1);
    let cell = &wide.backend().buffer()[(maximum.x + 1, maximum.y)];
    assert_eq!(cell.bg, theme::palette().surface_raised);
    assert_eq!(cell.fg, theme::palette().accent);
    assert!(cell.modifier.contains(Modifier::BOLD));
}

#[test]
fn sessions_sidebar_layout_uses_fractional_sizes_and_caps() {
    for (width, standard, maximized) in
        [(80, 40, 40), (120, 40, 60), (240, 80, 100), (480, 80, 100)]
    {
        let mut standard_dashboard = dashboard_with_session(running_session());
        standard_dashboard.set_pane_size(SupportPane::Sessions, PaneSize::Standard);
        drawn(&mut standard_dashboard, width, 40);
        assert_eq!(
            standard_dashboard.pane_areas.expect("standard pane areas")[0].width,
            standard,
            "standard width at {width} columns"
        );

        let mut maximized_dashboard = dashboard_with_session(running_session());
        maximized_dashboard.set_pane_size(SupportPane::Sessions, PaneSize::Maximized);
        drawn(&mut maximized_dashboard, width, 40);
        assert_eq!(
            maximized_dashboard
                .pane_areas
                .expect("maximized pane areas")[0]
                .width,
            maximized,
            "maximized width at {width} columns"
        );
    }
}

#[test]
fn maximize_availability_tracks_content_and_works_from_minimized() {
    let mut dashboard = dashboard_with_session(running_session());
    drawn(&mut dashboard, 120, 40);
    assert!(!dashboard.pane_maximize_enabled(SupportPane::Quota));

    let profile = dashboard.config.profiles.values().next().unwrap().clone();
    for index in 0..20 {
        dashboard
            .config
            .profiles
            .insert(format!("extra-{index}"), profile.clone());
    }
    drawn(&mut dashboard, 120, 40);
    assert!(dashboard.pane_maximize_enabled(SupportPane::Quota));
    let standard_height = dashboard.pane_areas.unwrap()[2].height;
    dashboard.set_pane_size(SupportPane::Quota, PaneSize::Minimized);
    drawn(&mut dashboard, 120, 40);
    assert!(dashboard.pane_maximize_enabled(SupportPane::Quota));
    let maximum = dashboard
        .pane_size_control_areas
        .iter()
        .find(|(pane, size, _)| *pane == SupportPane::Quota && *size == PaneSize::Maximized)
        .unwrap()
        .2;
    let focus = dashboard.focus();
    dashboard.handle_mouse(mouse_at_row(
        MouseEventKind::Down(MouseButton::Left),
        maximum,
        0,
    ));
    assert_eq!(dashboard.pane_size(SupportPane::Quota), PaneSize::Maximized);
    assert_eq!(dashboard.focus(), focus);
    drawn(&mut dashboard, 120, 40);
    assert!(dashboard.pane_areas.unwrap()[2].height > standard_height);

    drawn(&mut dashboard, 120, 120);
    assert!(!dashboard.pane_maximize_enabled(SupportPane::Quota));
}

#[test]
fn the_pane_preset_compacts_sessions_and_returns_space_to_the_conversation() {
    for (height, expected_sessions_height) in [(32, 28), (44, 40)] {
        let mut dashboard = minimized_sessions_dashboard(3, 2);
        dashboard
            .restore_pane_sizes(crate::PaneSizes::default())
            .unwrap();
        dashboard.focus_sessions();
        let standard = drawn(&mut dashboard, 120, height).join("\n");
        let standard_panes = dashboard.pane_areas.unwrap();
        let standard_transcript = dashboard.focused_transcript_area().unwrap();
        assert!(standard.contains("Idle"), "{standard}");
        assert!(standard.contains("codex-1"), "{standard}");

        chord(&mut dashboard, crate::CommandId::TogglePanePreset);
        let compact = drawn(&mut dashboard, 120, height).join("\n");
        let compact_panes = dashboard.pane_areas.unwrap();
        assert_eq!(compact_panes[0].height, expected_sessions_height);
        assert_eq!(compact_panes[1].height, 1);
        assert_eq!(compact_panes[2].height, 1);
        assert!(compact_panes[0].width < standard_panes[0].width);
        assert!(dashboard.focused_transcript_area().unwrap().height > standard_transcript.height);
        assert!(!compact.contains("You:"), "{compact}");
        assert!(!compact.contains("Agent:"), "{compact}");
        assert!(!dashboard.session_row_areas.is_empty());

        chord(&mut dashboard, crate::CommandId::TogglePanePreset);
        drawn(&mut dashboard, 120, height);
        assert_eq!(dashboard.pane_areas.unwrap(), standard_panes);
        assert_eq!(
            dashboard.focused_transcript_area().unwrap(),
            standard_transcript
        );
    }
}

#[test]
fn tab_focus_never_changes_band_geometry() {
    let mut dashboard = minimized_sessions_dashboard(3, 2);
    for pane in [
        SupportPane::Sessions,
        SupportPane::Targets,
        SupportPane::Quota,
    ] {
        dashboard.set_pane_size(pane, PaneSize::Standard);
    }
    drawn(&mut dashboard, 120, 44);
    let expected = (
        dashboard.pane_areas,
        dashboard.focused_transcript_area(),
        dashboard.focused_prompt_area(),
    );
    for _ in 0..4 {
        dashboard.cycle_focus(false);
        drawn(&mut dashboard, 120, 44);
        assert_eq!(
            (
                dashboard.pane_areas,
                dashboard.focused_transcript_area(),
                dashboard.focused_prompt_area(),
            ),
            expected
        );
    }
}

#[test]
fn sessions_in_an_expanded_project_have_a_blank_row_and_only_the_caret_marks_selection() {
    let mut first = running_session();
    first.id = "session-first".into();
    first.project_directory = Some("/projects/shared".into());
    first.session_title_override = Some("First session".into());
    let mut second = running_session();
    second.id = "session-second".into();
    second.project_directory = Some("/projects/shared".into());
    second.session_title_override = Some("Second session".into());
    second.created_at = "2026-08-10T00:00:00Z".into();
    let state = State {
        subagents: Default::default(),
        version: STATE_VERSION,
        sessions: BTreeMap::from([(first.id.clone(), first), (second.id.clone(), second)]),
        mount_history: BTreeMap::new(),
        container_sizes: BTreeMap::new(),
    };
    let mut dashboard = DashboardState::new(config(), state, BTreeMap::new());
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("terminal");

    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw dashboard");

    let buffer = terminal.backend().buffer();
    let lines = buffer_lines(buffer);
    let first_y = lines
        .iter()
        .position(|line| line.contains("First session"))
        .expect("first session row") as u16;
    let second_y = lines
        .iter()
        .position(|line| line.contains("Second session"))
        .expect("second session row") as u16;
    assert!(
        (first_y..first_y + 4).all(|y| {
            (dashboard.pane_areas.expect("pane areas")[0].x + 1
                ..dashboard.pane_areas.expect("pane areas")[0].right() - 1)
                .all(|x| buffer[(x, y)].bg != theme::palette().muted)
        }),
        "selection must not paint a background"
    );
    assert!(lines[first_y as usize].contains("› "));
    assert!(lines[first_y as usize].contains("First session"));
    assert_eq!(
        second_y,
        first_y + 5,
        "sessions in an expanded project have one blank row between them"
    );
    let pane = dashboard.pane_areas.expect("pane areas")[0];
    assert!(
        (pane.x + 1..pane.right() - 1).all(|x| buffer[(x, first_y + 4)].symbol().trim().is_empty())
    );
}

#[test]
fn missing_configuration_renders_repair_status_and_clears_after_restore() {
    let session = running_session();
    let bundle_id = session.bundle_id.clone();
    let mut dashboard = dashboard_with_session(session);
    let bundle = dashboard.config.bundles.remove(&bundle_id).unwrap();
    let broken = drawn(&mut dashboard, 120, 44).join("\n");
    assert!(broken.contains("Needs config repair"), "{broken}");
    dashboard.config.bundles.insert(bundle_id, bundle);
    let repaired = drawn(&mut dashboard, 120, 44).join("\n");
    assert!(!repaired.contains("Needs config repair"), "{repaired}");
}

#[test]
fn session_transitions_preserve_the_blank_row_before_the_next_session() {
    let mut first = running_session();
    first.project_directory = Some("/projects/shared".into());
    first.session_title_override = Some("First session".into());
    let mut second = first.clone();
    second.id = "session-second".into();
    second.session_title_override = Some("Second session".into());
    second.created_at = "2026-08-10T00:00:00Z".into();
    let mut dashboard = dashboard_with_session(first.clone());
    dashboard.state.sessions.insert(second.id.clone(), second);
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("terminal");

    for kind in [
        Some(SessionOperationKind::Launching),
        Some(SessionOperationKind::Resuming),
        Some(SessionOperationKind::Moving),
        Some(SessionOperationKind::Stopping),
        Some(SessionOperationKind::Destroying),
        None,
    ] {
        dashboard.session_operations.clear();
        if let Some(kind) = kind {
            dashboard
                .session_operations
                .insert(first.id.clone(), operation(kind, None));
        } else {
            let session = dashboard.state.sessions.get_mut(&first.id).unwrap();
            session.state = SessionState::Closing;
            session.last_error = Some("checkpoint failed".into());
        }
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw session transition");
        let buffer = terminal.backend().buffer();
        let lines = buffer_lines(buffer);
        let first_y = lines
            .iter()
            // The row leads with the session name; the ellipsis action marks
            // it as a Sessions-pane row rather than the conversation's own
            // transition notice.
            .position(|line| line.contains("First session") && line.contains('⋯'))
            .expect("transition row");
        let second_y = lines
            .iter()
            .position(|line| line.contains("Second session"))
            .expect("following session row");
        assert_eq!(second_y, first_y + 2, "{kind:?}: {lines:#?}");
        assert!(
            (dashboard.pane_areas.expect("pane areas")[0].x + 1
                ..dashboard.pane_areas.expect("pane areas")[0].right() - 1)
                .all(|x| { buffer[(x, (first_y + 1) as u16)].symbol().trim().is_empty() }),
            "{kind:?}: the separator must be blank"
        );
    }
}

#[test]
fn project_groups_have_one_blank_row_between_them() {
    let mut first = running_session();
    first.id = "session-alpha".into();
    first.project_directory = Some("/projects/alpha".into());
    let mut second = running_session();
    second.id = "session-beta".into();
    second.project_directory = Some("/projects/beta".into());
    second.created_at = "2026-08-10T00:00:00Z".into();
    let state = State {
        subagents: Default::default(),
        version: STATE_VERSION,
        sessions: BTreeMap::from([(first.id.clone(), first), (second.id.clone(), second)]),
        mount_history: BTreeMap::new(),
        container_sizes: BTreeMap::new(),
    };
    let mut dashboard = DashboardState::new(config(), state, BTreeMap::new());
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("terminal");

    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw dashboard");

    let buffer = terminal.backend().buffer();
    let lines = buffer_lines(buffer);
    let first_y = lines
        .iter()
        .position(|line| line.contains("› ") && line.contains("ACP pretty name"))
        .expect("first session row") as u16;
    let second_heading_y = lines
        .iter()
        .position(|line| line.contains("beta"))
        .expect("second project heading") as u16;
    let first_bottom = first_y + 4;
    assert_eq!(second_heading_y, first_bottom + 1);
    let pane = dashboard.pane_areas.expect("pane areas")[0];
    assert!(
        (pane.x + 1..pane.right() - 1)
            .all(|x| buffer[(x, first_bottom)].symbol().trim().is_empty())
    );
}

#[test]
fn project_hotkeys_collapse_and_expand_groups_independently() {
    let mut first = running_session();
    first.id = "session-alpha".into();
    first.project_directory = Some("/projects/alpha".into());
    let mut second = running_session();
    second.id = "session-beta".into();
    second.project_directory = Some("/projects/beta".into());
    second.created_at = "2026-08-10T00:00:00Z".into();
    let state = State {
        subagents: Default::default(),
        version: STATE_VERSION,
        sessions: BTreeMap::from([(first.id.clone(), first), (second.id.clone(), second)]),
        mount_history: BTreeMap::new(),
        container_sizes: BTreeMap::new(),
    };
    let mut dashboard = DashboardState::new(config(), state, BTreeMap::new());
    dashboard.apply_materialized_session(&materialized_session_for(
        "session-alpha",
        numbered_conversation(1),
    ));
    dashboard.apply_materialized_session(&materialized_session_for(
        "session-beta",
        vec![
            transcript_item(
                1,
                TranscriptBody::User {
                    content: vec![serde_json::json!({"type":"text","text":"beta question"})],
                },
            ),
            agent_message(2, "beta answer"),
        ],
    ));
    let mut terminal = Terminal::new(TestBackend::new(120, 44)).expect("terminal");
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw first project");
    let first_draw = buffer_lines(terminal.backend().buffer()).join("\n");
    // Every project starts expanded, so both groups show their full form.
    assert!(first_draw.contains("[1] alpha"));
    assert!(first_draw.contains("[2] beta"));
    assert!(first_draw.contains("answer 0"));
    assert!(first_draw.contains("beta answer"));

    // The numbered hotkey collapses only its own project.
    assert_eq!(
        dashboard.handle_key(crate::test_support::key(KeyCode::Char('2'))),
        DashboardAction::None
    );
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw with beta collapsed");
    let second_draw = buffer_lines(terminal.backend().buffer()).join("\n");
    assert!(second_draw.contains("answer 0"), "{second_draw}");
    assert!(!second_draw.contains("beta answer"), "{second_draw}");
    assert!(
        second_draw.contains("[2] beta") && second_draw.contains("Working"),
        "the collapsed group keeps a two-line summary per session: {second_draw}"
    );

    // Collapsing alpha too leaves both groups collapsed at once.
    dashboard.handle_key(crate::test_support::key(KeyCode::Char('1')));
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw with both collapsed");
    let third_draw = buffer_lines(terminal.backend().buffer()).join("\n");
    assert!(!third_draw.contains("answer 0"), "{third_draw}");
    assert!(!third_draw.contains("beta answer"), "{third_draw}");

    // And the hotkey is a toggle, so pressing it again brings beta back.
    dashboard.handle_key(crate::test_support::key(KeyCode::Char('2')));
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw with beta expanded again");
    let fourth_draw = buffer_lines(terminal.backend().buffer()).join("\n");
    assert!(fourth_draw.contains("beta answer"), "{fourth_draw}");
    assert!(!fourth_draw.contains("answer 0"), "{fourth_draw}");
}

#[test]
fn collapsed_duplicate_targets_are_numbered_within_their_project() {
    let mut alpha = running_session();
    alpha.id = "session-alpha".into();
    alpha.project_directory = Some("/projects/alpha".into());
    let mut beta_first = running_session();
    beta_first.id = "session-beta-first".into();
    beta_first.project_directory = Some("/projects/beta".into());
    beta_first.created_at = "2026-08-10T00:00:00Z".into();
    let mut beta_second = beta_first.clone();
    beta_second.id = "session-beta-second".into();
    beta_second.created_at = "2026-08-11T00:00:00Z".into();
    let state = State {
        subagents: Default::default(),
        version: STATE_VERSION,
        sessions: [alpha, beta_first, beta_second]
            .into_iter()
            .map(|session| (session.id.clone(), session))
            .collect(),
        mount_history: BTreeMap::new(),
        container_sizes: BTreeMap::new(),
    };
    let mut dashboard = DashboardState::new(config(), state, BTreeMap::new());
    dashboard.apply_materialized_session(&materialized_session_for(
        "session-beta-first",
        vec![agent_message(1, "first tail")],
    ));
    dashboard.apply_materialized_session(&materialized_session_for(
        "session-beta-second",
        vec![agent_message(1, "second tail")],
    ));
    for id in ["session-beta-first", "session-beta-second"] {
        dashboard
            .session_details
            .get_mut(id)
            .unwrap()
            .current_turn_started_at = Some(now_seconds().saturating_sub(10));
    }
    dashboard.focus_sessions();
    let mut terminal = Terminal::new(TestBackend::new(120, 44)).expect("terminal");
    // Collapse the beta project so its sessions draw their compact form,
    // which is where duplicate targets need their numbering.
    dashboard.handle_key(crate::test_support::key(KeyCode::Char('2')));

    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw collapsed duplicate targets");
    let rendered = buffer_lines(terminal.backend().buffer()).join("\n");

    assert_eq!(
        rendered
            .lines()
            .filter(|line| line.contains("podman [1]"))
            .count(),
        1,
        "{rendered}"
    );
    assert_eq!(
        rendered
            .lines()
            .filter(|line| line.contains("podman [2]"))
            .count(),
        1,
        "{rendered}"
    );
    assert!(!rendered.contains("first tail"), "{rendered}");
    assert!(!rendered.contains("second tail"), "{rendered}");
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

    let restarted_idle = SessionDetail {
        unread_session_restarts: 1,
        ..SessionDetail::default()
    };
    assert_eq!(
        band(Some(&restarted_idle), false, SessionState::Running),
        theme::palette().session_attention,
        "an unread restart is unread activity"
    );

    let restarted_running = SessionDetail {
        current_turn_started_at: Some(1),
        unread_session_restarts: 1,
        ..SessionDetail::default()
    };
    assert_eq!(
        band(Some(&restarted_running), false, SessionState::Running),
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

#[test]
fn marking_all_read_removes_the_unread_tint_from_an_idle_session() {
    let mut dashboard = dashboard_with_session(running_session());
    apply_materialized_transcript(&mut dashboard, vec![agent_message(4, "unread response")]);
    let detail = dashboard.session_details.get_mut("session-1").unwrap();
    detail.current_turn_started_at = None;
    detail.activity = mj_client::usage_format::SessionActivity::default();
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("terminal");
    let mut row_color = |dashboard: &mut DashboardState| {
        terminal
            .draw(|frame| render(frame, dashboard))
            .expect("draw dashboard");
        let buffer = terminal.backend().buffer();
        let lines = buffer_lines(buffer);
        let row = lines
            .iter()
            .position(|line| line.contains("podman"))
            .expect("session row");
        buffer[(cell_column(&lines[row], "podman"), row as u16)].fg
    };
    assert_eq!(
        row_color(&mut dashboard),
        theme::palette().session_attention,
        "an answer nobody has read wants a person"
    );
    assert_eq!(
        chord(&mut dashboard, crate::CommandId::MarkAllRead),
        DashboardAction::MarkAllRead {
            receipts: vec![("session-1".into(), 4)]
        }
    );
    assert_eq!(row_color(&mut dashboard), theme::palette().session_idle);
}

#[test]
fn dashboard_replaces_too_short_layout_with_required_height() {
    let mut dashboard = DashboardState::new(config(), State::default(), BTreeMap::new());
    let mut terminal = Terminal::new(TestBackend::new(120, 10)).expect("terminal");
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw short dashboard");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("Terminal too small"));
    assert!(
        rendered.contains("at least 13 rows (currently 10)"),
        "{rendered:?}"
    );

    let mut terminal = Terminal::new(TestBackend::new(120, 16)).expect("terminal");
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw exact minimum dashboard");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(!rendered.contains("Terminal too small"));
    assert!(rendered.contains("Sessions"));
    assert!(rendered.contains('▁'));
    assert!(rendered.contains('▪'));
    assert!(rendered.contains('□'));
    assert!(rendered.contains("Quota"));
}

#[test]
fn dashboard_stacks_below_80_columns_and_gives_up_below_60() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.set_deployment_capacity_targets(vec![test_capacity_target()]);
    let mut terminal = Terminal::new(TestBackend::new(59, 30)).expect("terminal");
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw narrow dashboard");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("Terminal too small"));
    assert!(rendered.contains("Need at least 60 columns"));
    assert!(rendered.contains("Current width: 59"));

    // Between 60 and 79 columns the Sessions list stacks above the
    // conversation in its compact form, and Targets and Quota go below.
    let lines = drawn(&mut dashboard, 70, 30);
    assert!(
        !lines.join("\n").contains("Terminal too small"),
        "{lines:#?}"
    );
    let [sessions, targets, quota] = dashboard.pane_areas.expect("pane geometry");
    let conversation = dashboard.conversation_area.expect("conversation geometry");
    assert_eq!(sessions.width, 70, "{lines:#?}");
    assert_eq!(sessions.y, 3, "the workspace tabs sit above the list");
    assert_eq!(conversation.y, sessions.bottom(), "{lines:#?}");
    assert_eq!(conversation.width, 70);
    assert_eq!(targets.y, conversation.bottom());
    assert_eq!(targets.width, 70);
    assert_eq!(quota.y, targets.bottom());
    assert!(
        dashboard.sessions_minimized(),
        "the stacked list is compact"
    );
    assert!(
        lines.iter().any(|line| line.contains("ACP pretty name")),
        "{lines:#?}"
    );
    assert!(
        lines.iter().any(|line| line.contains("Conversation")),
        "{lines:#?}"
    );
    assert!(lines.last().unwrap().contains("ctrl+b"), "{lines:#?}");

    // From 80 columns the sidebar sits beside the conversation again.
    let lines = drawn(&mut dashboard, 80, 30);
    let [sessions, _, _] = dashboard.pane_areas.expect("pane geometry");
    let conversation = dashboard.conversation_area.expect("conversation geometry");
    assert!(sessions.width < 80, "{lines:#?}");
    assert_eq!(conversation.x, sessions.right());
    assert!(!dashboard.sessions_minimized());
}

#[test]
fn new_session_picker_keeps_choices_and_controls_visible_at_minimum_width() {
    let mut dashboard = DashboardState::new(config(), State::default(), BTreeMap::new());
    assert_eq!(
        open_new_session_wizard(&mut dashboard),
        DashboardAction::None
    );
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw minimum-width new-session picker");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();

    assert!(rendered.contains("claude-1"));
    assert!(rendered.contains("codex-2"));
    assert!(rendered.contains("Cancel"));
    assert!(rendered.contains("Next"));
}

#[test]
fn the_footer_is_one_row_that_a_notice_takes_over() {
    let mut dashboard = DashboardState::new(config(), State::default(), BTreeMap::new());
    dashboard.set_workspace_name("personal".into());
    dashboard.set_notice("Transient dashboard message");
    let mut terminal = Terminal::new(TestBackend::new(120, 24)).expect("terminal");
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw dashboard");
    let buffer = terminal.backend().buffer();
    let line = |y| {
        (buffer.area.x..buffer.area.right())
            .map(|x| buffer[(x, y)].symbol())
            .collect::<String>()
    };

    // The workspace pane precedes Sessions, while the
    // transcript keeps the same upper band height.
    let sessions = dashboard.pane_areas.expect("pane geometry")[0];
    assert!(
        line(sessions.y).contains("Sessions"),
        "{:?}",
        line(sessions.y)
    );
    assert_eq!(dashboard.workspace_name, "personal");
    assert!(!line(sessions.y).contains("ACP sessions"));
    // The footer is one row: a notice replaces the hints while one is
    // showing, so the row costs one line whichever surface drew it.
    assert!(
        line(buffer.area.bottom() - 1).contains("Transient dashboard message"),
        "{:?}",
        line(buffer.area.bottom() - 1)
    );
    dashboard.notices.clear();
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw dashboard");
    let buffer = terminal.backend().buffer();
    let hotkeys = (buffer.area.x..buffer.area.right())
        .map(|x| buffer[(x, buffer.area.bottom() - 1)].symbol())
        .collect::<String>();
    assert!(hotkeys.contains("ctrl+b then: c create"), "{hotkeys:?}");
    assert!(hotkeys.contains("a read"), "{hotkeys:?}");
    assert!(!hotkeys.contains("[S]ort"));
}

/// The footer is the only place a beginner learns what the cancel chord does,
/// so it must name the operation it would cancel — and must not offer the key at
/// all while there is nothing in flight.
#[test]
fn footer_lists_cancel_only_while_an_operation_is_in_flight() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.focus_sessions();
    assert!(
        !combined_footer_text(&dashboard, 200).contains("cancel"),
        "{}",
        combined_footer_text(&dashboard, 200)
    );

    dashboard.begin_session_operation_at(
        "session-1".into(),
        SessionOperationKind::Launching,
        None,
        1_000,
    );
    let footer = combined_footer_text(&dashboard, 200);
    assert!(footer.contains("shift+c cancel launch"), "{footer}");

    dashboard.finish_session_operation("session-1");
    assert!(
        !combined_footer_text(&dashboard, 200).contains("cancel"),
        "{}",
        combined_footer_text(&dashboard, 200)
    );
}

/// Help is the one hint that is worth more than any other, so it survives
/// every focus and every width squeeze.
#[test]
fn footer_ends_with_help_at_every_focus() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.set_deployment_capacity_targets(vec![test_capacity_target()]);
    for focus in [Focus::Sessions, Focus::Targets, Focus::Quota, Focus::Prompt] {
        dashboard.focus = focus;
        let footer = combined_footer_text(&dashboard, 200);
        assert!(footer.ends_with("? keys"), "{focus:?}: {footer}");
    }
}

/// A hint cut in half names a key that does not exist. Narrow terminals
/// therefore lose whole hints from the right, and never part of one.
#[test]
fn footer_drops_whole_hints_when_the_width_runs_out() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.focus_sessions();
    let full = combined_footer_text(&dashboard, 200);
    assert!(full.chars().count() > 40, "{full}");

    for width in 0_u16..=80 {
        let footer = combined_footer_text(&dashboard, width);
        assert!(Line::raw(footer.as_str()).width() <= usize::from(width));
        if width >= 6 {
            assert!(footer.ends_with("? keys"), "{width}: {footer}");
        }
        if width >= 20 {
            assert!(footer.contains(": palette"), "{width}: {footer}");
        }
        // Every hint that survived is a whole hint of the full text.
        for hint in footer_hints(&footer) {
            assert!(
                footer_hints(&full).contains(&hint),
                "{width}: {hint:?} is not a whole hint of {full:?}"
            );
        }
    }
}

/// A half-typed chord owns the footer: the reader needs the way out of it and
/// the key that lists the rest, not the hints they are part-way through.
#[test]
fn the_footer_shows_the_prefix_banner_while_a_chord_is_pending() {
    for focus in [Focus::Sessions, Focus::Prompt] {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus = focus;
        assert_eq!(
            dashboard.route_bound_key(&crate::test_support::prefix_key()),
            crate::KeyRoute::Consumed
        );
        assert!(dashboard.prefix_pending(), "{focus:?}");
        let lines = drawn(&mut dashboard, 120, 40);
        let footer = lines.last().expect("the footer row");
        assert!(footer.contains("PREFIX"), "{focus:?}: {footer}");
        assert!(
            footer.contains("esc cancel · ctrl+b send · ? keys"),
            "{focus:?}: {footer}"
        );
        assert!(!footer.contains("detach"), "{focus:?}: {footer}");

        // Cancelling puts the hints back.
        dashboard.cancel_prefix();
        let lines = drawn(&mut dashboard, 120, 40);
        assert!(
            lines.last().expect("the footer row").contains("q detach"),
            "{focus:?}"
        );
    }
}

/// The letters in the chord group mean nothing without the key that starts
/// them, so the group leads with whichever prefix is configured.
#[test]
fn footer_chord_group_starts_with_the_live_prefix() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.focus_sessions();
    let mut config = config();
    config.keys.prefix = "ctrl+a".to_owned();
    dashboard.set_config(config);
    let footer = combined_footer_text(&dashboard, 200);
    assert!(footer.contains("│ ctrl+a then: c create"), "{footer}");
    assert!(!footer.contains("ctrl+b"), "{footer}");
}

/// Every hint in the footer, whichever separator it sits between.
fn footer_hints(footer: &str) -> Vec<String> {
    footer
        .split(theme::footer_group_separator())
        .flat_map(|group| group.split(theme::footer_separator()))
        .filter(|hint| !hint.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

/// The row is read left to right by someone hunting one key, so the kinds of
/// key never swap places: what this pane answers, then the keys that follow
/// the prefix, in one rank order. The chord group leads with the prefix
/// itself, because the letters after it mean nothing without it.
#[test]
fn footer_groups_pane_keys_then_prefix_chords_in_rank_order() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.focus_sessions();
    assert_eq!(
        combined_footer_text(&dashboard, 200),
        "Enter open · / search · Tab pane │ ctrl+b then: c create · g resume · a read · shift+z size \
         · b panes · q detach · u web · shift+r refresh · s settings · t rendering · : palette \
         · ? keys"
    );

    // The cancel chord takes its fixed place before detach, and only while
    // there is something to cancel.
    dashboard.set_deployment_capacity_targets(vec![test_capacity_target()]);
    dashboard.begin_session_operation_at(
        "session-1".into(),
        SessionOperationKind::Launching,
        None,
        1_000,
    );
    let footer = combined_footer_text(&dashboard, 200);
    assert!(footer.contains("│ ctrl+b then: c create"), "{footer}");
    assert!(
        footer.contains("b panes · shift+c cancel launch · q detach"),
        "{footer}"
    );
}

/// Pane hints give way before the prefix chords, and help and palette remain
/// visible after every other chord has been dropped.
#[test]
fn footer_drops_pane_hints_before_chord_hints_and_keeps_help_longest() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.set_deployment_capacity_targets(vec![test_capacity_target()]);
    dashboard.focus_sessions();

    let full = combined_footer_text(&dashboard, 200);
    assert!(
        footer_hints(&full).contains(&"Enter open".to_owned()),
        "{full}"
    );

    // Narrow enough to lose the pane group, wide enough to keep chords.
    let squeezed = combined_footer_text(&dashboard, 90);
    assert!(!squeezed.contains("Enter open"), "{squeezed}");
    assert!(squeezed.contains("ctrl+b then: c create"), "{squeezed}");
    assert!(squeezed.ends_with(": palette · ? keys"), "{squeezed}");

    assert_eq!(combined_footer_text(&dashboard, 20), ": palette · ? keys");
    assert_eq!(combined_footer_text(&dashboard, 6), "? keys");
    assert!(combined_footer_text(&dashboard, 5).is_empty());
    assert!(combined_footer_text(&dashboard, 0).is_empty());
}

/// The footer is generated from the same table the keyboard reads, so
/// pressing what it names must do what it says. This is the test that
/// makes the registry worth having.
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

/// Expanded output keeps the transcript's rich formatting without adding
/// a second role rail.
#[test]
fn an_expanded_agent_excerpt_flattens_newlines_without_a_transcript_gutter() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.focus_sessions();
    apply_materialized_transcript(
        &mut dashboard,
        vec![
            transcript_item(
                1,
                TranscriptBody::User {
                    content: vec![serde_json::json!({
                        "type": "text",
                        "text": "summarize the README"
                    })],
                },
            ),
            agent_message(2, "reliability\nreply: summarize the README"),
        ],
    );

    let lines = drawn(&mut dashboard, 120, 44);
    let agent = lines
        .iter()
        .find(|line| line.contains("reliability reply"))
        .expect("the agent excerpt row");
    // The rounded pane contributes one edge on each side; the excerpt
    // must not duplicate the conversation's interior rail.
    let session_cell = agent.split("││").next().unwrap_or(agent.as_str());
    assert!(
        !session_cell.trim_matches('\u{2502}').contains('\u{2502}'),
        "the excerpt carries no transcript rail: {agent:?}"
    );
}

/// The empty band has two causes and they need different advice. Telling
/// someone there is no live session while the pane above lists one is a
/// plain lie.
#[test]
fn the_empty_prompt_distinguishes_no_session_from_no_conversation() {
    let mut empty = DashboardState::new(config(), State::default(), BTreeMap::new());
    let lines = drawn(&mut empty, 120, 44).join("\n");
    assert!(lines.contains("No live session"), "{lines}");
    assert!(
        lines.contains("ctrl+b c to create a session or ctrl+b g to resume one"),
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

/// Attaching is asynchronous, and until it lands the chat still loaded
/// belongs to the row the selection has moved off. The transcript says
/// the session is opening, and the prompt band is the real composer
/// parked until the attach lands.
#[test]
fn an_attach_in_flight_draws_an_opening_transcript_and_the_standby_composer() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.set_opening_session(Some("session-1"));
    dashboard.focus_prompt();

    let lines = drawn(&mut dashboard, 120, 44).join("\n");
    // The transcript hero says the conversation is opening...
    assert!(
        lines.contains("Bringing your conversation into focus…"),
        "{lines}"
    );
    // ...while the prompt band is the real composer, parked.
    assert!(
        lines.contains("Type a draft · sending opens when the session is live"),
        "{lines}"
    );
    assert!(!lines.contains("No conversation open"), "{lines}");
}

/// The band order is the whole point of the surface: everything is on one
/// screen, in one arrangement, at every size it draws at.
#[test]
fn the_session_sidebar_spans_all_other_panes_on_either_side() {
    for side in [
        mj_core::config::SessionsSide::Left,
        mj_core::config::SessionsSide::Right,
    ] {
        for (width, height) in [(140, 32), (80, 20), (80, 16)] {
            let mut dashboard = dashboard_with_session(running_session());
            dashboard.config.sessions_side = side;
            let lines = drawn(&mut dashboard, width, height);
            let [sessions, targets, quota] = dashboard.pane_areas.unwrap();
            let transcript = dashboard.focused_transcript_area().unwrap();
            let prompt = dashboard.focused_prompt_area().unwrap();
            assert_eq!(sessions.y, 3);
            assert!(sessions.height > 0);
            assert!(sessions.width > 0 && transcript.width > 0);
            for pane in [transcript, prompt, targets, quota] {
                assert!(pane.height > 0);
            }
            for pane in [transcript, prompt] {
                if side == mj_core::config::SessionsSide::Left {
                    assert_eq!(sessions.right(), pane.x);
                } else {
                    assert_eq!(pane.right(), sessions.x);
                }
            }
            let support_in_content = targets.width == transcript.width;
            if support_in_content {
                if side == mj_core::config::SessionsSide::Left {
                    assert_eq!(sessions.right(), targets.x);
                } else {
                    assert_eq!(targets.right(), sessions.x);
                }
                assert_eq!(sessions.bottom(), quota.bottom());
            } else {
                assert_eq!(targets.x, 0);
                assert_eq!(targets.width, width);
                assert_eq!(sessions.bottom(), targets.y);
            }
            assert_eq!(transcript.bottom(), prompt.y);
            assert_eq!(prompt.bottom(), targets.y);
            assert_eq!(targets.bottom(), quota.y);
            assert!(!lines.last().unwrap().trim().is_empty());
        }
    }
}

/// Collapsing the support panes hands their freed rows to the transcript,
/// and the transcript also absorbs whatever the Sessions pane gives up (or
/// gives back) as it moves to its fixed mode-2 third — nothing appears or
/// vanishes, so the gesture is measurable rather than merely visible.
#[test]
fn minimizing_the_support_panes_gives_their_rows_to_the_transcript() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.set_deployment_capacity_targets(vec![test_capacity_target()]);
    dashboard.focus_prompt();
    /// Rows from the start of one band to the start of the next.
    fn band(lines: &[String], from: &str, to: &str) -> isize {
        let start = lines
            .iter()
            .position(|line| line.contains(from))
            .unwrap_or_else(|| panic!("missing {from}: {lines:#?}"));
        let end = lines
            .iter()
            .position(|line| line.contains(to))
            .unwrap_or_else(|| panic!("missing {to}: {lines:#?}"));
        (end - start) as isize
    }

    let before = drawn(&mut dashboard, 140, 44);
    minimize_all_panes(&mut dashboard);
    let after = drawn(&mut dashboard, 140, 44);

    // Every row the tables and the Sessions pane give up lands in the
    // transcript; the composer and footer are untouched.
    let tables_freed = (band(&before, "Targets", "Quota") - band(&after, "Targets", "Quota"))
        + (band(&before, "Quota", "q detach") - band(&after, "Quota", "q detach"));
    let sessions_freed = 0;
    let transcript_gain = band(&after, "Conversation", "No conversation open")
        - band(&before, "Conversation", "No conversation open");
    assert!(tables_freed > 0, "the tables gave up nothing");
    assert_eq!(transcript_gain, tables_freed + sessions_freed);
    // Each minimized pane really is one row.
    assert_eq!(band(&after, "Targets", "Quota"), 1);
    assert_eq!(band(&after, "Quota", "q detach"), 1);
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
    }
}

/// A profile quota with `remaining` percent of its weekly window left.
fn weekly_quota(profile_id: &str, remaining: u8) -> ProfileQuota {
    ProfileQuota {
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
        profile_id: profile_id.into(),
        harness: HarnessKind::Codex,
        windows: Vec::new(),
        extra: Some(API_LABEL.into()),
        error: None,
        refreshed_at_epoch_seconds: now_seconds(),
    }
}

/// Adds a usage-priced profile to the dashboard's configuration, since the
/// shared fixture only carries subscription profiles.
fn add_api_priced_profile(dashboard: &mut DashboardState) {
    dashboard.config.profiles.insert(
        "api-priced".into(),
        mj_core::config::HarnessProfile {
            enabled: true,
            context_window_bytes: None,
            guardian_review_model: None,
            kind: HarnessKind::Codex,
            home: std::path::PathBuf::from("/profiles/api-priced"),
            environment: BTreeMap::new(),
        },
    );
}

/// An agent that is idle but left a command running says so, in the wide
/// rows and in the minimized grid, from the one fact the daemon forwards.
#[test]
fn background_work_reaches_both_session_row_forms() {
    let started_at_ms = i64::try_from(mj_core::clock::epoch_seconds()).unwrap() * 1_000 - 2_616_000;
    let activity = mj_client::usage_format::SessionActivity {
        pursuing_goal: Default::default(),
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
#[test]
fn an_expanded_session_always_draws_four_rows() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.focus_sessions();

    let lines = drawn(&mut dashboard, 120, 44);
    let first = lines
        .iter()
        .position(|line| line.contains("ACP pretty name"))
        .expect("the session's name row");
    assert!(
        lines[first + 1].contains("podman"),
        "{:?}",
        lines[first + 1]
    );
    assert!(
        lines[first + 2].contains("No messages yet"),
        "{:?}",
        lines[first + 2]
    );
    // The fourth row is the second agent row, blank here because there is
    // only one line to show.
    assert!(
        lines[first + 3].trim_matches(['│', '║', ' ']).is_empty(),
        "{:?}",
        lines[first + 3]
    );
}

#[test]
fn runtime_review_activity_is_visible_on_an_unselected_session_row() {
    let mut first = running_session();
    first.id = "session-first".into();
    let mut second = running_session();
    second.id = "session-second".into();
    second.created_at = "2026-08-10T00:00:00Z".into();
    let mut dashboard = DashboardState::new(
        config(),
        State {
            subagents: Default::default(),
            version: mj_core::state::STATE_VERSION,
            sessions: BTreeMap::from([(first.id.clone(), first), (second.id.clone(), second)]),
            mount_history: BTreeMap::new(),
            container_sizes: BTreeMap::new(),
        },
        BTreeMap::new(),
    );
    dashboard.set_session_reviews([RuntimeReviewView {
        session_id: "session-second".into(),
        tier: mj_core::review::lanes::ReviewTier::Quick,
        phase: mj_core::review::driver::TurnReviewPhase::Running { roles: Vec::new() },
        roles: Vec::new(),
        status: "the reviewer is reading the change…".into(),
        verdict: None,
    }]);

    // Collapse the project so the test exercises the compact two-line
    // form; the reviewed session is intentionally unselected.
    dashboard.focus_sessions();
    dashboard.handle_key(crate::test_support::key(KeyCode::Char('1')));
    let rendered = drawn(&mut dashboard, 140, 44).join("\n");
    let review_line = rendered
        .lines()
        .find(|line| line.contains("Reviewing"))
        .expect("review activity on the unselected compact row");
    assert!(!review_line.contains("Idle"), "{review_line}");

    // Removing the complete runtime projection restores the primary
    // session's ordinary activity clock.
    dashboard.set_session_reviews(Vec::new());
    let restored = drawn(&mut dashboard, 140, 44).join("\n");
    assert!(
        restored
            .lines()
            .any(|line| line.contains("podman") && line.contains("Idle")),
        "{restored}"
    );

    // The minimized list uses the same compact activity slot and also
    // must not pair a live review with the primary's idle marker.
    dashboard.set_session_reviews([RuntimeReviewView {
        session_id: "session-second".into(),
        tier: mj_core::review::lanes::ReviewTier::Quick,
        phase: mj_core::review::driver::TurnReviewPhase::Running { roles: Vec::new() },
        roles: Vec::new(),
        status: "the reviewer is reading the change…".into(),
        verdict: None,
    }]);
    minimize_all_panes(&mut dashboard);
    let minimized = drawn(&mut dashboard, 140, 44).join("\n");
    let minimized_line = minimized
        .lines()
        .find(|line| line.contains("Reviewing"))
        .expect("review activity in the minimized list");
    let review_cell = &minimized_line[minimized_line.find("Reviewing").expect("review label")..];
    assert!(!review_cell.contains("Idle"), "{minimized_line}");
}

/// `projects` projects, `per_project` live sessions in each, laid out so
/// the minimized list has headings and enough sessions to scroll. Project
/// directories are zero-padded so they sort in the obvious order.
fn minimized_sessions_dashboard(projects: usize, per_project: usize) -> DashboardState {
    let mut sessions = BTreeMap::new();
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
            subagents: Default::default(),
            version: STATE_VERSION,
            sessions,
            mount_history: BTreeMap::new(),
            container_sizes: BTreeMap::new(),
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
fn minimized_sessions_keep_summary_rows_without_message_previews() {
    let mut dashboard = minimized_sessions_dashboard(3, 2);
    let rendered = drawn(&mut dashboard, 120, 44).join("\n");
    assert!(!dashboard.session_row_areas.is_empty());
    assert!(rendered.contains("ACP pretty"), "{rendered}");
    assert!(!rendered.contains("You:"), "{rendered}");
    assert!(!rendered.contains("Agent:"), "{rendered}");
}

/// A busy minimized list keeps a bounded height and two-line hitboxes.
#[test]
fn minimized_sessions_bound_the_viewport_to_preserve_the_conversation() {
    let mut dashboard = minimized_sessions_dashboard(3, 3);
    let lines = drawn(&mut dashboard, 120, 20);
    assert_eq!(dashboard.pane_areas.expect("pane geometry")[0].height, 16);
    assert!(
        dashboard
            .session_row_areas
            .iter()
            .all(|(_, area)| area.height == 2)
    );
    assert!(!lines.iter().any(|line| line.contains("You:")), "{lines:?}");
}

/// A sparse minimized list uses one row for its heading and each session.
#[test]
fn a_sparse_minimized_list_shows_its_heading_and_sessions() {
    let mut dashboard = minimized_sessions_dashboard(1, 2);
    let lines = drawn(&mut dashboard, 120, 44);
    assert!(
        !lines.iter().any(|line| line.contains("more")),
        "no marker expected when all sessions fit: {lines:?}"
    );
    assert_eq!(dashboard.pane_areas.expect("pane geometry")[0].height, 40);
}

/// The minimized row keeps the session name and its actionable status.
#[test]
fn minimized_sessions_identify_the_session_by_name() {
    let mut dashboard = minimized_sessions_dashboard(1, 1);
    let rendered = drawn(&mut dashboard, 120, 44).join("\n");
    assert!(rendered.contains("ACP pretty"), "{rendered}");
    assert!(rendered.contains("Idle"), "{rendered}");
    let rows = minimized_content_rows(&mut dashboard, 120, 44);
    let status = rows.iter().find(|line| line.contains("Idle")).unwrap();
    assert!(status.starts_with("  Idle"), "{status:?}");
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

/// A narrow minimized pane truncates the summary without bringing back
/// the message previews.
#[test]
fn minimized_sessions_truncate_only_the_top_line() {
    let mut dashboard = minimized_sessions_dashboard(1, 1);
    dashboard
        .state
        .sessions
        .get_mut("session-00")
        .expect("the session")
        .target_template_id = "extremely-long-target-identifier".into();

    let rows = minimized_content_rows(&mut dashboard, 80, 22);
    assert!(
        rows.iter().any(|line| line.contains("ACP pretty")),
        "the narrow summary keeps the session name: {rows:?}"
    );
    assert!(!rows.iter().any(|line| line.contains("You:")), "{rows:?}");
    assert!(!rows.iter().any(|line| line.contains("Agent:")), "{rows:?}");
}

/// The minimized list is a viewport: selecting a session past the visible
/// rows scrolls the window so it shows, and earlier rows leave view.
#[test]
fn the_minimized_list_scrolls_to_keep_the_selection_visible() {
    let mut dashboard = minimized_sessions_dashboard(12, 1);

    // Selecting the first session keeps the window at the start.
    dashboard.selected_session_id = Some("session-00".into());
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
    dashboard.selected_session_id = Some("session-11".into());
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

/// Clicking a minimized row selects that session and leaves the dial where
/// the user set it; the list draws the selection itself.
#[test]
fn clicking_a_minimized_row_selects_it_and_keeps_the_list() {
    use crossterm::event::{MouseButton, MouseEventKind};

    let mut dashboard = minimized_sessions_dashboard(2, 2);
    drawn(&mut dashboard, 120, 44);

    let (index, rect) = *dashboard
        .session_row_areas
        .first()
        .expect("a minimized row hitbox");
    let expected = dashboard.ordered_sessions()[index].id.clone();

    dashboard.handle_mouse(mouse_at_row(
        MouseEventKind::Down(MouseButton::Left),
        rect,
        0,
    ));

    assert_eq!(dashboard.selected_session_id(), Some(expected.as_str()));
    assert!(
        dashboard.sessions_minimized(),
        "the click should leave the minimized list alone"
    );
    assert_eq!(dashboard.focus(), Focus::Sessions);
}

#[test]
fn a_short_terminal_keeps_every_minimized_title_and_control() {
    let mut dashboard = minimized_sessions_dashboard(2, 2);
    dashboard.set_deployment_capacity_targets(vec![test_capacity_target()]);
    dashboard.apply_quota(weekly_quota("claude-1", 63));

    let lines = drawn(&mut dashboard, 120, 20);
    let pane = dashboard.pane_areas.expect("short minimized pane")[0];

    assert!(
        lines[usize::from(pane.y)].contains('╭') && lines[usize::from(pane.y)].contains("Sessi"),
        "the minimized list keeps its title and border: {lines:?}"
    );
    assert!(
        lines[usize::from(pane.y)].contains('▁')
            && lines[usize::from(pane.y)].contains('▪')
            && lines[usize::from(pane.y)].contains('□')
    );
    for visible in ["Targets", "Quota"] {
        assert!(
            lines.iter().any(|line| line.contains(visible)),
            "{visible} should remain visible: {lines:?}"
        );
    }
    let pane = dashboard.pane_areas.expect("pane geometry")[0];
    let selection = dashboard
        .frame_surfaces()
        .surface(SurfaceId::DashboardPane(0))
        .expect("tiny minimized selection surface");
    assert_eq!(selection.rect, pane.inner(Margin::new(1, 1)));
    // Targets and Quota fit beside the minimized sidebar at this width.
    assert_eq!(dashboard.pane_areas.unwrap()[1].x, pane.right());
    assert_eq!(selection.rect.height, 14);
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

/// Minimized Sessions draws a sparse list on a landscape terminal.
#[test]
fn minimized_on_a_landscape_terminal_draws_the_list() {
    let mut dashboard = dashboard_with_session(running_session());
    minimize_all_panes(&mut dashboard);

    let lines = drawn(&mut dashboard, 120, 40);

    assert!(dashboard.sessions_minimized());
    let sessions_height = lines
        .iter()
        .position(|line| line.contains("Conversation"))
        .expect("the conversation band");
    assert_eq!(sessions_height, 0, "{lines:#?}");
}

/// A brand-new workspace has no sessions at all, and minimizing Sessions
/// there must still draw rather than fall over on an empty list.
#[test]
fn the_minimized_list_draws_with_no_sessions() {
    let mut dashboard = DashboardState::new(config(), State::default(), BTreeMap::new());
    minimize_all_panes(&mut dashboard);

    let lines = drawn(&mut dashboard, 200, 50);
    assert!(
        lines.iter().any(|line| line.contains("Conversation")),
        "{lines:#?}"
    );
}

#[test]
fn minimized_on_a_portrait_terminal_still_draws_the_list() {
    let mut dashboard = dashboard_with_session(running_session());
    minimize_all_panes(&mut dashboard);

    let (width, height) = (80u16, 120u16);
    let lines = drawn(&mut dashboard, width, height);

    assert!(dashboard.sessions_minimized());
    let sessions_height = lines
        .iter()
        .position(|line| line.contains("Conversation"))
        .expect("the conversation band");
    assert_eq!(sessions_height, 0, "{lines:#?}");
}

#[test]
fn a_short_portrait_terminal_keeps_the_list_and_support_summaries() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.set_deployment_capacity_targets(vec![test_capacity_target()]);
    dashboard.apply_quota(weekly_quota("claude-1", 63));
    minimize_all_panes(&mut dashboard);

    let lines = drawn(&mut dashboard, 80, 38);

    let pane = dashboard.pane_areas.expect("pane geometry")[0];
    assert!(
        lines[usize::from(pane.y)].contains('╭') && pane.width > 0,
        "the portrait list keeps its bordered Sessions title: {lines:?}"
    );
    let panes = dashboard.pane_areas.expect("pane geometry");
    assert_eq!(panes[0].bottom(), panes[2].bottom());
    assert_eq!(panes[1].x, panes[0].right());
    assert_eq!(panes[1].width, 60);
    for visible in ["Tar", "Quo"] {
        assert!(
            lines.iter().any(|line| line.contains(visible)),
            "{visible} should remain: {lines:?}"
        );
    }
}

#[test]
fn the_minimized_rows_report_cpu_and_weekly_percent_used() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.set_deployment_capacity_targets(vec![test_capacity_target()]);
    dashboard.apply_deployment_capacity("local", Ok(Some(host_usage(42))), now_seconds());
    dashboard.apply_quota(weekly_quota("claude-1", 63));
    minimize_all_panes(&mut dashboard);

    let lines = drawn(&mut dashboard, 120, 44);
    let targets = lines
        .iter()
        .find(|line| line.contains("─ Targets ──"))
        .expect("the minimized Targets row");
    assert!(targets.contains("local 42%"), "{targets:?}");
    let quota = lines
        .iter()
        .find(|line| line.contains("─ Quota ──"))
        .expect("the minimized Quota row");
    // The open pane prints the remaining percentage; so does this row.
    assert!(quota.contains("claude-1 63%"), "{quota:?}");
    // Each minimized pane is exactly one row.
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.contains("─ Targets ──"))
            .count(),
        1
    );
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.contains("─ Quota ──"))
            .count(),
        1
    );
}

/// A fleet with `count` machines running, which is what its probe list
/// records: one probe per live instance.
fn fleet_target(count: usize) -> mj_core::targets::DeploymentCapacityTarget {
    mj_core::targets::DeploymentCapacityTarget {
        id: "aws:ec2".into(),
        host: "ec2".into(),
        target_ids: vec!["ec2".into()],
        kind: DeploymentCapacityKind::AwsFleet,
        local: false,
        probes: (0..count)
            .map(|index| mj_core::targets::CommandSpec::new("true", [format!("instance-{index}")]))
            .collect(),
        probe_error: None,
    }
}

/// A fleet has no CPU percentage of its own, so what it reports in use is
/// how many machines it is running - including when that is none, which
/// used to read "on demand" and said nothing about the fleet's state.
#[test]
fn a_fleet_reports_how_many_machines_it_is_running() {
    for (count, expected) in [(0, "0 VMs"), (1, "1 VM"), (3, "3 VMs")] {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.set_deployment_capacity_targets(vec![fleet_target(count)]);
        if count > 0 {
            dashboard.apply_deployment_capacity(
                "aws:ec2",
                Ok(Some(mj_core::targets::DeploymentCapacityUsage {
                    cpu_percent: None,
                    memory_used_bytes: 0,
                    memory_total_bytes: 8,
                    logical_cores: 4,
                    disk_total_bytes: Some(64),
                })),
                now_seconds(),
            );
        } else {
            dashboard.apply_deployment_capacity("aws:ec2", Ok(None), now_seconds());
        }

        let open = drawn(&mut dashboard, 140, 44).join("\n");
        assert!(open.contains(expected), "open pane, {count}: {open}");
        assert!(!open.contains("on demand"), "open pane, {count}: {open}");

        minimize_all_panes(&mut dashboard);
        let minimized = drawn(&mut dashboard, 140, 44)
            .into_iter()
            .find(|line| line.contains("─ Targets ──"))
            .expect("the minimized Targets row");
        assert!(
            minimized.contains(&format!("ec2 {expected}")),
            "minimized row, {count}: {minimized}"
        );
        assert!(!minimized.contains("no CPU"), "minimized row, {count}");
    }
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
        .find(|line| line.contains("─ Quota ──"))
        .expect("the minimized Quota row");
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
        .find(|line| line.contains("─ Quota ──"))
        .expect("the minimized Quota row");
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

/// The minimized rows read as pane titles: the pane's rule, plain text,
/// and readings separated by commas. Colour is the only thing carrying
/// meaning, and it comes from the same scale the full quota bar uses.
#[test]
fn the_minimized_rows_keep_the_pane_rule_and_colour_by_headroom() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.set_deployment_capacity_targets(vec![
        test_capacity_target(),
        mj_core::targets::DeploymentCapacityTarget {
            id: "morannon".into(),
            host: "morannon".into(),
            ..test_capacity_target()
        },
    ]);
    // A quiet host has headroom; a busy one does not.
    dashboard.apply_deployment_capacity("local", Ok(Some(host_usage(3))), now_seconds());
    dashboard.apply_deployment_capacity("morannon", Ok(Some(host_usage(95))), now_seconds());
    // Plenty of the weekly window left.
    dashboard.apply_quota(weekly_quota("claude-1", 63));
    // Nearly none, and in trouble.
    dashboard.apply_quota(weekly_quota("codex-1", 10));
    minimize_all_panes(&mut dashboard);

    let mut terminal = Terminal::new(TestBackend::new(120, 44)).expect("terminal");
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw the minimized panes");
    let buffer = terminal.backend().buffer();
    let lines = buffer_lines(buffer);

    let targets_row = lines
        .iter()
        .position(|line| line.contains("─ Targets ──"))
        .expect("the minimized Targets row");
    let targets = &lines[targets_row];
    assert!(targets.contains("local 3%, morannon 95%"), "{targets:?}");
    assert!(targets.contains("─ ▁   ▪ "), "{targets:?}");

    let quota_row = lines
        .iter()
        .position(|line| line.contains("─ Quota ──"))
        .expect("the minimized Quota row");
    assert!(
        lines[quota_row].contains("claude-1 63%, codex-1 10%"),
        "the row reads the remaining percentage the open pane prints: {:?}",
        lines[quota_row]
    );

    // The colour of a value, by the column its first digit sits in.
    let colour_of = |row: usize, needle: &str| {
        let column = cell_column(&lines[row], needle);
        buffer[(column, row as u16)].fg
    };
    // A quiet host has headroom left, a busy one does not; a quota reads
    // the same scale on the headroom it reports.
    assert_eq!(colour_of(targets_row, "3%"), theme::palette().success);
    assert_eq!(colour_of(targets_row, "95%"), theme::palette().error);
    assert_eq!(colour_of(quota_row, "63%"), theme::palette().success);
    assert_eq!(colour_of(quota_row, "10%"), theme::palette().error);
    // The label and the names are ordinary text; only the values carry a
    // colour.
    assert_eq!(colour_of(targets_row, "Targets"), theme::palette().text);
    assert_eq!(colour_of(targets_row, "morannon"), theme::palette().text);
}

/// A usage-priced profile has no window to summarise, so once its report
/// says it is API-priced it is left out of the minimized row rather than
/// spending width on a placeholder.
#[test]
fn a_usage_priced_profile_is_absent_from_the_minimized_row() {
    let mut dashboard = dashboard_with_session(running_session());
    add_api_priced_profile(&mut dashboard);
    minimize_all_panes(&mut dashboard);

    let row = |dashboard: &mut DashboardState| {
        drawn(dashboard, 160, 44)
            .into_iter()
            .find(|line| line.contains("─ Quota ──"))
            .expect("the minimized Quota row")
    };

    dashboard.apply_quota(api_quota("api-priced"));
    let after = row(&mut dashboard);
    assert!(!after.contains("api-priced"), "{after:?}");
    assert!(!after.contains("API"), "{after:?}");
    // The subscription profiles still read normally.
    assert!(after.contains("claude-1"), "{after:?}");
}

/// A week with headroom left is no comfort while the next five hours are
/// spent, so a profile that has dipped into its week reports both figures.
/// An untouched week says everything there is to say on its own, and a
/// profile with no five-hour window has nothing more to add.
#[test]
fn the_minimized_row_pairs_the_weekly_and_five_hour_figures() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.apply_quota(weekly_and_five_hour_quota("claude-1", 96, 40));
    dashboard.apply_quota(weekly_and_five_hour_quota("codex-1", 100, 40));
    dashboard.apply_quota(weekly_quota("codex-2", 63));
    minimize_all_panes(&mut dashboard);

    let quota = drawn(&mut dashboard, 160, 44)
        .into_iter()
        .find(|line| line.contains("─ Quota ──"))
        .expect("the minimized Quota row");
    assert!(quota.contains("claude-1 96%/40%"), "{quota:?}");
    assert!(quota.contains("codex-1 100%,"), "{quota:?}");
    assert!(!quota.contains("100%/"), "{quota:?}");
    assert!(quota.contains("codex-2 63%"), "{quota:?}");
    assert!(!quota.contains("63%/"), "{quota:?}");
}

/// The reading's colour has to describe the window that is actually
/// running out: a profile with most of its week left but no five-hour
/// headroom is in trouble now.
#[test]
fn the_paired_reading_takes_the_colour_of_the_tighter_window() {
    let mut dashboard = dashboard_with_session(running_session());
    // Plenty of week, almost no five hours.
    dashboard.apply_quota(weekly_and_five_hour_quota("claude-1", 96, 5));
    // Almost no week, plenty of five hours.
    dashboard.apply_quota(weekly_and_five_hour_quota("codex-1", 8, 90));
    minimize_all_panes(&mut dashboard);

    let mut terminal = Terminal::new(TestBackend::new(160, 44)).expect("terminal");
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw the minimized panes");
    let buffer = terminal.backend().buffer();
    let lines = buffer_lines(buffer);
    let row = lines
        .iter()
        .position(|line| line.contains("─ Quota ──"))
        .expect("the minimized Quota row");
    let colour_of = |needle: &str| {
        let column = cell_column(&lines[row], needle);
        buffer[(column, row as u16)].fg
    };

    assert_eq!(colour_of("96%/5%"), theme::palette().error);
    assert_eq!(colour_of("8%/90%"), theme::palette().error);
}

/// A minimized pane is one row by definition, so more hosts than fit have
/// to be cut rather than wrapped onto a second row.
#[test]
fn the_minimized_rows_truncate_rather_than_wrap() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.set_deployment_capacity_targets(
        (0..8)
            .map(|index| mj_core::targets::DeploymentCapacityTarget {
                id: format!("host-{index}"),
                host: format!("a-rather-long-host-name-{index}"),
                ..test_capacity_target()
            })
            .collect(),
    );
    minimize_all_panes(&mut dashboard);

    let lines = drawn(&mut dashboard, 80, 44);
    let rows = lines
        .iter()
        .filter(|line| line.contains("─ Targets ──"))
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), 1, "{lines:#?}");
    assert!(rows[0].chars().count() <= 80);
    assert!(
        rows[0].contains('…'),
        "the readings are cut rather than wrapped: {:?}",
        rows[0]
    );
    assert!(rows[0].contains("─ ▁   ▪ "), "{:?}", rows[0]);
}

#[test]
fn read_idle_session_stays_blue_in_expanded_and_collapsed_rows() {
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
        assert!(
            (pane.x + 1..pane.right() - 1)
                .filter(|x| summary_text_cell(&buffer[(*x, status_y)]))
                .all(|x| buffer[(x, status_y)].fg == theme::palette().session_idle),
            "{collapsed}: {status}"
        );
    }
}

#[test]
fn session_name_prefers_override_then_acp_title_then_hel_uuid() {
    let mut session = stopped_session();
    assert_eq!(session_name(&session), "ACP pretty name");

    session.acp_session_title = None;
    assert_eq!(session_name(&session), "session-1");

    session.session_title_override = Some("My name".into());
    assert_eq!(session_name(&session), "My name");

    session.session_title_override = None;
    session.native_session_id = None;
    assert_eq!(session_name(&session), "session-1");
    assert_ne!(session_name(&session), session.title);
}

/// A capacity sample the poller keeps refreshing carries no clock column
/// and no staleness marker: the number on screen is the current one.
#[test]
fn capacity_pane_renders_grouped_host_load_without_sample_clock() {
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
        })),
        now_epoch_seconds(),
    );
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

    assert!(
        rendered.contains("podman, mac-container  37% CPU"),
        "{rendered}"
    );
    assert!(rendered.contains("37% CPU · 75% RAM"));
    assert!(!rendered.contains("Sample"));
    assert!(!rendered.contains("stale"));
    let buffer = terminal.backend().buffer();
    let header = (buffer.area.y..buffer.area.bottom())
        .map(|y| {
            (buffer.area.x..buffer.area.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .find(|line| line.contains("Host / fleet") && line.contains("Targets"))
        .expect("capacity header");
    assert!(header.contains("In Use"));
}

#[test]
fn dashboard_colors_named_host_permission_badges() {
    let mut config = config();
    let container = match config.targets["podman"].clone() {
        mj_core::config::TargetTemplate::LocalPodman { container } => container,
        _ => unreachable!(),
    };
    let ssh = |host: &str| mj_core::config::SshConnection {
        host: host.into(),
        user: None,
        identity_file: None,
        extra_args: Vec::new(),
    };
    config.targets.insert(
        "precision-3260".into(),
        mj_core::config::TargetTemplate::SshBare {
            ssh: ssh("precision-3260"),
            permissions: PermissionMode::Yolo,
            workspace_prefix: ".local/share/hel/workspaces".into(),
        },
    );
    config.targets.insert(
        "morannon-podman".into(),
        mj_core::config::TargetTemplate::SshPodman {
            ssh: ssh("morannon"),
            container,
        },
    );
    config.targets.insert(
        "morannon-raw".into(),
        mj_core::config::TargetTemplate::SshBare {
            ssh: ssh("morannon"),
            permissions: PermissionMode::Guardian,
            workspace_prefix: ".local/share/hel/workspaces".into(),
        },
    );
    let mut session = running_session();
    session.target_template_id = "precision-3260".into();
    session.project_directory = Some("/home/dev/hel".into());
    let state = State {
        subagents: Default::default(),
        version: STATE_VERSION,
        sessions: BTreeMap::from([(session.id.clone(), session)]),
        mount_history: BTreeMap::new(),
        container_sizes: BTreeMap::new(),
    };
    let mut dashboard = DashboardState::new(config, state, BTreeMap::new());
    let capacity_target =
        |host: &str, target_ids: &[&str]| mj_core::targets::DeploymentCapacityTarget {
            id: format!("ssh:{host}"),
            host: host.into(),
            target_ids: target_ids.iter().map(|id| (*id).into()).collect(),
            kind: DeploymentCapacityKind::Host,
            local: false,
            probes: Vec::new(),
            probe_error: None,
        };
    dashboard.set_deployment_capacity_targets(vec![
        capacity_target("precision-3260", &["precision-3260"]),
        capacity_target("morannon", &["morannon-podman", "morannon-raw"]),
    ]);
    let mut terminal = Terminal::new(TestBackend::new(160, 40)).expect("terminal");

    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw dashboard");

    let buffer = terminal.backend().buffer();
    let lines = buffer_lines(buffer);
    let badge_has_color = |needle: &str, color: Color| {
        lines.iter().enumerate().any(|(row, line)| {
            let Some(byte) = line.find(needle) else {
                return false;
            };
            let x = buffer.area.x + line[..byte].chars().count() as u16;
            (x..x + 3).all(|x| buffer[(x, buffer.area.y + row as u16)].fg == color)
        })
    };
    let rendered = lines.join("\n");
    assert!(rendered.contains("precision-3260 [Y]"), "{rendered}");
    assert!(
        rendered.contains("morannon-podman, morannon-raw [G]"),
        "{rendered}"
    );
    assert!(!rendered.contains("morannon-podman [G]"), "{rendered}");
    assert!(badge_has_color("[Y]", theme::palette().error), "{rendered}");
    assert!(
        badge_has_color("[G]", theme::palette().success),
        "{rendered}"
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
    }
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

#[test]
fn selected_transcript_tail_adapts_to_a_constrained_terminal() {
    let mut session = stopped_session();
    session.state = SessionState::Running;
    let mut dashboard = dashboard_with_session(session);
    let message = (1..=20)
        .map(|line| format!("line {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    apply_materialized_transcript(&mut dashboard, vec![agent_message(1, message)]);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");

    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw constrained dashboard");

    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("Sessions"));
    assert!(rendered.contains("Targets"));
    assert!(rendered.contains("Quota"));
}

#[test]
fn overflowing_session_pane_shows_a_scrollbar() {
    let mut sessions = BTreeMap::new();
    for index in 0..6 {
        let mut session = running_session();
        session.id = format!("active-{index:02}");
        session.state = SessionState::Running;
        sessions.insert(session.id.clone(), session);
    }
    for index in 0..20 {
        let mut session = stopped_session();
        session.id = format!("archived-{index:02}");
        sessions.insert(session.id.clone(), session);
    }
    let state = State {
        subagents: Default::default(),
        version: STATE_VERSION,
        sessions,
        mount_history: BTreeMap::new(),
        container_sizes: BTreeMap::new(),
    };
    let mut dashboard = DashboardState::new(config(), state, BTreeMap::new());
    for index in 0..6 {
        apply_materialized_transcript_for(
            &mut dashboard,
            &format!("active-{index:02}"),
            vec![agent_message(1, "one\ntwo\nthree\nfour")],
        );
    }
    let backend = TestBackend::new(120, 24);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw dashboard");
    let symbols = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<Vec<_>>();

    let thumb = symbols.iter().filter(|symbol| **symbol == "▐").count();
    let track = symbols.iter().filter(|symbol| **symbol == "│").count();
    assert!(thumb >= 1, "expected a scrollbar thumb, rendered {thumb}");
    assert!(track >= 1, "expected a scrollbar track, rendered {track}");
}

#[test]
fn fully_visible_tables_do_not_show_scrollbars() {
    let mut dashboard = dashboard_with_session(stopped_session());
    let mut terminal = Terminal::new(TestBackend::new(120, 36)).expect("test terminal");

    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw fully visible tables");
    let symbols = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<Vec<_>>();

    assert!(!symbols.contains(&"▐"));
}

#[test]
fn overflowing_quota_pane_uses_the_shared_scrollbar() {
    let mut config = config();
    let profile = config.profiles["codex-1"].clone();
    for index in 0..20 {
        config
            .profiles
            .insert(format!("profile-{index:02}"), profile.clone());
    }
    let mut dashboard = DashboardState::new(config, State::default(), BTreeMap::new());
    dashboard.focus = Focus::Quota;
    let mut terminal = Terminal::new(TestBackend::new(120, 24)).expect("test terminal");

    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw overflowing quotas");
    let symbols = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<Vec<_>>();

    assert!(symbols.contains(&"▐"));
    assert!(symbols.contains(&"│"));
}

#[test]
fn active_checkpoint_age_uses_compact_seconds_minutes_hours_and_days() {
    let checkpointed_at = "2026-08-09T01:00:00Z";
    let base = chrono::DateTime::parse_from_rfc3339(checkpointed_at)
        .unwrap()
        .timestamp() as u64;

    assert_eq!(checkpoint_age(base + 12, checkpointed_at), "12s");
    assert_eq!(checkpoint_age(base + 8 * 60, checkpointed_at), "8m");
    assert_eq!(checkpoint_age(base + 3 * 3_600, checkpointed_at), "3h");
    assert_eq!(checkpoint_age(base + 2 * 86_400, checkpointed_at), "2d");
}

#[test]
fn recovery_state_is_hidden_until_a_failure_needs_attention() {
    let mut session = stopped_session();
    session.state = SessionState::Running;
    assert_eq!(
        recovery_warning_name(&session, "Build Hel".into(), 0),
        "Build Hel"
    );

    session.last_checkpoint_error = Some("copy failed".into());
    session.checkpoint = None;
    assert_eq!(
        recovery_warning_name(&session, "Build Hel".into(), 0),
        "Build Hel  ⚠ Recovery unavailable"
    );
}

#[test]
fn active_session_with_no_turn_in_flight_reads_idle() {
    let mut session = stopped_session();
    session.state = SessionState::Running;
    let detail = SessionDetail {
        last_activity_at_ms: Some(1_000_000),
        ..SessionDetail::default()
    };

    let text = session_metadata_text(&session, Some(&detail), None, 1_480, &config());
    assert!(text.contains("Idle"), "{text}");
}

#[test]
fn provisioning_clock_uses_elapsed_seconds_since_state_update() {
    let mut session = stopped_session();
    session.state = SessionState::Provisioning;
    session.updated_at = "1970-01-01T00:16:40Z".into();

    let text = session_metadata_text(&session, None, None, 1_012, &config());
    assert!(text.contains("Launch 12s"), "{text}");
}

#[test]
fn transition_row_is_compact_and_contains_stage_identity_and_elapsed() {
    let session = stopped_session();
    let operation = operation(SessionOperationKind::Moving, Some(ProvisionStage::Cloning));
    let line = session_transition_line(
        "› ",
        &session,
        mj_core::state::SessionTransitionKind::Moving,
        Some(&operation),
        1_012,
        "podman",
        120,
        None,
    );
    let text = line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();
    assert!(text.contains("Moving"), "{text}");
    assert!(text.contains("Clone"), "{text}");
    assert!(text.contains("12s"), "{text}");
    assert!(text.contains("ACP pretty name"), "{text}");
    assert!(!text.contains("No messages"), "{text}");

    // Sessions that start together on one target differ only by their names,
    // so the name has to survive a narrow Sessions pane.
    let narrow = session_transition_line(
        "› ",
        &session,
        mj_core::state::SessionTransitionKind::Moving,
        Some(&operation),
        1_012,
        "podman",
        40,
        None,
    );
    let narrow = narrow
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();
    assert!(narrow.contains("ACP pretty name"), "{narrow}");
    assert!(narrow.contains("Moving"), "{narrow}");
}

#[test]
fn launch_clock_names_the_reported_stage() {
    let session = stopped_session();
    let operation = operation(
        SessionOperationKind::Launching,
        Some(ProvisionStage::Booting),
    );

    let text = session_metadata_text(&session, None, Some(&operation), 1_012, &config());
    assert!(text.contains("Boot 12s"), "{text}");
}

#[test]
fn launch_clock_falls_back_to_the_kind_label_without_a_stage() {
    let session = stopped_session();
    let operation = operation(SessionOperationKind::Launching, None);

    let text = session_metadata_text(&session, None, Some(&operation), 1_012, &config());
    assert!(text.contains("Launch 12s"), "{text}");
}

#[test]
fn a_stage_does_not_rename_a_non_launch_operation() {
    let session = stopped_session();
    let operation = operation(
        SessionOperationKind::Stopping,
        Some(ProvisionStage::Syncing),
    );

    let text = session_metadata_text(&session, None, Some(&operation), 1_012, &config());
    assert!(text.contains("Stopping 12s"), "{text}");
}

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
fn without_a_resume_destination_the_row_falls_back_to_the_session_record() {
    let session = stopped_session();
    let resuming = operation(SessionOperationKind::Resuming, None);

    let text = session_metadata_text(&session, None, Some(&resuming), 1_012, &config());

    assert!(text.contains(&session.last_profile), "{text}");
    assert!(text.contains(&session.target_template_id), "{text}");
}

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
fn install_stage_names_the_harness_not_the_profile() {
    let session = stopped_session();
    assert_eq!(session.last_profile, "codex-1");
    let operation = operation(
        SessionOperationKind::Launching,
        Some(ProvisionStage::Installing(HarnessKind::Codex)),
    );

    let text = session_metadata_text(&session, None, Some(&operation), 1_012, &config());

    assert!(text.contains("Installing Codex 12s"), "{text}");
    assert!(text.contains("codex-1"), "{text}");
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
fn focused_panes_use_accented_rounded_borders_without_focus_title_text() {
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
    assert!(rendered.contains("╭Sessions"));
    assert!(rendered.contains("Targets"));
    assert!(!rendered.contains("[focused]"));

    for (focus, rounded) in [(Focus::Quota, "╭ Quota"), (Focus::Targets, "╭ Targets")] {
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
        assert_eq!(border.fg, theme::palette().accent);
        assert!(border.modifier.contains(Modifier::BOLD));
    }
}

#[test]
fn only_focused_pane_draws_caret_without_shifting_table_columns() {
    let mut first = stopped_session();
    first.id = "session-0".into();
    first.state = SessionState::Running;
    let mut second = stopped_session();
    second.state = SessionState::Running;
    let mut dashboard = DashboardState::new(
        config(),
        State {
            subagents: Default::default(),
            version: STATE_VERSION,
            sessions: BTreeMap::from([(first.id.clone(), first), (second.id.clone(), second)]),
            mount_history: BTreeMap::new(),
            container_sizes: BTreeMap::new(),
        },
        BTreeMap::new(),
    );
    dashboard.set_deployment_capacity_targets(vec![test_capacity_target()]);
    let backend = TestBackend::new(120, 40);
    let mut terminal = Terminal::new(backend).expect("terminal");
    let mut initial_name_columns = None;

    for expected_focus in [Focus::Sessions, Focus::Targets, Focus::Quota] {
        dashboard.focus = expected_focus;
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw dashboard");
        let buffer = terminal.backend().buffer();
        let lines = (buffer.area.y..buffer.area.bottom())
            .map(|y| {
                (buffer.area.x..buffer.area.right())
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        // The Sessions pane always marks the conversation on screen, and
        // a focused table marks its own row. Nothing else draws a caret,
        // so a support pane that does not have focus adds none.
        let carets = lines
            .iter()
            .flat_map(|line| line.chars())
            .filter(|character| *character == '›')
            .count();
        let expected_carets = usize::from(expected_focus != Focus::Sessions) + 1;
        assert_eq!(carets, expected_carets, "{expected_focus:?}");
        if expected_focus == Focus::Sessions {
            // Both sessions draw their expanded form, and the caret on one
            // of them does not shift the other's columns.
            let name_columns = lines
                .iter()
                .filter_map(|line| {
                    line.find("ACP pretty name")
                        .map(|byte| line[..byte].chars().count())
                })
                .collect::<Vec<_>>();
            assert_eq!(name_columns.len(), 2);
            assert_eq!(name_columns[0], name_columns[1]);
            initial_name_columns = Some(name_columns);
        }
    }
    assert!(initial_name_columns.is_some());
}

#[test]
fn existing_sessions_remain_visible_when_settings_has_no_accounts_or_targets() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.set_config(Config::default());
    let rendered = drawn(&mut dashboard, 120, 40).join("\n");
    assert!(rendered.contains("Sessions"), "{rendered}");
    assert!(rendered.contains("ACP pretty name"), "{rendered}");
    assert!(dashboard.pane_areas.is_some());
    dashboard.focus_sessions();
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::None
    );
    assert!(matches!(dashboard.mode, Mode::Confirm(_)));
}

#[test]
fn empty_config_renders_onboarding_with_the_workspace_name() {
    let mut dashboard = DashboardState::new(Config::default(), State::default(), BTreeMap::new());
    dashboard.set_workspace_name("personal".into());
    let backend = TestBackend::new(100, 24);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw dashboard");
    let buffer = terminal.backend().buffer();
    let rendered = buffer
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("Mjolnir needs a little fuel."));
    assert!(rendered.contains("personal"));
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Char('e'))),
        DashboardAction::None
    );
}

#[test]
fn workspace_name_does_not_change_with_dashboard_updates() {
    let mut dashboard = DashboardState::new(config(), State::default(), BTreeMap::new());
    dashboard.set_workspace_name("acme-workspace".into());
    dashboard.set_state(State::default());
    dashboard.set_quotas(BTreeMap::new());

    let mut terminal = Terminal::new(TestBackend::new(100, 24)).expect("terminal");
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
    assert!(rendered.contains("Sessions"));
    assert_eq!(dashboard.workspace_name, "acme-workspace");
}

#[test]
fn quota_render_includes_errors_and_refresh_age_in_title() {
    let mut dashboard = DashboardState::new(
        config(),
        State::default(),
        BTreeMap::from([(
            "codex-1".into(),
            ProfileQuota {
                profile_id: "codex-1".into(),
                harness: HarnessKind::Codex,
                windows: vec![],
                extra: None,
                error: Some("offline".into()),
                refreshed_at_epoch_seconds: 1,
            },
        )]),
    );
    let backend = TestBackend::new(120, 28);
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
    assert!(rendered.contains("unavailable"));
    assert!(!rendered.contains("offline"));
    assert!(rendered.contains("Quota (refreshed"));
    assert!(!rendered.contains("Refreshed"));
    assert!(!rendered.contains("Access"));
    assert!(!rendered.contains("agent-full-access"));
}

#[test]
fn quota_render_shows_login_expired_without_unavailable_prefix() {
    let mut dashboard = DashboardState::new(
        config(),
        State::default(),
        BTreeMap::from([(
            "claude-1".into(),
            ProfileQuota {
                profile_id: "claude-1".into(),
                harness: HarnessKind::Claude,
                windows: vec![],
                extra: None,
                error: Some("login expired".into()),
                refreshed_at_epoch_seconds: 1,
            },
        )]),
    );
    let backend = TestBackend::new(120, 28);
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
    assert!(rendered.contains("login expired"));
    assert!(!rendered.contains("unavailable: login expired"));
}

#[test]
fn a_usage_priced_quota_row_shows_api_without_bars_or_reset_dates() {
    let mut dashboard = DashboardState::new(config(), State::default(), BTreeMap::new());
    dashboard.apply_quota(api_quota("codex-1"));
    let mut terminal = Terminal::new(TestBackend::new(120, 28)).expect("terminal");
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

    assert!(rendered.contains("API"));
    assert!(rendered.contains("▕   API    ▏"));
    assert!(!rendered.contains("API Pricing"));
    assert!(!rendered.contains("unavailable"));
    assert!(!rendered.contains('%'));
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

#[test]
fn api_quota_label_uses_the_black_bordered_chart_field() {
    let api = api_quota_bar();
    let rendered: String = api.spans.iter().map(|span| span.content.as_ref()).collect();
    assert_eq!(rendered, "   API    ▏");
    assert!(
        api.spans
            .iter()
            .all(|span| span.style.bg == Some(theme::palette().background))
    );
    assert_eq!(api.spans[3].style.fg, Some(theme::palette().muted));
}

#[test]
fn quota_render_hides_five_hour_bar_and_reset_when_weekly_quota_is_exhausted() {
    let quota = ProfileQuota {
        profile_id: "codex-1".into(),
        harness: HarnessKind::Codex,
        windows: vec![
            QuotaWindow {
                label: "Week".into(),
                remaining_percent: Some(0),
                used: None,
                limit: None,
                resets: Some("09:00 Aug 20".into()),
                resets_at_epoch_seconds: Some(604_800),
            },
            QuotaWindow {
                label: "5H".into(),
                remaining_percent: Some(70),
                used: None,
                limit: None,
                resets: Some("14:00 Aug 13".into()),
                resets_at_epoch_seconds: Some(14_400),
            },
        ],
        extra: None,
        error: None,
        refreshed_at_epoch_seconds: 0,
    };
    let mut dashboard = DashboardState::new(
        config(),
        State::default(),
        BTreeMap::from([("codex-1".into(), quota)]),
    );
    let mut terminal = Terminal::new(TestBackend::new(140, 28)).expect("terminal");
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

    assert!(rendered.contains("0%"));
    assert!(!rendered.contains("70%"));
    assert!(!rendered.contains("4h"));
}

#[test]
fn quota_reset_countdown_always_shows_hours() {
    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;
    let now = 100;

    assert_eq!(
        quota_reset_countdown(now, (now + 2 * DAY + 5 * HOUR) as i64),
        "2d 5h"
    );
    assert_eq!(quota_reset_countdown(now, (now + 2 * DAY) as i64), "2d 0h");
    assert_eq!(
        quota_reset_countdown(now, (now + DAY + 5 * HOUR) as i64),
        "1d 5h"
    );
    assert_eq!(
        quota_reset_countdown(now, (now + 2 * HOUR + 5 * MINUTE) as i64),
        "2h"
    );
    assert_eq!(
        quota_reset_countdown(now, (now + HOUR + 5 * MINUTE) as i64),
        "1h 5m"
    );
    assert_eq!(
        quota_reset_countdown(now, (now + 35 * MINUTE) as i64),
        "35m"
    );
    assert_eq!(quota_reset_countdown(now, (now + 30) as i64), "<1m");
    assert_eq!(quota_reset_countdown(now, now as i64), "now");
}

#[test]
fn weekly_and_five_hour_resets_are_independent() {
    let quota = ProfileQuota {
        profile_id: "codex-1".into(),
        harness: HarnessKind::Codex,
        windows: vec![
            QuotaWindow {
                label: "Week".into(),
                remaining_percent: Some(73),
                used: None,
                limit: None,
                resets: Some("09:00 Aug 20".into()),
                resets_at_epoch_seconds: Some(604_800),
            },
            QuotaWindow {
                label: "5H".into(),
                remaining_percent: Some(80),
                used: None,
                limit: None,
                resets: Some("14:00 Aug 13".into()),
                resets_at_epoch_seconds: Some(14_400),
            },
        ],
        extra: None,
        error: None,
        refreshed_at_epoch_seconds: 0,
    };

    assert_eq!(
        quota_reset_cells(&quota, 0),
        ("7d 0h".into(), "4h 0m".into())
    );
}

#[test]
fn five_hour_reset_always_uses_minutes_above_one_hour() {
    const MINUTE: i64 = 60;
    const HOUR: i64 = 60 * MINUTE;

    assert_eq!(
        five_hour_quota_reset_countdown(100, 100 + 4 * HOUR + 50 * MINUTE),
        "4h 50m"
    );
    assert_eq!(
        five_hour_quota_reset_countdown(100, 100 + 4 * HOUR + 5 * MINUTE),
        "4h 5m"
    );
    assert_eq!(
        five_hour_quota_reset_countdown(100, 100 + HOUR + 5 * MINUTE),
        "1h 5m"
    );
    assert_eq!(five_hour_quota_reset_countdown(100, 130), "<1m");
}

#[test]
fn quota_render_uses_weekly_five_hour_and_reset_columns() {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let now = i64::try_from(now).unwrap();
    let quota = ProfileQuota {
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
    };
    let mut dashboard = DashboardState::new(
        config(),
        State::default(),
        BTreeMap::from([("codex-1".into(), quota)]),
    );
    let mut terminal = Terminal::new(TestBackend::new(140, 28)).expect("terminal");
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw dashboard");
    let lines = buffer_lines(terminal.backend().buffer());
    let rendered = lines.join("\n");

    assert!(rendered.contains("Weekly"));
    assert!(rendered.contains("5H"));
    assert_eq!(rendered.matches("Resets").count(), 2);
    assert!(rendered.contains("73%"));
    assert!(rendered.contains("70%"));
    assert!(rendered.contains("2d"));
    assert!(rendered.contains("1h 5m"));
    assert!(!rendered.contains("09:00 Aug 20"));

    let row = lines
        .iter()
        .find(|line| line.contains("codex-1"))
        .expect("quota row");
    assert!(row.contains("▕███████▎  ▏73%"), "{row:?}");
    assert!(row.contains("▕███████   ▏70%"), "{row:?}");
    let weekly_percent = cell_column(row, "73%");
    let weekly_reset = cell_column(row, "2d");
    let five_hour_percent = cell_column(row, "70%");
    let five_hour_reset = cell_column(row, "1h 5m");
    assert_eq!(weekly_reset, weekly_percent + 3 + 1);
    assert_eq!(five_hour_percent - 12, weekly_reset + 6 + 2);
    assert_eq!(five_hour_reset, five_hour_percent + 3 + 1);
}

#[test]
fn quota_render_keeps_both_percentages_and_resets_at_eighty_columns() {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let now = i64::try_from(now).unwrap();
    let quota = ProfileQuota {
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
    };
    let mut dashboard = DashboardState::new(
        config(),
        State::default(),
        BTreeMap::from([("codex-1".into(), quota)]),
    );
    let mut terminal = Terminal::new(TestBackend::new(80, 28)).expect("terminal");
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw dashboard");
    let row = buffer_lines(terminal.backend().buffer())
        .into_iter()
        .find(|line| line.contains("codex-1"))
        .expect("quota row");
    assert!(row.contains("73%"), "{row:?}");
    assert!(row.contains("70%"), "{row:?}");
    assert!(row.contains("2d"), "{row:?}");
    assert!(row.contains("1h 5m"), "{row:?}");
}

/// Two panes are two conversations on one screen: each draws its own bordered
/// panel, in the rectangle the layout tree says it owns.
#[test]
fn two_panes_draw_two_conversation_panels_at_the_rectangles_the_layout_reports() {
    let mut second = running_session();
    second.id = "session-2".into();
    let mut dashboard = dashboard_with_session(running_session());
    dashboard
        .state
        .sessions
        .insert(second.id.clone(), second.clone());
    dashboard.set_current_session(Some("session-1"));
    drawn(&mut dashboard, 160, 44);
    let first = dashboard.focused_pane();
    let right = dashboard
        .split_focused_pane(ratatui::layout::Direction::Horizontal, Some("session-2"))
        .expect("a 160-column frame has room for two panes");

    let lines = drawn(&mut dashboard, 160, 44);

    let band = dashboard.conversation_area.expect("the conversation band");
    let expected = dashboard.conversation_layout.panes(band);
    assert_eq!(expected.len(), 2);
    for pane in &expected {
        let (transcript, prompt) = dashboard.pane_bands(pane.id).expect("the pane drew");
        assert_eq!(transcript.x, pane.rect.x);
        assert_eq!(transcript.width, pane.rect.width);
        assert_eq!(transcript.y, pane.rect.y);
        assert_eq!(prompt.x, pane.rect.x);
        assert_eq!(prompt.width, pane.rect.width);
        assert_eq!(transcript.bottom(), prompt.y);
        assert_eq!(prompt.bottom(), pane.rect.bottom());
        assert!(transcript.height >= 3 && prompt.height >= 3);
    }
    // Both panels are on screen at once, side by side on the same rows.
    let panels = lines
        .iter()
        .filter(|line| line.matches(" Conversation ").count() == 2)
        .count();
    assert!(panels > 0, "{lines:#?}");
    assert_ne!(first, right);
}

/// The focused pane is the one the user is typing into: only it wears the
/// focused border, and only it hands its keys to the footer.
#[test]
fn only_the_focused_pane_draws_the_focused_border() {
    let mut second = running_session();
    second.id = "session-2".into();
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.state.sessions.insert(second.id.clone(), second);
    dashboard.set_current_session(Some("session-1"));
    dashboard.focus_prompt();
    drawn(&mut dashboard, 160, 44);
    let first = dashboard.focused_pane();
    let right = dashboard
        .split_focused_pane(ratatui::layout::Direction::Horizontal, Some("session-2"))
        .expect("a 160-column frame has room for two panes");
    dashboard.focus_prompt();

    let mut terminal = Terminal::new(TestBackend::new(160, 44)).expect("terminal");
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw the two panes");
    let buffer = terminal.backend().buffer();

    let border_style = |pane| {
        let (_, prompt) = dashboard.pane_bands(pane).expect("the pane drew");
        buffer[(prompt.x, prompt.y)].style()
    };
    assert_eq!(dashboard.focused_pane(), right);
    assert_ne!(
        border_style(right),
        border_style(first),
        "the focused pane's composer is drawn differently from its neighbour's"
    );
}
