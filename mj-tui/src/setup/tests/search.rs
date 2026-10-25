use super::*;

#[test]
fn search_selects_a_switch_without_flipping_it() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    search(&mut dashboard, "detailed activity");
    dashboard.handle_key(key(KeyCode::Enter));
    let dialog = setup_dialog_mut(&mut dashboard.mode).expect("settings");
    assert_eq!(dialog.path, vec!["advanced".to_owned()]);
    assert_eq!(
        dialog
            .focused_path()
            .as_ref()
            .and_then(|p| p.last())
            .map(String::as_str),
        Some("detailed_activity_clocks"),
        "the switch is left selected on its own page"
    );
    assert!(dialog.editor.is_none());
    assert_eq!(
        dialog.draft["advanced"]["detailed_activity_clocks"],
        Value::Bool(false),
        "finding a setting must not change it"
    );
}

#[test]
fn typing_in_the_search_does_not_reach_the_page_shortcuts() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_settings_section("profiles", None);
    let before = setup_dialog_mut(&mut dashboard.mode)
        .expect("settings")
        .keys()
        .len();
    search(&mut dashboard, "");
    // On a collection page a bare "a" adds an entry; inside the query it is
    // just a letter.
    dashboard.handle_key(key(KeyCode::Char('a')));
    assert_eq!(search_state(&mut dashboard).input.value(), "a");
    let dialog = setup_dialog_mut(&mut dashboard.mode).expect("settings");
    assert!(dialog.editor.is_none(), "no entry was added");
    assert_eq!(dialog.keys().len(), before);
}

#[test]
fn cache_search_aliases_find_every_machine_and_preserve_the_draft() {
    for query in ["mbx", "cache", "build cache"] {
        let mut dashboard = dashboard_with_session(stopped_session());
        dashboard.config.machines.insert("builder".into(), serde_json::from_value(json!({
            "kind":"ssh", "host":"builder.example.com", "build_cache":{"enabled":true,"max_total_size":"50GB"}
        })).unwrap());
        dashboard.begin_setup();
        let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
        dialog.draft["phone"]["enabled"] = json!(false);
        search(&mut dashboard, query);
        let paths = result_paths(&mut dashboard);
        for machine in ["local", "builder"] {
            assert!(
                paths
                    .iter()
                    .any(|path| path == &["machines", machine, "build_cache"]),
                "{query}: {paths:?}"
            );
        }
        assert!(!paths.iter().any(|path| path == &["build_cache"]));
        let search = search_state(&mut dashboard);
        let index = search
            .matches
            .iter()
            .position(|index| search.entries[*index].path == ["machines", "builder", "build_cache"])
            .unwrap();
        let entry = &search.entries[search.matches[index]];
        assert_eq!(entry.trail, "Machines › builder");
        assert_eq!(entry.value, "Enabled · 50 GB budget  ›");
        setup_dialog_mut(&mut dashboard.mode)
            .unwrap()
            .search
            .as_mut()
            .unwrap()
            .selected = index;
        dashboard.handle_key(key(KeyCode::Enter));
        let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
        assert_eq!(dialog.path, ["machines", "builder", "build_cache"]);
        assert_eq!(dialog.draft["phone"]["enabled"], false);
        assert!(dialog.is_dirty());
    }
}

#[test]
fn search_focuses_nullable_dropdowns_without_opening_or_changing_them() {
    for (query, path) in [
        ("theme", vec!["interface", "theme"]),
        (
            "build caches enabled",
            vec!["machines", "local", "build_cache", "enabled"],
        ),
    ] {
        let mut dashboard = dashboard_with_session(stopped_session());
        dashboard.begin_setup();
        let before = setup_dialog_mut(&mut dashboard.mode).unwrap().draft.clone();
        search(&mut dashboard, query);
        let paths = result_paths(&mut dashboard);
        let target = paths
            .iter()
            .position(|candidate| {
                candidate
                    .iter()
                    .map(String::as_str)
                    .eq(path.iter().copied())
            })
            .unwrap_or_else(|| panic!("{query}: {paths:?}"));
        for _ in 0..target {
            dashboard.handle_key(key(KeyCode::Down));
        }
        dashboard.handle_key(key(KeyCode::Enter));
        let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
        assert_eq!(dialog.focused_path().unwrap(), path);
        assert!(dialog.combo.open_id().is_none());
        assert_eq!(dialog.draft, before);
    }
}

// Hard-won: Settings live test: the first search-result click only selected its row.
#[test]
fn search_results_open_on_one_click_across_the_label_trail_and_value() {
    for (query, label, trail, expected, page) in [
        (
            "Ring the terminal bell",
            "Ring the terminal bell",
            "Notifications",
            vec!["notify", "bell"],
            false,
        ),
        (
            "Theme",
            "Theme",
            "Interface",
            vec!["interface", "theme"],
            false,
        ),
        ("Interface", "Interface", "", vec!["interface"], true),
        (
            "Enabled",
            "Enabled",
            "codex-1",
            vec!["profiles", "codex-1", "enabled"],
            false,
        ),
    ] {
        // Each position starts with a fresh dialog and click history. A second
        // click elsewhere on the same row must not be required for activation.
        for offset in [0, 8, 40, 70, 90] {
            let mut dashboard = dashboard_with_session(stopped_session());
            dashboard.begin_setup();
            let before = setup_dialog_mut(&mut dashboard.mode).unwrap().draft.clone();
            search(&mut dashboard, query);
            let lines = drawn(&mut dashboard, 140, 42);
            let row = lines
                .iter()
                .rposition(|line| line.contains(label) && line.contains(trail))
                .unwrap_or_else(|| panic!("missing {label:?} result: {lines:#?}"));
            let column = cell_column(&lines[row], label) + offset;
            for kind in [
                MouseEventKind::Down(MouseButton::Left),
                MouseEventKind::Up(MouseButton::Left),
            ] {
                dashboard.handle_mouse(MouseEvent {
                    kind,
                    column,
                    row: row as u16,
                    modifiers: KeyModifiers::NONE,
                });
                drawn(&mut dashboard, 140, 42);
            }
            let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
            assert!(
                dialog.search.is_none(),
                "{query:?} at offset {offset} must open on its first click"
            );
            if page {
                assert_eq!(dialog.path, expected);
            } else {
                assert_eq!(
                    dialog.focused_path().unwrap(),
                    expected,
                    "a click must open the clicked result, including a result not previously selected"
                );
            }
            assert!(
                dialog.combo.open_id().is_none(),
                "search must only focus dropdowns"
            );
            assert_eq!(
                dialog.draft, before,
                "search must not toggle or change a value"
            );
        }
    }
}
