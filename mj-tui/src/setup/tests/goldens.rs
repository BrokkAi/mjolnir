use super::*;

#[test]
fn golden_settings_root() {
    use std::fmt::Write as _;

    let mut output = String::new();

    let mut root = setup_at(&[]);
    let lines = append_setup_state(&mut output, "grouped Settings root", &mut root, 140, 42);
    let (save_x, save_y) = point(&lines, "  Save and Close  ");
    let (body_x, body_y) = point(&lines, "Runtimes");
    writeln!(
        output,
        "root footer alignment: save=({save_x},{save_y}); body=({body_x},{body_y})"
    )
    .expect("write root alignment");

    let mut interface = setup_at(&["interface"]);
    append_setup_state(
        &mut output,
        "virtual Interface page",
        &mut interface,
        100,
        30,
    );
    choose_by_keyboard(&mut interface, "theme");
    append_setup_state(
        &mut output,
        "Interface theme choices",
        &mut interface,
        100,
        30,
    );
    interface.handle_key(key(KeyCode::Down));
    interface.handle_key(key(KeyCode::Tab));
    append_setup_state(
        &mut output,
        "theme selection stays at the root config path",
        &mut interface,
        100,
        30,
    );
    let Mode::Setup(dialog) = &interface.mode else {
        panic!("settings");
    };
    writeln!(
        output,
        "draft paths: theme={}; interface.theme present={}; keys.prefix={}",
        dialog.draft["theme"],
        dialog.draft["interface"].get("theme").is_some(),
        dialog.draft["keys"]["prefix"]
    )
    .expect("write stored paths");

    // The same modal body is used by the root, nested pages, popups, editors,
    // and the Code Review child.
    let mut size_journey = setup_at(&[]);
    append_setup_state(&mut output, "compact root body", &mut size_journey, 100, 30);
    append_setup_body_size(&mut output, &size_journey);
    choose_by_keyboard(&mut size_journey, "interface");
    append_setup_state(
        &mut output,
        "compact nested page body",
        &mut size_journey,
        100,
        30,
    );
    append_setup_body_size(&mut output, &size_journey);
    choose_by_keyboard(&mut size_journey, "theme");
    append_setup_state(
        &mut output,
        "compact choice popup body",
        &mut size_journey,
        100,
        30,
    );
    append_setup_body_size(&mut output, &size_journey);
    size_journey.handle_key(key(KeyCode::Esc));
    size_journey.handle_key(key(KeyCode::Backspace));
    choose_by_keyboard(&mut size_journey, "advanced");
    append_setup_state(
        &mut output,
        "compact Advanced page body",
        &mut size_journey,
        100,
        30,
    );
    append_setup_body_size(&mut output, &size_journey);
    size_journey.handle_key(key(KeyCode::Backspace));
    choose_by_keyboard(&mut size_journey, "phone");
    choose_by_keyboard(&mut size_journey, "bind");
    append_setup_state(
        &mut output,
        "compact text editor body",
        &mut size_journey,
        100,
        30,
    );
    append_setup_body_size(&mut output, &size_journey);
    while !matches!(
        setup_dialog_mut(&mut size_journey.mode)
            .unwrap()
            .form
            .borrow()
            .focused(),
        Some(SetupControl::Back)
    ) {
        size_journey.handle_key(key(KeyCode::Tab));
    }
    size_journey.handle_key(key(KeyCode::Enter));
    choose_by_keyboard(&mut size_journey, "review");
    append_setup_state(
        &mut output,
        "compact Code Review body",
        &mut size_journey,
        100,
        30,
    );
    append_setup_body_size(&mut output, &size_journey);

    // Each page renders only the actions available at that location.
    for (label, path) in [
        ("root footer action", &[][..]),
        ("runtime collection actions", &["targets"][..]),
        ("project collection actions", &["bundles"][..]),
        ("profile collection actions", &["profiles"][..]),
        ("leaf page footer", &["phone"][..]),
        ("text editor actions", &["phone", "bind"][..]),
    ] {
        let mut page = setup_at(path);
        append_setup_state(&mut output, label, &mut page, 100, 30);
    }

    let mut runtime_actions = setup_at(&["targets"]);
    append_setup_state(
        &mut output,
        "runtime collection actions",
        &mut runtime_actions,
        100,
        30,
    );
    for _ in 0..10 {
        if setup_dialog_mut(&mut runtime_actions.mode)
            .unwrap()
            .form
            .borrow()
            .focused()
            == Some(SetupControl::Add)
        {
            break;
        }
        runtime_actions.handle_key(key(KeyCode::Tab));
    }
    let focused = setup_dialog_mut(&mut runtime_actions.mode)
        .unwrap()
        .form
        .borrow()
        .focused();
    assert_eq!(focused, Some(SetupControl::Add));
    append_setup_state(
        &mut output,
        "runtime Add reached from the keyboard",
        &mut runtime_actions,
        100,
        30,
    );
    writeln!(output, "focused control: {focused:?}").expect("write Add focus");
    let action = runtime_actions.handle_key(key(KeyCode::Enter));
    append_setup_state(
        &mut output,
        "runtime Add opens a new-entry editor",
        &mut runtime_actions,
        100,
        30,
    );
    let adding = setup_dialog_mut(&mut runtime_actions.mode)
        .unwrap()
        .editor
        .as_ref()
        .is_some_and(|editor| editor.adding);
    writeln!(output, "action: {action:?}; new entry: {adding}").expect("write new-entry result");

    let mut projects = setup_at(&["bundles"]);
    projects.handle_key(key(KeyCode::End));
    projects.handle_key(key(KeyCode::Tab));
    append_setup_state(
        &mut output,
        "project Create reached with Tab",
        &mut projects,
        100,
        24,
    );
    writeln!(
        output,
        "entry editor adding: {}",
        setup_dialog_mut(&mut projects.mode)
            .unwrap()
            .editor
            .as_ref()
            .is_some_and(|editor| editor.adding)
    )
    .expect("write editor state");
    let mut projects = setup_at(&["bundles"]);
    projects.handle_key(key(KeyCode::End));
    projects.handle_key(key(KeyCode::Down));
    append_setup_state(
        &mut output,
        "project Create reached with Down",
        &mut projects,
        100,
        24,
    );
    writeln!(
        output,
        "entry editor adding: {}",
        setup_dialog_mut(&mut projects.mode)
            .unwrap()
            .editor
            .as_ref()
            .is_some_and(|editor| editor.adding)
    )
    .expect("write editor state");

    // Theme values take effect only after a successful save, and are read
    // back by the next Settings dialog.
    let mut themed = dashboard_with_session(stopped_session());
    themed.begin_setup();
    choose_by_keyboard(&mut themed, "interface");
    choose_by_keyboard(&mut themed, "theme");
    themed.handle_key(key(KeyCode::Down));
    themed.handle_key(key(KeyCode::Tab));
    append_setup_state(
        &mut output,
        "Light selected while Midnight remains active",
        &mut themed,
        100,
        30,
    );
    writeln!(
        output,
        "{}",
        rendered_theme_colors(&mut themed, theme::UiTheme::Midnight)
    )
    .expect("write active theme colors");
    let action = themed.handle_key(crossterm::event::KeyEvent::new(
        KeyCode::Char('s'),
        KeyModifiers::CONTROL,
    ));
    let DashboardAction::SaveSetup {
        generation,
        updated,
        ..
    } = action
    else {
        panic!("expected Settings save, got {action:?}");
    };
    let saved: Config = serde_json::from_str(&updated).expect("saved config");
    writeln!(output, "saved theme: {}", saved.theme.label()).expect("write saved theme");
    append_setup_state(
        &mut output,
        "active theme before save acknowledgement",
        &mut themed,
        100,
        30,
    );
    writeln!(
        output,
        "{}",
        rendered_theme_colors(&mut themed, theme::UiTheme::Midnight)
    )
    .expect("write active theme colors");
    themed.setup_saved(generation, Ok(saved));
    append_setup_state(
        &mut output,
        "Light theme after save acknowledgement",
        &mut themed,
        100,
        30,
    );
    writeln!(
        output,
        "{}",
        rendered_theme_colors(&mut themed, theme::UiTheme::Light)
    )
    .expect("write applied theme colors");
    themed.begin_setup();
    choose_by_keyboard(&mut themed, "interface");
    choose_by_keyboard(&mut themed, "theme");
    append_setup_state(&mut output, "reopened theme choice", &mut themed, 100, 30);
    let Mode::Setup(dialog) = &themed.mode else {
        panic!("settings");
    };
    let (choices, selected) = open_choices(dialog);
    writeln!(output, "reopened selected theme: {}", choices[selected])
        .expect("write reopened theme");

    // Auto, the accessible and Catppuccin palettes, and the Windows Terminal
    // and terminal-color themes save and apply through the same flow,
    // starting from the reopened choice list. No terminal answers here, so
    // Auto draws Midnight and says why.
    for target in [
        theme::ThemeChoice::Auto,
        theme::UiTheme::HighContrastLight.into(),
        theme::UiTheme::ColorblindDark.into(),
        theme::UiTheme::ColorblindLight.into(),
        theme::UiTheme::CatppuccinMocha.into(),
        theme::UiTheme::CatppuccinLatte.into(),
        theme::UiTheme::Campbell.into(),
        theme::UiTheme::OneHalfDark.into(),
        theme::UiTheme::OneHalfLight.into(),
        theme::UiTheme::SolarizedDark.into(),
        theme::UiTheme::SolarizedLight.into(),
        theme::UiTheme::TerminalDark.into(),
        theme::UiTheme::TerminalLight.into(),
    ] {
        let wanted = serde_json::to_value(target).expect("theme serializes");
        let (current, position) = {
            let Mode::Setup(dialog) = &themed.mode else {
                panic!("settings");
            };
            let (choices, selected) = open_choices(dialog);
            (
                selected,
                choices
                    .iter()
                    .position(|choice| *choice == wanted)
                    .expect("theme offered"),
            )
        };
        let step = if position < current {
            KeyCode::Up
        } else {
            KeyCode::Down
        };
        for _ in 0..current.abs_diff(position) {
            themed.handle_key(key(step));
        }
        themed.handle_key(key(KeyCode::Tab));
        let action = themed.handle_key(crossterm::event::KeyEvent::new(
            KeyCode::Char('s'),
            KeyModifiers::CONTROL,
        ));
        let DashboardAction::SaveSetup {
            generation,
            updated,
            ..
        } = action
        else {
            panic!("expected Settings save, got {action:?}");
        };
        let saved: Config = serde_json::from_str(&updated).expect("saved config");
        writeln!(output, "saved theme: {}", saved.theme.label()).expect("write saved theme");
        themed.setup_saved(generation, Ok(saved));
        writeln!(
            output,
            "{} after save acknowledgement: {}",
            target.label(),
            rendered_theme_colors(
                &mut themed,
                theme::resolve(target, false, theme::terminal_background()).theme
            )
        )
        .expect("write applied theme colors");
        themed.begin_setup();
        choose_by_keyboard(&mut themed, "interface");
        choose_by_keyboard(&mut themed, "theme");
    }

    let mut managed = setup_at(&["profiles"]);
    click(&mut managed, "Set up managed");
    append_setup_state(&mut output, "managed agent buttons", &mut managed, 100, 30);
    managed.handle_key(key(KeyCode::Esc));
    assert!(
        setup_dialog_mut(&mut managed.mode)
            .unwrap()
            .managed_setup
            .is_none()
    );

    writeln!(output, "managed chooser Back returns to Agent Profiles").expect("write managed back");

    mj_core::golden::assert_golden(env!("CARGO_MANIFEST_DIR"), "settings-root", &output);
}

