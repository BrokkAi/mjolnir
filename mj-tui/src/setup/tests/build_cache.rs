use super::*;

#[test]
fn the_build_cache_page_shows_the_values_its_host_resolves_for_blank_fields() {
    use mj_core::state::{BuildCacheLimit, BuildCachePreview};
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.config.targets.insert(
        "podman-host".into(),
        serde_json::from_value(json!({"kind":"local-podman","image":"example/image:latest"}))
            .unwrap(),
    );
    dashboard.begin_setup();
    choose(&mut dashboard, "machines");
    let DashboardAction::PreviewBuildCache {
        generation,
        key: preview_key,
        ..
    } = choose(&mut dashboard, "local")
    else {
        panic!("preview build cache");
    };
    let Mode::Setup(dialog) = &mut dashboard.mode else {
        panic!("settings");
    };
    focus_setting(dialog, "build_cache");
    dialog.prepare();
    // Opening the machine starts the host lookup; opening its cache page does
    // not start a second one.
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::None
    );
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Down)),
        DashboardAction::None
    );
    let mut terminal = Terminal::new(TestBackend::new(140, 30)).unwrap();
    terminal
        .draw(|frame| crate::render::render(frame, &mut dashboard))
        .unwrap();
    let resolving = buffer_lines(terminal.backend().buffer()).join("\n");
    assert!(resolving.contains("Resolving…"), "{resolving}");

    dashboard.build_cache_previewed(
        generation,
        &preview_key,
        Ok(Some(BuildCachePreview {
            user_managed: false,
            application: Default::default(),
            budget_note: None,
            native_mbx: Some("1.12.0".into()),
            mbx_profile_file: None,
            mbx_profile_warning: None,
            mbx_manual_path_line: None,
            directory: Some("/mnt/fast/mbx-cache".into()),
            max_total_size: Some(BuildCacheLimit::HostConfiguration(Some("500GiB".into()))),
            stats: None,
            off_reason: Some(mj_core::state::BuildCacheOff::Unavailable(
                "the filesystem under /mnt/fast/mbx-cache does not support reflinks".into(),
            )),
        })),
        false,
    );
    terminal
        .draw(|frame| crate::render::render(frame, &mut dashboard))
        .unwrap();
    let resolved = buffer_lines(terminal.backend().buffer()).join("\n");
    for expected in [
        "Enabled",
        "Default (on)",
        "Effective: Off",
        "mbx: the filesystem under /mnt/fast/mbx-cache does not support reflinks",
        "/mnt/fast/mbx-cache",
        "537 GB, host mbx config",
        "mbx build cache is unavailable: the filesystem under",
    ] {
        assert!(
            resolved.contains(expected),
            "missing {expected:?} in\n{resolved}"
        );
    }

    dashboard.build_cache_previewed(
        generation,
        &preview_key,
        Ok(Some(BuildCachePreview {
            native_mbx: Some("1.26.0".into()),
            mbx_profile_file: None,
            mbx_profile_warning: None,
            mbx_manual_path_line: None,
            directory: None,
            max_total_size: None,
            user_managed: false,
            application: Default::default(),
            budget_note: None,
            stats: None,
            off_reason: Some(mj_core::state::BuildCacheOff::UnsupportedPlatform(
                "mbx 1.26.0 runs natively on this Mac at /Users/builder/.local/bin/mbx; bare \
                 sessions here use it when it is on their login PATH. Container sessions run \
                 without the shared build cache."
                    .into(),
            )),
        })),
        false,
    );
    terminal
        .draw(|frame| crate::render::render(frame, &mut dashboard))
        .unwrap();
    let unsupported = buffer_lines(terminal.backend().buffer()).join("\n");
    assert!(
        unsupported.contains("mbx: mbx 1.26.0 runs natively on this Mac"),
        "{unsupported}"
    );
    // Hard-won: #1330: a Mac's working native mbx was reported as missing.
    assert!(!unsupported.contains("no mbx on the host"), "{unsupported}");
    assert!(
        !unsupported.contains("Pending application"),
        "{unsupported}"
    );
    assert!(!unsupported.contains("Mj-managed mbx"), "{unsupported}");

    // The status line belongs to this page: leaving it takes the line along.
    let shown = setup_dialog_mut(&mut dashboard.mode)
        .expect("settings")
        .notice
        .clone();
    assert!(
        shown
            .as_deref()
            .is_some_and(|notice| notice.contains("build cache")),
        "{shown:?}"
    );
    dashboard.handle_key(key(KeyCode::Esc));
    let left = setup_dialog_mut(&mut dashboard.mode).expect("settings");
    assert_eq!(
        left.notice, None,
        "the build cache status outlived its page"
    );
    let dialog = setup_dialog_mut(&mut dashboard.mode).expect("settings");
    focus_setting(dialog, "build_cache");
    dialog.prepare();
    dashboard.handle_key(key(KeyCode::Enter));

    // Native tool caches remain selectable without mbx.
    let dialog = setup_dialog_mut(&mut dashboard.mode).expect("settings");
    focus_setting(dialog, "enabled");
    dialog.prepare();
    dashboard.handle_key(key(KeyCode::Enter));
    select_value(&mut dashboard, &Value::Bool(false));
    let dialog = setup_dialog_mut(&mut dashboard.mode).expect("settings");
    assert_eq!(
        dialog.draft["machines"]["local"]["build_cache"]["enabled"],
        Value::Bool(false),
        "the switch controls native tools even when mbx is unavailable"
    );

    // Changing a setting on the page makes the answer stale and asks again.
    let Mode::Setup(dialog) = &mut dashboard.mode else {
        panic!("settings");
    };
    dialog.draft["machines"]["local"]["build_cache"]["max_total_size"] = json!("1GiB");
    assert!(matches!(
        dashboard.handle_key(key(KeyCode::Down)),
        DashboardAction::PreviewBuildCache { .. }
    ));
}

