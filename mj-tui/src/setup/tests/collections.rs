use super::*;

#[test]
fn this_machine_is_always_listed_and_cannot_be_removed() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    choose(&mut dashboard, "machines");
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    assert_eq!(dialog.keys(), ["local"]);
    choose(&mut dashboard, "local");
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    assert!(
        dialog
            .actions()
            .iter()
            .any(|(id, _, enabled)| *id == SetupControl::Remove && !enabled)
    );
    dashboard.handle_key(key(KeyCode::Delete));
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    assert!(dialog.draft["machines"].get("local").is_some());
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    assert_eq!(dialog.keys(), ["build_cache"]);
}

#[test]
fn the_sessionwiki_page_estimates_what_an_archive_window_would_reclaim() {
    use mj_core::state::ArchiveSpacePreview;
    let used = ArchiveSpacePreview {
        sessions: 40,
        bytes: 5_153_960_755,
        reclaimable_sessions: 0,
        reclaimable_bytes: 0,
    };
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    focus_setting(dialog, "sessionwiki");
    dialog.prepare();

    // Entering the page measures what sessions use now, exactly once.
    let DashboardAction::PreviewArchiveSpace {
        generation,
        older_than_days: None,
    } = dashboard.handle_key(key(KeyCode::Enter))
    else {
        panic!("entering the page must ask for the space sessions use");
    };
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Down)),
        DashboardAction::None
    );
    let mut terminal = Terminal::new(TestBackend::new(140, 30)).unwrap();
    let mut rendered = |dashboard: &mut DashboardState| {
        terminal
            .draw(|frame| crate::render::render(frame, dashboard))
            .unwrap();
        buffer_lines(terminal.backend().buffer()).join("\n")
    };
    assert!(rendered(&mut dashboard).contains("Resolving…"));

    dashboard.archive_space_previewed(generation, None, Ok(used.clone()));
    let never = rendered(&mut dashboard);
    assert!(
        never.contains("Never · sessions use 4.8G"),
        "the row must report the space sessions use:\n{never}"
    );

    // Typing a number asks again for that number, keystroke by keystroke.
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    focus_setting(dialog, "archive_after_days");
    dialog.prepare();
    dashboard.handle_key(key(KeyCode::Enter));
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Char('3'))),
        DashboardAction::PreviewArchiveSpace {
            generation,
            older_than_days: Some(3),
        }
    );
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Char('0'))),
        DashboardAction::PreviewArchiveSpace {
            generation,
            older_than_days: Some(30),
        }
    );

    // The answer for the value already typed past is dropped.
    dashboard.archive_space_previewed(
        generation,
        Some(3),
        Ok(ArchiveSpacePreview {
            reclaimable_sessions: 39,
            reclaimable_bytes: 5_000_000_000,
            ..used.clone()
        }),
    );
    let stale = rendered(&mut dashboard);
    assert!(
        !stale.contains("39 sessions"),
        "an answer for a value typed past must not be shown:\n{stale}"
    );

    dashboard.archive_space_previewed(
        generation,
        Some(30),
        Ok(ArchiveSpacePreview {
            reclaimable_sessions: 12,
            reclaimable_bytes: 1_288_490_188,
            ..used
        }),
    );
    // The estimate follows pending text and stays under the same field after apply.
    let editing = rendered(&mut dashboard);
    assert!(
        editing.contains("would reclaim 1.2G of 4.8G (12 of 40 sessions)")
            && !editing.contains("30 · would reclaim"),
        "the editor must show what the typed value would reclaim:\n{editing}"
    );
    dashboard.handle_key(key(KeyCode::Enter));
    let reclaim = rendered(&mut dashboard);
    assert!(
        reclaim.contains("would reclaim 1.2G of 4.8G (12 of 40 sessions)"),
        "the row must report what the saved value would reclaim:\n{reclaim}"
    );
}

/// A new SSH machine keeps its workspaces under Mjolnir's own directory, not
/// the product's former name. Launch campaign finding C-17.
// Hard-won: d0c3d418: new SSH machines defaulted workspaces under the former product-name directory.
#[test]
fn a_new_ssh_machine_keeps_workspaces_under_the_mjolnir_directory() {
    let machine = schema::defaults(
        &["machines".to_owned(), "box".to_owned()],
        &json!({"kind": "ssh"}),
    );
    assert_eq!(
        machine["workspace_prefix"],
        ".local/share/mjolnir/workspaces"
    );
}