#[test]
fn golden_build_cache() {
    use std::fmt::Write as _;

    let mut output = String::new();

    let mut cache = setup_at(&["machines", "local", "build_cache"]);
    choose_by_keyboard(&mut cache, "max_total_size");
    for digit in ['2', '5'] {
        cache.handle_key(key(KeyCode::Char(digit)));
    }
    cache.handle_key(key(KeyCode::Enter));
    append_setup_state(
        &mut output,
        "cache budget entered in whole GB",
        &mut cache,
        140,
        30,
    );
    let dialog = setup_dialog_mut(&mut cache.mode).unwrap();
    dialog.draft["machines"]["local"]["build_cache"]["max_total_size"] = json!("100GiB");
    choose_by_keyboard(&mut cache, "max_total_size");
    append_setup_state(
        &mut output,
        "existing GiB value opens as whole GB",
        &mut cache,
        140,
        30,
    );
    writeln!(
        output,
        "editor value: {}",
        focused_field(setup_dialog_mut(&mut cache.mode).unwrap()).input
    )
    .expect("write converted value");
    for _ in 0..3 {
        cache.handle_key(key(KeyCode::Backspace));
    }
    for character in "10GiB".chars() {
        cache.handle_key(key(KeyCode::Char(character)));
    }
    cache.handle_key(key(KeyCode::Enter));
    append_setup_state(&mut output, "unit suffix refused", &mut cache, 140, 30);
    writeln!(
        output,
        "stored size after rejection: {}",
        setup_dialog_mut(&mut cache.mode).unwrap().draft["machines"]["local"]["build_cache"]["max_total_size"]
    )
    .expect("write preserved size");
    {
        let dialog = setup_dialog_mut(&mut cache.mode).unwrap();
        *focused_field_mut(dialog).input = mj_chat::text_input::TextInput::from("0".to_owned());
    }
    cache.handle_key(key(KeyCode::Enter));
    append_setup_state(
        &mut output,
        "zero refused with a default hint",
        &mut cache,
        140,
        30,
    );
    writeln!(
        output,
        "stored size after rejection: {}",
        setup_dialog_mut(&mut cache.mode).unwrap().draft["machines"]["local"]["build_cache"]["max_total_size"]
    )
    .expect("write preserved size");

    let mut saved_size = setup_at(&["machines", "local", "build_cache"]);
    choose_by_keyboard(&mut saved_size, "max_total_size");
    for digit in ['1', '2'] {
        saved_size.handle_key(key(KeyCode::Char(digit)));
    }
    saved_size.handle_key(key(KeyCode::Enter));
    let action = saved_size.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
    let DashboardAction::SaveSetup {
        generation,
        updated,
        ..
    } = action
    else {
        panic!("expected cache settings save");
    };
    let saved: Config = serde_json::from_str(&updated).expect("saved config");
    writeln!(
        output,
        "saved machine.local: {}",
        serde_json::to_value(&saved.machines["local"]).expect("machine settings")
    )
    .expect("write saved machine");
    saved_size.setup_saved(generation, Ok(saved));
    saved_size.begin_setup();
    choose_by_keyboard(&mut saved_size, "machines");
    choose_by_keyboard(&mut saved_size, "local");
    choose_by_keyboard(&mut saved_size, "build_cache");
    append_setup_state(
        &mut output,
        "all cache fields remain on the reopened page",
        &mut saved_size,
        140,
        30,
    );
    choose_by_keyboard(&mut saved_size, "max_total_size");
    append_setup_state(
        &mut output,
        "Use default clears only the optional budget",
        &mut saved_size,
        140,
        30,
    );
    replace_text(&mut saved_size, "");
    saved_size.handle_key(key(KeyCode::Enter));
    writeln!(
        output,
        "budget after Use default: {}",
        setup_dialog_mut(&mut saved_size.mode).unwrap().draft["machines"]["local"]["build_cache"]["max_total_size"]
    )
    .expect("write cleared budget");

    let mut scheduler = setup_at(&["machines", "local", "build_cache"]);
    choose_by_keyboard(&mut scheduler, "scheduler");
    append_setup_state(
        &mut output,
        "machine scheduler fields",
        &mut scheduler,
        140,
        30,
    );
    choose_by_keyboard(&mut scheduler, "cpus");
    for digit in ['1', '2'] {
        scheduler.handle_key(key(KeyCode::Char(digit)));
    }
    scheduler.handle_key(key(KeyCode::Enter));
    choose_by_keyboard(&mut scheduler, "memory");
    for character in "6GiB".chars() {
        scheduler.handle_key(key(KeyCode::Char(character)));
    }
    scheduler.handle_key(key(KeyCode::Enter));
    append_setup_state(
        &mut output,
        "scheduler values entered",
        &mut scheduler,
        140,
        30,
    );
    let DashboardAction::SaveSetup { updated, .. } =
        scheduler.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
    else {
        panic!("expected scheduler save");
    };
    let updated: Value = serde_json::from_str(&updated).expect("saved config");
    writeln!(
        output,
        "saved scheduler: {}",
        updated["machines"]["local"]["build_cache"]["scheduler"]
    )
    .expect("write saved scheduler");

    let mut tabbed = setup_at(&["machines", "local", "build_cache"]);
    let count = setup_dialog_mut(&mut tabbed.mode)
        .unwrap()
        .descriptors
        .len();
    for _ in 0..count {
        tabbed.handle_key(key(KeyCode::Tab));
    }
    assert!(
        setup_dialog_mut(&mut tabbed.mode)
            .unwrap()
            .form
            .borrow()
            .is_focused(SetupControl::Back)
    );
    append_setup_state(
        &mut output,
        "Build cache Tab reaches Back after its controls",
        &mut tabbed,
        140,
        30,
    );
    tabbed.handle_key(key(KeyCode::Tab));
    assert!(
        setup_dialog_mut(&mut tabbed.mode)
            .unwrap()
            .form
            .borrow()
            .is_focused(SetupControl::Save)
    );
    append_setup_state(
        &mut output,
        "Build cache Tab reaches Save",
        &mut tabbed,
        140,
        30,
    );
    let action = tabbed.handle_key(key(KeyCode::Enter));
    writeln!(
        output,
        "Tab-reached Save action: {}",
        matches!(action, DashboardAction::SaveSetup { .. })
    )
    .unwrap();

    let mut arrows = setup_at(&["machines", "local", "build_cache"]);
    for _ in 0..count {
        arrows.handle_key(key(KeyCode::Down));
    }
    assert!(
        setup_dialog_mut(&mut arrows.mode)
            .unwrap()
            .form
            .borrow()
            .is_focused(SetupControl::Back)
    );
    arrows.handle_key(key(KeyCode::Down));
    assert!(
        setup_dialog_mut(&mut arrows.mode)
            .unwrap()
            .form
            .borrow()
            .is_focused(SetupControl::Save)
    );
    append_setup_state(
        &mut output,
        "Build cache arrows reach footer",
        &mut arrows,
        140,
        30,
    );
    arrows.handle_key(key(KeyCode::Up));
    arrows.handle_key(key(KeyCode::Enter));
    assert_eq!(
        setup_dialog_mut(&mut arrows.mode).unwrap().path,
        ["machines", "local"]
    );
    append_setup_state(
        &mut output,
        "Up and Enter return from Save to the machine page",
        &mut arrows,
        140,
        30,
    );

    let mut shared = setup_at(&["machines", "local", "build_cache"]);
    choose_by_keyboard(&mut shared, "max_total_size");
    for digit in ['2', '5', '0'] {
        shared.handle_key(key(KeyCode::Char(digit)));
    }
    shared.handle_key(key(KeyCode::Enter));
    append_setup_state(
        &mut output,
        "shared storage budget uses GB",
        &mut shared,
        140,
        30,
    );

    // The machine row opens the same page through keyboard activation or a
    // double-click, and its summary reflects the saved machine policy.
    for mouse in [false, true] {
        let mut machine = setup_at(&["machines", "local"]);
        let mut lines = append_setup_state(
            &mut output,
            if mouse {
                "Build cache summary before mouse open"
            } else {
                "Build cache summary before keyboard open"
            },
            &mut machine,
            160,
            40,
        );
        if mouse {
            let location = point(&lines, "Build caches");
            for _ in 0..2 {
                for kind in [
                    MouseEventKind::Down(MouseButton::Left),
                    MouseEventKind::Up(MouseButton::Left),
                ] {
                    machine.handle_mouse(MouseEvent {
                        kind,
                        column: location.0,
                        row: location.1,
                        modifiers: KeyModifiers::NONE,
                    });
                    drawn(&mut machine, 160, 40);
                }
            }
        } else {
            choose_by_keyboard(&mut machine, "build_cache");
        }
        append_setup_state(
            &mut output,
            if mouse {
                "Build cache opened by mouse"
            } else {
                "Build cache opened by keyboard"
            },
            &mut machine,
            160,
            40,
        );
        let dialog = setup_dialog_mut(&mut machine.mode).unwrap();
        dialog.draft["machines"]["local"]["build_cache"]["enabled"] = json!(false);
        dialog.draft["machines"]["local"]["build_cache"]["max_total_size"] = json!("100GB");
        machine.handle_key(key(KeyCode::Esc));
        lines = append_setup_state(
            &mut output,
            if mouse {
                "disabled cache summary after mouse journey"
            } else {
                "disabled cache summary after keyboard journey"
            },
            &mut machine,
            160,
            40,
        );
        writeln!(
            output,
            "summary contains changed state: {}",
            lines
                .iter()
                .any(|line| line.contains("Disabled · 100 GB budget"))
        )
        .expect("write changed summary state");
    }

    mj_core::golden::assert_golden(env!("CARGO_MANIFEST_DIR"), "build-cache", &output);
}