/// On a host that supports the cache the switch is a checkbox: on, off, and
/// back to unset, which means on.
// Hard-won: 39cce3de: the build-cache setting lacked a checkbox control.
#[test]
fn build_cache_enabled_round_trips_default_on_and_off_with_host_stats() {
    use mj_core::state::{BuildCacheLimit, BuildCachePreview};
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    choose(&mut dashboard, "machines");
    let DashboardAction::PreviewBuildCache {
        generation,
        key: preview_key,
        ..
    } = choose(&mut dashboard, "local")
    else {
        panic!("preview build cache");
    };
    let dialog = setup_dialog_mut(&mut dashboard.mode).expect("settings");
    focus_setting(dialog, "build_cache");
    dialog.prepare();
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::None
    );
    dashboard.build_cache_previewed(
        generation,
        &preview_key,
        Ok(Some(BuildCachePreview {
            user_managed: false,
            application: Default::default(),
            budget_note: None,
            native_mbx: Some("1.12.0".into()),
            mbx_profile_file: None,
            mbx_profile_warning: None,
            mbx_manual_path_line: None,
            directory: Some("/home/dev/.cache/mbx".into()),
            max_total_size: Some(BuildCacheLimit::Size("100000000000B".into())),
            stats: Some(mj_core::state::BuildCacheStats {
                builds: 155,
                cached_compilations: 12050,
                avoided_compiler_ns: 6_004_997_818_721,
                reflinked_bytes: 47_612_059_386,
            }),
            off_reason: None,
        })),
        false,
    );
    let checked = drawn(&mut dashboard, 140, 30).join("\n");
    assert!(checked.contains("Default (on)"), "{checked}");
    // The page otherwise only predicts; this line is the one thing on it that
    // says the cache is being used.
    for expected in [
        "155 builds",
        "12050 compilations from cache",
        "1h 40m",
        "44.3",
    ] {
        assert!(
            checked.contains(expected),
            "missing {expected:?} in\n{checked}"
        );
    }
    assert!(
        !checked.contains("Off:"),
        "a supported host explains nothing:\n{checked}"
    );

    for value in [Value::Bool(true), Value::Bool(false), Value::Null] {
        choose(&mut dashboard, "enabled");
        select_value(&mut dashboard, &value);
        let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
        assert_eq!(
            dialog.draft["machines"]["local"]["build_cache"]["enabled"],
            value
        );
        let saved = saved_config(dialog);
        assert_eq!(
            serde_json::to_value(saved).unwrap()["machines"]["local"]["build_cache"]["enabled"],
            value
        );
    }
}

