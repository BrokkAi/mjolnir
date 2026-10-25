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
        dashboard.welcome_configured(vec!["Agents: Claude Code.".into()]);
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
    dashboard.welcome_configured(vec!["Agents: Codex.".into()]);
    dashboard.welcome_checked(Vec::new());
    assert!(matches!(dashboard.mode, Mode::Setup(_)));
    dashboard.cancel_modal();
    let text = drawn(&mut dashboard, 100, 30).join("\n");
    assert!(text.contains("Welcome to Mjolnir"));
    assert!(text.contains("Agents: Codex."));
}

#[test]
fn welcome_retains_background_results_under_help() {
    let mut dashboard = dashboard();
    dashboard.begin_welcome();
    dashboard.dispatch_command(CommandId::Help);
    dashboard.welcome_configured(vec!["Agents: Claude Code.".into()]);
    dashboard.welcome_checked(vec!["Login required.".into()]);
    let Mode::Help(help) = &dashboard.mode else {
        panic!("help closed");
    };
    let Mode::Welcome(welcome) = help.return_to.as_ref() else {
        panic!("welcome lost");
    };
    assert!(welcome.lines.iter().any(|line| line == "Login required."));
}

#[test]
fn welcome_offers_managed_setup_when_no_agent_was_found() {
    let mut dashboard = dashboard();
    dashboard.begin_welcome();
    dashboard.welcome_configured(vec![
        "No installed coding agent was found. Mjolnir can install and sign in to one for you."
            .into(),
    ]);
    dashboard.welcome_managed_setup_available(true);
    dashboard.welcome_checked(Vec::new());
    let lines = drawn(&mut dashboard, 110, 32);
    assert!(
        lines
            .iter()
            .any(|line| line.contains("No installed coding agent"))
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