/// A machine no runtime runs on is no session target, so its Machines row
/// says why. Detect runtimes asks every machine in the draft what it runs
/// and adds those runtimes under that machine.
#[test]
fn golden_machines_without_runtimes() {
    use mj_core::config::{DEFAULT_WORKSPACE_PREFIX, Machine, SshConnection, TargetTemplate};

    let ssh = |host: &str| SshConnection {
        host: host.to_owned(),
        user: Some("dev".to_owned()),
        identity_file: None,
        extra_args: vec![],
    };
    let machine = |host: &str| Machine::Ssh {
        ssh: ssh(host),
        workspace_prefix: DEFAULT_WORKSPACE_PREFIX.into(),
        build_cache: None,
    };
    let mut output = String::new();
    let mut dashboard = dashboard_with_session(stopped_session());
    let mut config = (*dashboard.config).clone();
    config
        .machines
        .insert("nas".into(), machine("nas.example.test"));
    dashboard.set_config(config);
    crate::test_support::answer_availability(&mut dashboard, |dashboard| {
        crate::test_support::availability_answer(dashboard, &[], &["nas"])
    });
    dashboard.begin_setup();
    choose(&mut dashboard, "machines");
    dashboard.handle_key(key(KeyCode::Char('a')));
    dashboard.handle_paste("builder");
    dashboard.handle_key(key(KeyCode::Enter));
    choose(&mut dashboard, "host");
    dashboard.handle_paste("builder.example.test");
    dashboard.handle_key(key(KeyCode::Enter));
    activate(&mut dashboard, SetupControl::Back);
    append_setup_state(
        &mut output,
        "machines without a runtime say why",
        &mut dashboard,
        140,
        34,
    );

    activate(&mut dashboard, SetupControl::Back);
    choose(&mut dashboard, "targets");
    let Mode::Setup(dialog) = &mut dashboard.mode else {
        panic!("settings");
    };
    dialog.form.get_mut().focus(SetupControl::DetectRuntimes);
    let generation = dialog.generation;
    // Detection asks every machine in the draft, the unsaved one too.
    let DashboardAction::DiscoverSetup { machines, .. } = dashboard.handle_key(key(KeyCode::Enter))
    else {
        panic!("Detect runtimes starts a detection");
    };
    assert_eq!(
        machines
            .iter()
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>(),
        ["builder", "nas"]
    );
    let Machine::Ssh { ssh: builder, .. } = &machines[0].1 else {
        panic!("builder is an SSH machine");
    };
    dashboard.setup_discovered(
        generation,
        Ok(crate::setup::SetupDetection {
            scope: DetectScope::Runtimes,
            config: Config::default(),
            rejected_runtimes: Vec::new(),
            machines: vec![
                crate::MachineRuntimeProbe {
                    machine_id: "builder".into(),
                    machine: machines[0].1.clone(),
                    finding: crate::MachineFinding::Answered {
                        runtimes: vec![
                            (
                                "builder".into(),
                                TargetTemplate::SshBare {
                                    ssh: builder.clone(),
                                    permissions: mj_core::config::PermissionMode::Guardian,
                                    workspace_prefix: DEFAULT_WORKSPACE_PREFIX.into(),
                                },
                            ),
                            (
                                "builder-docker".into(),
                                TargetTemplate::SshDocker {
                                    ssh: builder.clone(),
                                    container: mj_core::config::ContainerTemplate {
                                        build_cache: None,
                                        image: mj_client::target::DEFAULT_IMAGE.into(),
                                        pull_policy: Default::default(),
                                        platform: None,
                                        cpus: None,
                                        memory: None,
                                        environment: Default::default(),
                                        workspace_storage: Default::default(),
                                    },
                                },
                            ),
                        ],
                        rejected: vec![RejectedRuntime {
                            kind: "podman".into(),
                            label: "Podman".into(),
                            detail: "Podman is not installed.".into(),
                            remediation: None,
                        }],
                    },
                },
                crate::MachineRuntimeProbe {
                    machine_id: "nas".into(),
                    machine: machines[1].1.clone(),
                    finding: crate::MachineFinding::Unreachable {
                        detail: "`ssh dev@nas.example.test true` failed: Connection timed out"
                            .into(),
                        remediation: "Check that nas.example.test is up.".into(),
                    },
                },
            ],
        }),
    );
    append_setup_state(
        &mut output,
        "detection adds each machine's runtimes under it",
        &mut dashboard,
        140,
        34,
    );
    let Mode::Setup(dialog) = &dashboard.mode else {
        panic!("settings");
    };
    for (id, kind) in [("builder", "bare"), ("builder-docker", "docker")] {
        assert_eq!(dialog.draft["targets"][id]["kind"], kind);
        assert_eq!(dialog.draft["targets"][id]["machine"], "builder");
    }
    assert!(dialog.draft["targets"].get("nas").is_none());

    // Starting detection parks its button's activation while the button is
    // disabled. A real key retires that, as it does for a person who moves on
    // to the report button; Enter alone would replay the detect action.
    dashboard.handle_key(key(KeyCode::Tab));
    activate(&mut dashboard, SetupControl::DetectionResults);
    append_setup_state(
        &mut output,
        "detection results lists each refusal and its fix",
        &mut dashboard,
        140,
        34,
    );
    dashboard.handle_key(key(KeyCode::End));
    dashboard.handle_key(key(KeyCode::Esc));
    append_setup_state(
        &mut output,
        "esc from detection results returns to runtimes",
        &mut dashboard,
        140,
        34,
    );

    dashboard.handle_key(key(KeyCode::Esc));
    choose(&mut dashboard, "machines");
    append_setup_state(
        &mut output,
        "only the unreachable machine still lacks a runtime",
        &mut dashboard,
        140,
        34,
    );
    mj_core::golden::assert_golden(
        env!("CARGO_MANIFEST_DIR"),
        "machines-without-runtimes",
        &output,
    );
}