/// Test-and-fix M-3: the machine's list row said "Enabled by default" while
/// the build cache page, opened next, said the host cannot share the cache.
// Hard-won: b4c3a95d: the machine row contradicted the build-cache page on unsupported hosts.
#[test]
fn machine_row_reports_an_unsupported_cache_host_like_its_page() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    choose(&mut dashboard, "machines");
    choose(&mut dashboard, "local");
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    // Opening the machine resolves its host, so the row does not have to wait
    // for the build cache page to be opened.
    let (generation, key) = {
        let (_, key) = dialog.build_cache_machine_page().unwrap();
        (dialog.generation, key)
    };
    assert_eq!(
        dialog.build_cache_previews.get("local").map(|p| &p.key),
        Some(&key)
    );
    dashboard.build_cache_previewed(
        generation,
        &key,
        Ok(Some(mj_core::state::BuildCachePreview {
            native_mbx: None,
            mbx_profile_file: None,
            mbx_profile_warning: None,
            mbx_manual_path_line: None,
            directory: None,
            max_total_size: None,
            user_managed: false,
            application: Default::default(),
            budget_note: None,
            stats: None,
            off_reason: Some(mj_core::state::BuildCacheOff::UnsupportedPlatform(
                "Shared mbx requires a Linux host".into(),
            )),
        })),
        false,
    );
    let lines = drawn(&mut dashboard, 160, 40);
    assert!(
        lines.iter().any(|line| line.contains("Build caches")
            && line.contains("mbx unavailable")
            && !line.contains("Enabled by default")),
        "{lines:#?}"
    );

    // A Mac reports the mbx its bare sessions use, not a missing one.
    dashboard.build_cache_previewed(
        generation,
        &key,
        Ok(Some(mj_core::state::BuildCachePreview {
            native_mbx: Some("1.26.0".into()),
            mbx_profile_file: None,
            mbx_profile_warning: None,
            mbx_manual_path_line: None,
            directory: None,
            max_total_size: None,
            user_managed: false,
            application: Default::default(),
            budget_note: None,
            stats: None,
            off_reason: Some(mj_core::state::BuildCacheOff::UnsupportedPlatform(
                "mbx 1.26.0 runs natively on this Mac".into(),
            )),
        })),
        false,
    );
    let lines = drawn(&mut dashboard, 160, 40);
    assert!(
        lines
            .iter()
            .any(|line| line.contains("Build caches")
                && line.contains("mbx 1.26.0, bare sessions only")),
        "{lines:#?}"
    );
    assert!(
        lines
            .iter()
            .all(|line| !line.contains("mbx unavailable") && !line.contains("no mbx on the host")),
        "{lines:#?}"
    );
}

