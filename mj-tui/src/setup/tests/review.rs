use super::*;

#[test]
fn off_review_with_no_available_model_can_save_setup() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    choose(&mut dashboard, "review");
    let Mode::Setup(setup) = &dashboard.mode else {
        panic!("setup")
    };
    assert!(
        setup
            .review_editor
            .as_ref()
            .unwrap()
            .review
            .reviewers
            .is_empty()
    );
    assert!(setup.review_editor.as_ref().unwrap().can_save());
    let action = dashboard.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
    assert!(matches!(action, DashboardAction::SaveSetup { .. }));
}