#[test]
fn golden_settings_search() {
    use std::fmt::Write as _;

    let mut output = String::new();

    let mut memory = setup_at(&[]);
    search(&mut memory, "memory");
    append_setup_state(
        &mut output,
        "search memory from Settings root",
        &mut memory,
        140,
        30,
    );
    writeln!(output, "matching paths: {:?}", result_paths(&mut memory))
        .expect("write search results");
    memory.handle_key(key(KeyCode::Enter));
    append_setup_state(
        &mut output,
        "memory result focuses its inline field",
        &mut memory,
        140,
        30,
    );
    let landed_page = setup_dialog_mut(&mut memory.mode).unwrap().path.clone();
    let landed_control = setup_dialog_mut(&mut memory.mode).unwrap().focused_path();
    writeln!(
        output,
        "landed page and focused setting: {:?} / {:?}",
        landed_page, landed_control
    )
    .expect("write result destination");

    let mut compaction = setup_at(&[]);
    search(&mut compaction, "compaction");
    append_setup_state(
        &mut output,
        "help text finds context budget by compaction",
        &mut compaction,
        140,
        30,
    );
    writeln!(
        output,
        "matching paths: {:?}",
        result_paths(&mut compaction)
    )
    .expect("write help-text results");

    let mut image = setup_at(&[]);
    search(&mut image, "image");
    append_setup_state(
        &mut output,
        "search result includes its section, label, and current value",
        &mut image,
        140,
        30,
    );
    writeln!(output, "matching paths: {:?}", result_paths(&mut image))
        .expect("write image results");

    mj_core::golden::assert_golden(env!("CARGO_MANIFEST_DIR"), "settings-search", &output);
}