#[test]
fn machine_page_confirms_mbx_install_shows_busy_and_refreshes_after_success() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    choose(&mut dashboard, "machines");
    let DashboardAction::PreviewBuildCache {
        generation,
        key: preview_key,
        ..
    } = choose(&mut dashboard, "local")
    else {
        panic!("preview build cache");
    };
    dashboard.build_cache_previewed(
        generation,
        &preview_key,
        Ok(Some(mj_core::state::BuildCachePreview {
            native_mbx: None,
            mbx_profile_file: Some("~/.profile".into()),
            mbx_profile_warning: Some(
                "Your login shell may not read ~/.profile. Add this PATH line to a startup file it reads:".into(),
            ),
            mbx_manual_path_line: Some(
                "export PATH='/home/test/.local/share/mbx/bin:/home/test/.local/bin'${PATH:+:$PATH}".into(),
            ),
            directory: None,
            max_total_size: None,
            user_managed: false,
            application: Default::default(),
            budget_note: None,
            stats: None,
            off_reason: None,
        })),
        true,
    );
    assert!(
        drawn(&mut dashboard, 140, 34)
            .join("\n")
            .contains("Install mbx")
    );

    activate(&mut dashboard, SetupControl::InstallMbx);
    let Mode::Confirm(confirm) = &dashboard.mode else {
        panic!("confirmation");
    };
    let (_, lines) = crate::dialogs::confirmation_body(&confirm.confirmation, None, 72);
    let prompt = lines
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    for expected in [
        "the local machine",
        "mbx setup --yes",
        "~/.profile",
        "may not read ~/.profile",
        "export PATH='/home/test/.local/share/mbx/bin:/home/test/.local/bin'${PATH:+:$PATH}",
    ] {
        assert!(prompt.contains(expected), "missing {expected:?}: {prompt}");
    }

    dashboard.handle_key(key(KeyCode::Right));
    let DashboardAction::InstallMbx {
        generation: confirmed_generation,
        key: confirmed_key,
        machine_id,
        ..
    } = dashboard.handle_key(key(KeyCode::Enter))
    else {
        panic!("confirmed mbx install");
    };
    assert_eq!(confirmed_generation, generation);
    assert_eq!(confirmed_key, preview_key);
    assert_eq!(machine_id, "local");
    assert!(dashboard.mbx_install_started(generation, &preview_key, "local"));
    let busy = drawn(&mut dashboard, 140, 34).join("\n");
    assert!(busy.contains("Installing mbx…"), "{busy}");

    dashboard.mbx_install_finished(
        generation,
        &preview_key,
        "local",
        Ok("Installed mbx at /home/test/.local/bin/mbx (version 1.22.0). Updated ~/.profile. Your login shell may not read ~/.profile. Add this PATH line: export PATH='/home/test/.local/share/mbx/bin:/home/test/.local/bin'${PATH:+:$PATH}".into()),
        Ok(Some(mj_core::state::BuildCachePreview {
            native_mbx: Some("1.22.0".into()),
            mbx_profile_file: None,
            mbx_profile_warning: None,
            mbx_manual_path_line: None,
            directory: Some("/home/test/.cache/mbx".into()),
            max_total_size: None,
            user_managed: true,
            application: Default::default(),
            budget_note: None,
            stats: None,
            off_reason: None,
        })),
        false,
    );
    let finished = drawn(&mut dashboard, 140, 34).join("\n");
    assert!(
        finished.contains("Installed mbx at /home/test/.local/bin/mbx"),
        "{finished}"
    );
    assert!(finished.contains("Updated ~/.profile"), "{finished}");
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    assert!(
        !dialog
            .actions()
            .iter()
            .any(|(control, _, _)| *control == SetupControl::InstallMbx)
    );
}

#[test]
fn machine_page_labels_old_mbx_as_upgrade_and_hides_unavailable_actions() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    choose(&mut dashboard, "machines");
    let DashboardAction::PreviewBuildCache {
        generation,
        key: preview_key,
        ..
    } = choose(&mut dashboard, "local")
    else {
        panic!("preview build cache");
    };
    dashboard.build_cache_previewed(
        generation,
        &preview_key,
        Ok(Some(mj_core::state::BuildCachePreview {
            native_mbx: Some("1.21.0".into()),
            mbx_profile_file: Some("~/.bash_profile".into()),
            mbx_profile_warning: None,
            mbx_manual_path_line: None,
            directory: None,
            max_total_size: None,
            user_managed: true,
            application: Default::default(),
            budget_note: None,
            stats: None,
            off_reason: Some(mj_core::state::BuildCacheOff::Unavailable(
                "the host's mbx 1.21.0 is older than the pinned release".into(),
            )),
        })),
        true,
    );
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    assert!(
        dialog
            .actions()
            .iter()
            .any(
                |(control, label, enabled)| *control == SetupControl::InstallMbx
                    && *label == "Upgrade mbx"
                    && *enabled
            )
    );

    let preview = dialog.build_cache_previews.get_mut("local").unwrap();
    preview.install_mbx_available = false;
    dialog.prepare();
    assert!(
        !dialog
            .actions()
            .iter()
            .any(|(control, _, _)| *control == SetupControl::InstallMbx)
    );
    let preview = dialog.build_cache_previews.get_mut("local").unwrap();
    preview.result =
        BuildCachePreviewResult::Ready(Some(Box::new(mj_core::state::BuildCachePreview {
            native_mbx: None,
            mbx_profile_file: None,
            mbx_profile_warning: None,
            mbx_manual_path_line: None,
            directory: None,
            max_total_size: None,
            user_managed: false,
            application: Default::default(),
            budget_note: None,
            stats: None,
            off_reason: Some(mj_core::state::BuildCacheOff::UnsupportedPlatform(
                "Shared mbx requires a Linux host".into(),
            )),
        })));
    dialog.prepare();
    assert!(
        !dialog
            .actions()
            .iter()
            .any(|(control, _, _)| *control == SetupControl::InstallMbx)
    );
}

