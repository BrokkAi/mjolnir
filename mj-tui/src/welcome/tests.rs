use super::*;
use crate::test_support::{drawn, key};

fn dashboard() -> DashboardState {
    DashboardState::new(
        mj_core::config::Config::default().with_local_targets(),
        mj_core::state::State::default(),
        Default::default(),
    )
}

#[test]
fn welcome_renders_discovery_results_and_errors_without_creating_a_session() {
    let mut dashboard = dashboard();
    dashboard.begin_welcome();
    let text = drawn(&mut dashboard, 100, 30).join("\n");
    assert!(text.contains("Welcome to Mjolnir"));
    assert!(text.contains("Finding your agents"));
    dashboard.welcome_configured(vec![
        "Agents: Codex, Claude Code.".into(),
        "Project: BrokkAi/mjolnir.".into(),
    ]);
    dashboard.welcome_checked(vec![
        "Claude needs login.\nRun mj login --profile claude".into(),
    ]);
    let text = drawn(&mut dashboard, 100, 30).join("\n");
    assert!(text.contains("Agents: Codex, Claude Code."));
    assert!(text.contains("Project: BrokkAi/mjolnir."));
    assert!(text.contains("Run mj login --profile claude"));
    assert!(text.contains("Continue"));
    assert!(text.contains("ctrl+b c"));
    assert!(!text.contains("Checking prerequisites"));
    assert!(dashboard.state.sessions.is_empty());
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
