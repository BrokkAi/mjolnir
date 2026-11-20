use super::*;
use crate::test_support::{drawn, key, point};
use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

fn dashboard() -> DashboardState {
    DashboardState::new(
        mj_core::config::Config::default().with_local_targets(),
        mj_core::state::State::default(),
        Default::default(),
    )
}

#[test]
fn welcome_can_be_dismissed_during_discovery_and_late_errors_become_notices() {
    for code in [KeyCode::Enter, KeyCode::Esc] {
        let mut dashboard = dashboard();
        dashboard.begin_welcome();
        drawn(&mut dashboard, 100, 30);
        assert_eq!(dashboard.handle_key(key(code)), DashboardAction::None);
        assert!(matches!(dashboard.mode, Mode::Dashboard));
        dashboard.welcome_configured(vec!["Agents: Claude Code.".into()], true);
        dashboard.welcome_checked(vec!["Run mj login --profile claude".into()]);
        assert!(matches!(dashboard.mode, Mode::Dashboard));
        assert!(
            dashboard
                .notices
                .history()
                .iter()
                .any(|notice| notice.failure && notice.text.contains("mj login"))
        );
    }
}

#[test]
fn first_run_does_not_replace_an_open_settings_draft() {
    let mut dashboard = dashboard();
    dashboard.begin_setup();
    dashboard.begin_welcome();
    dashboard.welcome_configured(vec!["Agents: Codex.".into()], true);
    dashboard.welcome_checked(Vec::new());
    assert!(matches!(dashboard.mode, Mode::Setup(_)));
    dashboard.cancel_modal();
    let text = drawn(&mut dashboard, 100, 30).join("\n");
    assert!(text.contains("Welcome to Mjolnir"));
    assert!(text.contains("Agents: Codex."));
}

// Hard-won: 5238411cd: a no-setup result left an automatic Welcome open or pending.
#[test]
fn existing_setup_completion_retires_welcome_without_closing_another_dialog() {
    for settings_open in [false, true] {
        let mut dashboard = dashboard();
        if settings_open {
            dashboard.begin_setup();
        }
        dashboard.begin_welcome();
        dashboard.welcome_not_needed();
        if settings_open {
            assert!(matches!(dashboard.mode, Mode::Setup(_)));
            dashboard.cancel_modal();
        }
        assert!(matches!(dashboard.mode, Mode::Dashboard));
        let text = drawn(&mut dashboard, 100, 30).join("\n");
        assert!(!text.contains("Welcome to Mjolnir"), "{text}");
    }
}

#[test]
fn welcome_offers_managed_setup_when_no_agent_was_found() {
    let mut dashboard = dashboard();
    dashboard.begin_welcome();
    dashboard.welcome_configured(
        vec!["No coding agent found yet. Mjolnir can install one and sign you in.".into()],
        false,
    );
    dashboard.welcome_checked(Vec::new());
    let lines = drawn(&mut dashboard, 110, 32);
    assert!(
        lines
            .iter()
            .any(|line| line.contains("No coding agent found yet"))
    );
    let (column, row) = point(&lines, "Set up a managed agent");
    dashboard.handle_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column,
        row,
        modifiers: KeyModifiers::NONE,
    });
    let action = dashboard.handle_mouse(MouseEvent {
        kind: MouseEventKind::Up(MouseButton::Left),
        column,
        row,
        modifiers: KeyModifiers::NONE,
    });
    assert_eq!(action, DashboardAction::OpenManagedSetup);
}

#[test]
fn golden_first_run_welcome() {
    let mut output = String::new();
    for (label, agents) in [("no agent found", false), ("agents found", true)] {
        let mut dashboard = dashboard();
        dashboard.begin_welcome();
        dashboard.welcome_configured(
            vec![if agents {
                "Agents: Codex, Claude Code.".to_owned()
            } else {
                "No coding agent found yet. Mjolnir can install one and sign you in.".to_owned()
            }],
            agents,
        );
        dashboard.welcome_checked(Vec::new());
        let lines = drawn(&mut dashboard, 100, 24);
        output.push_str(&format!("=== {label} (100x24) ===\n{}\n", lines.join("\n")));
    }
    mj_core::golden::assert_golden(
        env!("CARGO_MANIFEST_DIR"),
        "first-run-welcome",
        output.trim_end_matches('\n'),
    );
}