#[test]
fn machine_page_reports_mbx_install_failure_and_preserves_the_retry_action() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    choose(&mut dashboard, "machines");
    let DashboardAction::PreviewBuildCache {
        generation,
        key: preview_key,
        ..
    } = choose(&mut dashboard, "local")
    else {
        panic!("preview build cache");
    };
    let absent = || mj_core::state::BuildCachePreview {
        native_mbx: None,
        mbx_profile_file: Some("~/.profile".into()),
        mbx_profile_warning: None,
        mbx_manual_path_line: None,
        directory: None,
        max_total_size: None,
        user_managed: false,
        application: Default::default(),
        budget_note: None,
        stats: None,
        off_reason: None,
    };
    dashboard.build_cache_previewed(generation, &preview_key, Ok(Some(absent())), true);
    assert!(dashboard.mbx_install_started(generation, &preview_key, "local"));
    dashboard.mbx_install_finished(
        generation,
        &preview_key,
        "local",
        Err("mbx setup exited with status 1: setup refused".into()),
        Ok(Some(absent())),
        true,
    );
    let lines = drawn(&mut dashboard, 140, 34).join("\n");
    assert!(
        lines.contains("Could not install mbx: mbx setup exited"),
        "{lines}"
    );
    assert!(lines.contains("Install mbx"), "{lines}");
}

// Hard-won: Settings review: off-page Save bypassed a late mbx ownership reply.
#[test]
fn host_owned_pending_cache_edits_are_discarded_before_saving_from_another_page() {
    for (reply_on_cache_page, save_started) in [(true, false), (false, false), (false, true)] {
        let mut dashboard = dashboard_with_session(stopped_session());
        dashboard.config.machines.insert(
            "builder".into(),
            serde_json::from_value(json!({"kind":"ssh", "host":"builder"})).unwrap(),
        );
        dashboard.begin_setup();
        choose(&mut dashboard, "machines");
        choose(&mut dashboard, "local");
        choose(&mut dashboard, "build_cache");
        choose(&mut dashboard, "max_total_size");
        replace_text(&mut dashboard, "500");
        if save_started {
            choose(&mut dashboard, "directory");
            replace_text(&mut dashboard, "~/cache");
            assert!(matches!(
                dashboard.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
                DashboardAction::ResolveSetupPath { .. }
            ));
        }
        let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
        let (generation, key) = (dialog.generation, dialog.build_cache_page().unwrap().1);
        let path = vec![
            "machines".into(),
            "local".into(),
            "build_cache".into(),
            "max_total_size".into(),
        ];
        assert!(dialog.is_dirty());
        if !reply_on_cache_page {
            for _ in 0..3 {
                dashboard.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
            }
            choose(&mut dashboard, "machines");
            choose(&mut dashboard, "builder");
        }
        dashboard.build_cache_previewed(
            generation,
            &key,
            Ok(Some(mj_core::state::BuildCachePreview {
                native_mbx: Some("1.21.0".into()),
                mbx_profile_file: None,
                mbx_profile_warning: None,
                mbx_manual_path_line: None,
                directory: Some("/native/cache".into()),
                max_total_size: None,
                user_managed: true,
                application: mj_core::state::BuildCacheApplication::Applied,
                budget_note: None,
                stats: None,
                off_reason: None,
            })),
            false,
        );
        let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
        assert!(
            !dialog.fields.contains_key(&path),
            "unfocusable buffers must not remain pending"
        );
        assert!(
            !dialog.is_dirty(),
            "the discarded pending edit was the only change"
        );
        assert!(dialog.notice.as_deref().is_some_and(
            |notice| notice.contains("Discarded pending")
                && notice.contains("local")
                && notice.contains("mbx installation owns")
        ));
        while !setup_dialog_mut(&mut dashboard.mode)
            .unwrap()
            .path
            .is_empty()
        {
            dashboard.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        }
        let action = if save_started {
            dashboard
                .take_prerequisite_check()
                .expect("the accepted Save resumes after its owned field is dropped")
        } else {
            dashboard.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
        };
        let DashboardAction::SaveSetup { updated, .. } = action else {
            panic!("discarded host-owned edits should not block saving");
        };
        let saved: Value = serde_json::from_str(&updated).unwrap();
        assert!(saved["machines"]["local"]["build_cache"]["directory"].is_null());
        assert!(
            saved["machines"]["local"]["build_cache"]["max_total_size"].is_null(),
            "the discarded edit must never reach the saved config: {updated}"
        );
    }
}