#[test]
fn golden_global_subagent_discovery_and_save() {
    use mj_core::profile_capabilities::{
        CapabilityState, ProfileCapabilities, ProfileCapabilitiesSnapshot,
    };
    use mj_core::worker_launch::ProfileConfig;
    let mut dashboard = setup_at(&["subagents"]);
    let choice = |value: &str| mj_core::acp::SessionConfigChoice {
        value: value.into(),
        name: value.into(),
        description: None,
    };
    let profiles = dashboard
        .config
        .enabled_profiles()
        .enumerate()
        .map(|(index, (id, profile))| {
            let model = if index == 0 { "shared" } else { "smarter" };
            (
                profile.capabilities_key(id),
                ProfileCapabilities {
                    choices: CapabilityState::Ready(ProfileConfig {
                        model: Some(model.into()),
                        models: vec![choice("shared"), choice(model)],
                        efforts: vec![],
                        observed_at: 1,
                    }),
                    efforts: std::collections::BTreeMap::from([
                        (
                            "shared".into(),
                            CapabilityState::Ready(vec![choice("high")]),
                        ),
                        (
                            model.into(),
                            CapabilityState::Ready(vec![choice("high"), choice("max")]),
                        ),
                    ]),
                },
            )
        })
        .collect();
    let snapshot = ProfileCapabilitiesSnapshot { profiles };
    dashboard.set_profile_capabilities(snapshot.clone());
    assert!(!setup_dialog_mut(&mut dashboard.mode).unwrap().is_dirty());
    let mut output = String::new();
    append_setup_state(&mut output, "global task choices", &mut dashboard, 140, 40);
    for task in ["straightforward", "subtle"] {
        assert!(
            setup_dialog_mut(&mut dashboard.mode).unwrap().draft["subagents"][task]["model"]
                .is_null()
        );
    }
    assert!(setup_dialog_mut(&mut dashboard.mode).unwrap().draft["subagents"]["straightforward"]["effort"].is_null());
    assert!(
        setup_dialog_mut(&mut dashboard.mode).unwrap().draft["subagents"]["subtle"]["effort"]
            .is_null()
    );
    for (task, model) in [("straightforward", "shared"), ("subtle", "smarter")] {
        let model_path = vec!["subagents".into(), task.into(), "model".into()];
        {
            let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
            dialog.prepare();
            dialog.focus_path(&model_path);
        }
        dashboard.handle_key(key(KeyCode::Enter));
        let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
        assert_eq!(
            open_choices(dialog)
                .0
                .iter()
                .filter(|c| **c == "shared")
                .count(),
            1
        );
        select_choice(&mut dashboard, model);
        dashboard.take_setup_subagent_choices();
        let effort_path = vec!["subagents".into(), task.into(), "effort".into()];
        setup_dialog_mut(&mut dashboard.mode)
            .unwrap()
            .focus_path(&effort_path);
        dashboard.handle_key(key(KeyCode::Enter));
        select_choice(&mut dashboard, "high");
        append_setup_state(&mut output, task, &mut dashboard, 140, 40);
    }
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    let DashboardAction::SaveSetup { updated, .. } = dialog.save(&Default::default()) else {
        panic!("save");
    };
    let saved: Config = serde_json::from_str(&updated).unwrap();
    assert!(saved.subagents.is_configured());
    assert_eq!(saved.subagents.subtle.unwrap().model, "smarter");
    dialog.saving = false;
    dialog.path = vec!["subagents".into()];
    // Discovery arriving while the picker is open must retain a saved selection.
    dashboard.profile_capabilities = Default::default();
    dashboard.take_setup_subagent_choices();
    let effort_path = vec![
        "subagents".into(),
        "straightforward".into(),
        "effort".into(),
    ];
    setup_dialog_mut(&mut dashboard.mode)
        .unwrap()
        .focus_path(&effort_path);
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.set_profile_capabilities(snapshot);
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    let (values, selected) = open_choices(dialog);
    assert_eq!(values[selected], "high");
    dashboard.handle_key(key(KeyCode::Enter));
    let model_path = vec!["subagents".into(), "straightforward".into(), "model".into()];
    setup_dialog_mut(&mut dashboard.mode)
        .unwrap()
        .focus_path(&model_path);
    dashboard.handle_key(key(KeyCode::Enter));
    select_choice(&mut dashboard, "smarter");
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    assert!(dialog.draft["subagents"]["straightforward"]["effort"].is_null());
    setup_dialog_mut(&mut dashboard.mode)
        .unwrap()
        .focus_path(&model_path);
    dashboard.handle_key(key(KeyCode::Enter));
    select_value(&mut dashboard, &Value::Null);
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    let config = config_from_draft(dialog.draft.clone()).unwrap();
    assert!(config.subagents.straightforward.is_none());
    assert!(!config.subagents.is_configured());
    assert!(dialog.draft["subagents"]["straightforward"]["effort"].is_null());
    let before_rejected_save = dialog.draft.clone();
    let _ = dialog;
    append_setup_state(&mut output, "cleared choice", &mut dashboard, 140, 40);

    let action = setup_dialog_mut(&mut dashboard.mode)
        .unwrap()
        .save(&Default::default());
    let DashboardAction::SaveSetup {
        generation,
        updated,
        ..
    } = action
    else {
        panic!("expected background validation save");
    };
    let invalid: Config = serde_json::from_str(&updated).unwrap();
    let validation_error = invalid.validate().unwrap_err().to_string();
    dashboard.setup_saved(generation, Err(validation_error));
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    assert_eq!(dialog.path, ["subagents"]);
    assert_eq!(dialog.draft, before_rejected_save);
    assert!(!dialog.saving);
    assert!(
        dialog
            .notice
            .as_deref()
            .is_some_and(|notice| notice.starts_with("Could not save:"))
    );
    assert!(dashboard.config.subagents.straightforward.is_none());
    append_setup_state(
        &mut output,
        "invalid save keeps draft and reports validation",
        &mut dashboard,
        140,
        40,
    );
    mj_core::golden::assert_golden(env!("CARGO_MANIFEST_DIR"), "global-subagents", &output);
}