#[test]
fn remove_targets_the_collection_entry_detail_page() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_settings_section("profiles", Some("codex-1"));
    click(&mut dashboard, "Remove");
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    assert_eq!(dialog.path, ["profiles"]);
    assert!(dialog.draft["profiles"].get("codex-1").is_none());
    assert!(dialog.draft["profiles"].get("claude-1").is_some());
    assert!(
        !dialog
            .actions()
            .iter()
            .any(|(id, _, _)| matches!(id, SetupControl::SignIn | SetupControl::Remove))
    );
}

#[test]
fn empty_optional_field_uses_default_but_environment_empty_string_is_preserved() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_settings_section("profiles", Some("codex-1"));
    choose(&mut dashboard, "guardian_review_model");
    replace_text(&mut dashboard, "custom");
    dashboard.handle_key(key(KeyCode::Enter));
    replace_text(&mut dashboard, "");
    dashboard.handle_key(key(KeyCode::Enter));
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    assert_eq!(
        dialog.draft["profiles"]["codex-1"]["guardian_review_model"],
        Value::Null
    );
    choose(&mut dashboard, "environment");
    activate(&mut dashboard, SetupControl::Add);
    dashboard.handle_paste("EMPTY");
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.handle_key(key(KeyCode::Enter));
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    assert_eq!(
        dialog.draft["profiles"]["codex-1"]["environment"]["EMPTY"],
        ""
    );
}

#[test]
fn deleting_an_array_entry_rebinds_pending_text_to_the_surviving_entry() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    dialog.draft["bundles"]["hel"]["repositories"] = json!([
        {"id":"first", "github":null, "local":null, "destination":"first"},
        {"id":"second", "github":null, "local":null, "destination":"second"}
    ]);
    choose(&mut dashboard, "bundles");
    choose(&mut dashboard, "hel");
    choose(&mut dashboard, "repositories");
    choose(&mut dashboard, "1");
    choose(&mut dashboard, "destination");
    replace_text(&mut dashboard, "second-pending");
    dashboard.handle_key(key(KeyCode::Esc));
    choose(&mut dashboard, "0");
    click(&mut dashboard, "Remove");
    choose(&mut dashboard, "0");
    choose(&mut dashboard, "destination");
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    assert_eq!(
        dialog.draft["bundles"]["hel"]["repositories"][0]["id"],
        "second"
    );
    assert_eq!(focused_field(dialog).input.value(), "second-pending");
}

// Hard-won: Settings live test: a scalar entry's pending buffer disabled its parent page shortcuts.
#[test]
fn environment_collection_shortcuts_remain_active_after_inline_edits() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    search(&mut dashboard, "Environment variables");
    dashboard.handle_key(key(KeyCode::Enter));
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    let collection = dialog.path.clone();
    assert_eq!(collection, ["profiles", "claude-1", "environment"]);
    assert!(dialog.keys().is_empty());
    for (name, pending) in [("LIVE_A", "pending-alpha"), ("LIVE_B", "pending-beta")] {
        dashboard.handle_key(key(KeyCode::Char('a')));
        let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
        assert!(
            dialog.editor.as_ref().is_some_and(|editor| editor.adding),
            "a must open Add even when the focused page button has a retained buffer"
        );
        dashboard.handle_paste(name);
        dashboard.handle_key(key(KeyCode::Enter));
        dashboard.handle_paste(pending);
        let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
        assert_eq!(focused_field(dialog).input.value(), pending);
        dashboard.handle_key(key(KeyCode::Esc));
        let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
        assert_eq!(dialog.path, collection);
        assert!(
            !dialog.text_input_focused(),
            "the entry is now a Page button"
        );
        assert!(dialog.is_dirty());
    }
    dashboard.handle_key(key(KeyCode::Char('/')));
    assert!(
        setup_dialog_mut(&mut dashboard.mode)
            .unwrap()
            .search
            .is_some(),
        "the collection search shortcut must also remain available"
    );
    dashboard.handle_key(key(KeyCode::Esc));
    choose(&mut dashboard, "LIVE_A");
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    assert_eq!(
        focused_field(dialog).input.value(),
        "pending-alpha",
        "navigation must retain the original edit"
    );
    dashboard.handle_key(key(KeyCode::Esc));
    dashboard.handle_key(key(KeyCode::Backspace));
    assert_eq!(
        setup_dialog_mut(&mut dashboard.mode).unwrap().path,
        ["profiles", "claude-1"],
        "Backspace must navigate from the collection's Page button"
    );
}