// Hard-won: Settings review round 2: off-page installs retained their pre-install preview.
#[test]
fn mbx_install_completion_refreshes_its_machine_after_navigation() {
    let preview = |installed: bool| mj_core::state::BuildCachePreview {
        native_mbx: installed.then(|| "1.22.0".to_owned()),
        mbx_profile_file: (!installed).then(|| "~/.profile".to_owned()),
        mbx_profile_warning: None,
        mbx_manual_path_line: None,
        directory: installed.then(|| "/native/cache".into()),
        max_total_size: None,
        user_managed: installed,
        application: Default::default(),
        budget_note: None,
        stats: None,
        off_reason: None,
    };
    for (refresh_succeeds, draft_changes) in
        [(true, false), (false, false), (true, true), (false, true)]
    {
        let mut dashboard = dashboard_with_session(stopped_session());
        dashboard.config.machines.insert(
            "builder".into(),
            serde_json::from_value(json!({"kind":"ssh", "host":"builder"})).unwrap(),
        );
        dashboard.begin_setup();
        choose(&mut dashboard, "machines");
        let DashboardAction::PreviewBuildCache {
            generation,
            key: local_key,
            ..
        } = choose(&mut dashboard, "local")
        else {
            panic!("local preview");
        };
        dashboard.build_cache_previewed(generation, &local_key, Ok(Some(preview(false))), true);
        assert!(dashboard.mbx_install_started(generation, &local_key, "local"));
        if draft_changes {
            choose(&mut dashboard, "build_cache");
            choose(&mut dashboard, "enabled");
            select_value(&mut dashboard, &Value::Bool(false));
            dashboard.handle_key(key(KeyCode::Esc));
        }
        dashboard.handle_key(key(KeyCode::Esc));
        assert!(matches!(
            choose(&mut dashboard, "builder"),
            DashboardAction::PreviewBuildCache { .. }
        ));
        let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
        let local_before = dialog.build_cache_previews["local"].clone();
        let builder_before = dialog.build_cache_previews["builder"].clone();
        dashboard.mbx_install_finished(
            generation,
            &local_key,
            "local",
            Ok("Installed mbx".into()),
            if refresh_succeeds {
                Ok(Some(preview(true)))
            } else {
                Err("host preview unavailable".into())
            },
            false,
        );
        let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
        assert_eq!(
            dialog.build_cache_previews["builder"], builder_before,
            "install completion must only update its own machine"
        );
        assert!(dialog.mbx_install.is_none());
        if draft_changes {
            assert_eq!(
                dialog.build_cache_previews["local"], local_before,
                "a stale install key must not overwrite or invalidate the newer resolution"
            );
            continue;
        }
        if refresh_succeeds {
            assert!(
                matches!(&dialog.build_cache_previews["local"].result, BuildCachePreviewResult::Ready(Some(fresh)) if fresh.native_mbx.as_deref() == Some("1.22.0") && fresh.user_managed)
            );
            assert!(!dialog.build_cache_previews["local"].install_mbx_available);
        } else {
            assert!(
                !dialog.build_cache_previews.contains_key("local"),
                "a failed post-install refresh must invalidate the pre-install cache"
            );
            assert!(
                dialog.notice.as_deref().unwrap().contains(
                    "Could not refresh the build cache preview: host preview unavailable"
                )
            );
        }
        dashboard.handle_key(key(KeyCode::Esc));
        let revisit = choose(&mut dashboard, "local");
        if refresh_succeeds {
            assert_eq!(revisit, DashboardAction::None);
        } else {
            assert!(
                matches!(revisit, DashboardAction::PreviewBuildCache { .. }),
                "revisit must retry after invalidation"
            );
            dashboard.build_cache_previewed(generation, &local_key, Ok(Some(preview(true))), false);
        }
        let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
        assert!(
            !dialog
                .actions()
                .iter()
                .any(|(id, _, _)| *id == SetupControl::InstallMbx)
        );
        choose(&mut dashboard, "build_cache");
        let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
        for field in ["directory", "max_total_size"] {
            assert!(
                dialog.descriptors.iter().any(|control| control
                    .path
                    .last()
                    .is_some_and(|key| key == field)
                    && control.kind == SettingKind::ReadOnly),
                "fresh ownership must protect {field}"
            );
        }
    }
}
