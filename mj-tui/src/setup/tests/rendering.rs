use super::*;

/// Every on/off setting uses the same checkbox, whichever section holds it.
/// Launch campaign finding C-3.
// Hard-won: e03accb6: boolean settings used On/Off text instead of checkbox markers.
#[test]
fn every_boolean_setting_is_drawn_as_a_checkbox() {
    for (section, label) in [
        ("continuation", "Enabled"),
        ("github.watch", "Enabled"),
        ("jev", "Enabled"),
        ("phone", "Enabled"),
        ("phone", "Detect Tailscale"),
        ("notify", "Ring the terminal bell"),
        ("notify", "Show counts in the terminal title"),
        ("advanced", "Detailed activity clocks"),
    ] {
        let mut dashboard = dashboard_with_session(stopped_session());
        dashboard.begin_settings_section(section.split('.').next().unwrap(), None);
        if section == "github.watch" {
            setup_dialog_mut(&mut dashboard.mode)
                .unwrap()
                .path
                .push("watch".into());
        }
        setup_dialog_mut(&mut dashboard.mode).unwrap().prepare();
        let lines = drawn(&mut dashboard, 140, 40);
        let row = lines
            .iter()
            .find(|line| line.contains(label))
            .unwrap_or_else(|| panic!("{section}: missing {label:?}: {lines:#?}"));
        assert!(
            row.contains('☒') || row.contains('☐'),
            "{section} › {label} is a checkbox: {row:?}"
        );
        assert!(
            !row.contains(" On ") && !row.contains(" Off "),
            "{section} › {label} has no On/Off text: {row:?}"
        );
    }
}

/// Continuation runs only while Jev is on (`Config::automatic_continuation_enabled`).
/// With Jev off, the Continuation row and page say it is off and why, and
/// point to the Privacy page, rather than "On" and a ticked box alone.
/// Launch re-verification finding R3-2.
// Hard-won: 76289ac9: the continuation row implied it remained active while jev was off.
#[test]
fn continuation_reads_as_off_while_jev_is_off() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.config.jev.enabled = false;
    dashboard.begin_setup();
    let lines = drawn(&mut dashboard, 140, 48);
    let row = lines
        .iter()
        .find(|line| line.contains("Continuation"))
        .unwrap_or_else(|| panic!("missing row: {lines:#?}"));
    assert!(row.contains("Off"), "{row:?}");
    assert!(row.contains("Jev"), "{row:?}");
    assert!(!row.contains("On ·"), "{row:?}");

    dashboard.begin_settings_section("continuation", None);
    setup_dialog_mut(&mut dashboard.mode).unwrap().prepare();
    let page = drawn(&mut dashboard, 140, 40).join("\n");
    assert!(page.contains("Jev is off"), "{page}");
    assert!(page.contains("Privacy"), "{page}");

    // With Jev on, the page and row keep their ordinary text.
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    let lines = drawn(&mut dashboard, 140, 48);
    let row = lines
        .iter()
        .find(|line| line.contains("Continuation"))
        .unwrap();
    assert!(row.contains("On · 3 continuations"), "{row:?}");
}

#[test]
fn github_events_read_as_off_while_jev_is_off() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.config.jev.enabled = false;
    dashboard.begin_setup();
    let lines = drawn(&mut dashboard, 140, 48);
    let row = lines
        .iter()
        .find(|line| line.contains("GitHub events"))
        .unwrap_or_else(|| panic!("missing row: {lines:#?}"));
    assert!(row.contains("Off"), "{row:?}");
    assert!(row.contains("Jev"), "{row:?}");
    assert!(!row.contains("On ·"), "{row:?}");

    dashboard.begin_settings_section("github", None);
    setup_dialog_mut(&mut dashboard.mode)
        .unwrap()
        .path
        .push("watch".into());
    setup_dialog_mut(&mut dashboard.mode).unwrap().prepare();
    let page = drawn(&mut dashboard, 140, 40).join("\n");
    assert!(page.contains("Jev is off"), "{page}");
    assert!(page.contains("child reports"), "{page}");
    let help = schema::help(&["github".to_owned(), "watch".to_owned()]);
    assert!(help.contains("GitHub activity"), "{help}");
    assert!(help.contains("child reports"), "{help}");
    assert!(help.contains("tool boundaries"), "{help}");
    assert!(help.contains("remain available"), "{help}");

    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    let lines = drawn(&mut dashboard, 140, 48);
    let row = lines
        .iter()
        .find(|line| line.contains("GitHub events"))
        .unwrap();
    assert!(row.contains("On · GitHub activity"), "{row:?}");
}

/// The breadcrumb names each page the way the page above it named its row, so
/// a machine the user called `local` is not retitled as the repository setting
/// that shares the key.
// Hard-won: a9226786: a user-chosen machine name collided with a settings key in the breadcrumb.
#[test]
fn the_breadcrumb_shows_a_user_chosen_name_as_the_user_wrote_it() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    choose(&mut dashboard, "machines");
    choose(&mut dashboard, "local");
    let text = drawn(&mut dashboard, 140, 30).join("\n");
    assert!(text.contains("Settings › Machines › local"), "{text}");
    assert!(
        !text.contains("Local repository directory"),
        "the machine's name went through the label table:\n{text}"
    );
    choose(&mut dashboard, "build_cache");
    let text = drawn(&mut dashboard, 140, 30).join("\n");
    assert!(
        text.contains("Settings › Machines › local › Build caches"),
        "a schema key below it still gets its label:\n{text}"
    );
}

/// Launch campaign finding A-9 / C-4: the Continuation section has a short
/// name, its row shows its whole value, and its page shows its whole
/// description.
// Hard-won: 70f7a0f5: the continuation label and description were clipped.
#[test]
fn continuation_section_has_a_short_name_a_whole_value_and_a_whole_description() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    let text = drawn(&mut dashboard, 100, 30).join("\n");
    let row = text
        .lines()
        .find(|line| line.contains("Continuation"))
        .unwrap_or_else(|| panic!("no Continuation row in {text}"));
    assert!(
        row.contains("Continuation ") && row.contains(" On · 3 continuations plus quota recovery"),
        "{row}"
    );

    dashboard.begin_settings_section("continuation", None);
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal
        .draw(|frame| crate::render::render(frame, &mut dashboard))
        .unwrap();
    let body = dashboard
        .frame_surfaces()
        .surface(mj_chat::selection::SurfaceId::ModalBody)
        .expect("rendered Settings modal surface")
        .rect;
    let buffer = terminal.backend().buffer();
    // Join only the modal's cells: backdrop text beside a wrapped help line
    // must not become part of its description.
    let text = (body.y..body.bottom())
        .map(|y| {
            (body.x..body.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join(" ");
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        text.contains(schema::help(&["continuation".to_owned()])),
        "{text}"
    );
}

/// A-14: with `NO_COLOR` set the screen is monochrome whatever theme is
/// configured, so Setup says so instead of naming a theme it is not drawing.
/// The configured theme is still named, because it returns once `NO_COLOR`
/// is unset.
// Hard-won: 584574a5: Setup claimed monochrome mode while the renderer still applied its theme.
#[test]
fn setup_reports_monochrome_while_no_color_overrides_the_theme() {
    let midnight = theme::ThemeChoice::Fixed(theme::UiTheme::Midnight);
    assert_eq!(
        schema::theme_report(midnight, true, None),
        "Monochrome (NO_COLOR; configured: Midnight)"
    );
    assert_eq!(schema::theme_report(midnight, false, None), "Midnight");
    // The override is the one the renderer applies.
    assert_eq!(
        theme::resolve(midnight, true, None).theme,
        theme::UiTheme::Mono
    );
    // Auto names the palette it drew and what the terminal said.
    assert_eq!(
        schema::theme_report(theme::ThemeChoice::Auto, false, Some((253, 246, 227))),
        "Auto: Light (terminal background #fdf6e3)"
    );
    assert_eq!(
        schema::theme_report(theme::ThemeChoice::Auto, true, Some((253, 246, 227))),
        "Monochrome (NO_COLOR; configured: Auto)"
    );
}

/// An open dropdown shows its value once on the row, in the dropdown.
/// Launch campaign finding C-16.
// Hard-won: 27dfd170: an open dropdown rendered the selected value twice.
#[test]
fn an_open_dropdown_draws_its_value_once() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_settings_section("notify", None);
    choose(&mut dashboard, "mode");
    let lines = drawn(&mut dashboard, 140, 40);
    let row = lines
        .iter()
        .find(|line| line.contains("Notify through"))
        .unwrap_or_else(|| panic!("no Notify through row:\n{}", lines.join("\n")));
    assert_eq!(row.matches("terminal").count(), 1, "{row}");
}

/// Every description fits the two rows above a page at the narrowest width
/// Settings takes, so none stops mid-sentence. Launch campaign finding C-18.
// Hard-won: 524f4307: setting descriptions were clipped by the two-row layout.
#[test]
fn every_setting_description_fits_its_two_rows() {
    let paths: &[&[&str]] = &[
        &["interface", "prefix"],
        &["theme"],
        &["profiles"],
        &["profiles", "p", "home"],
        &["machines"],
        &["targets"],
        &["targets", "t", "machine"],
        &["phone"],
        &["advanced"],
        &["notify"],
        &["notify", "mode"],
        &["notify", "bell"],
        &["notify", "delay_seconds"],
        &["notify", "title"],
        &["advanced", "detailed_activity_clocks"],
        &["advanced", "session_order"],
        &["advanced", "symbols"],
        &["bundles"],
        &["bundles", "b", "repositories"],
        &["review"],
        &["continuation"],
        &["jev"],
        &["sessionwiki"],
        &["sessionwiki", "archive_after_days"],
        &["subagents"],
        &["machines", "m", "build_cache"],
        &["machines", "m", "build_cache", "directory"],
        &["machines", "m", "build_cache", "tools_directory"],
        &["machines", "m", "build_cache", "max_total_size"],
        &["machines", "m", "build_cache", "scheduler"],
        &["machines", "m", "build_cache", "scheduler", "cpus"],
        &["machines", "m", "build_cache", "scheduler", "memory"],
        &["targets", "t", "memory"],
        &["targets", "t", "pull_policy"],
        &["profiles", "p", "context_window_bytes"],
        &["profiles", "p", "guardian_review_model"],
        &[],
        &["other"],
    ];
    // The dialog is at least 64 columns wide, less its border and margin.
    let width = 60;
    let overflowing = paths
        .iter()
        .map(|path| path.iter().map(|key| (*key).to_owned()).collect::<Vec<_>>())
        .filter(|path| {
            ratatui::widgets::Paragraph::new(schema::help(path))
                .wrap(Wrap { trim: false })
                .line_count(width)
                > 2
        })
        .map(|path| path.join("."))
        .collect::<Vec<_>>();
    assert!(overflowing.is_empty(), "{overflowing:#?}");
    let jev_off = json!({"jev": {"enabled": false}});
    let continuation = schema::page_help(&["continuation".to_owned()], &jev_off);
    assert!(
        ratatui::widgets::Paragraph::new(continuation)
            .wrap(Wrap { trim: false })
            .line_count(width)
            <= 2,
        "{continuation}"
    );
}

/// A page lists its settings in one fixed order, whichever of them the file
/// happens to store and in whatever order. Launch campaign finding C-24.
// Hard-won: 9a0d40d3: editing a value changed the settings row order.
#[test]
fn a_page_lists_its_settings_in_a_fixed_order() {
    let order = |draft: serde_json::Value, path: &[&str]| {
        let mut draft = draft;
        schema::expand(&mut draft, &mut Vec::new());
        let path = path.iter().map(|key| (*key).to_owned()).collect::<Vec<_>>();
        visible_keys(&path, draft.pointer(&pointer(&path)).unwrap())
    };
    assert_eq!(
        order(json!({"phone": {"tailscale_detect": false}}), &["phone"]),
        order(json!({"phone": {}}), &["phone"]),
    );
    assert_eq!(
        order(
            json!({"machines": {"local": {"kind": "local", "build_cache": {"max_total_size": "20GB", "enabled": false}}}}),
            &["machines", "local", "build_cache"],
        ),
        [
            "enabled",
            "directory",
            "tools_directory",
            "max_total_size",
            "scheduler"
        ],
    );
    assert_eq!(
        order(
            json!({"machines": {"local": {"kind": "local", "build_cache": {"scheduler": {"memory": "8GiB"}}}}}),
            &["machines", "local", "build_cache", "scheduler"],
        ),
        ["cpus", "memory"],
    );
    assert_eq!(
        order(
            json!({"machines": {"box": {"workspace_prefix": "w", "host": "h", "kind": "ssh"}}}),
            &["machines", "box"],
        ),
        order(
            json!({"machines": {"box": {"kind": "ssh"}}}),
            &["machines", "box"]
        ),
    );
}

/// Launch campaign finding A-4: with `symbols = "ascii"`, dialog titles and
/// hints kept `·`, popup headers kept `↑/↓`, notices kept `…`, and Settings
/// fields kept `▾`. Every page and popup below is drawn with the ASCII set
/// and must contain nothing else.
// Hard-won: 71a1ad4b: ASCII mode left literal Unicode glyphs in rendered settings.
#[test]
fn ascii_symbols_draw_settings_and_dialogs_without_non_ascii_text() {
    fn assert_ascii(context: &str, lines: &[String]) {
        let offenders = lines
            .iter()
            .flat_map(|line| line.chars())
            .filter(|character| !character.is_ascii())
            .collect::<std::collections::BTreeSet<_>>();
        assert!(
            offenders.is_empty(),
            "{context}: non-ASCII {offenders:?}\n{lines:#?}"
        );
    }
    fn ascii_dashboard() -> DashboardState {
        let mut dashboard = dashboard_with_session(stopped_session());
        let mut config = dashboard.config.clone();
        config.advanced.symbols = Some(mj_core::config::SymbolSet::Ascii);
        dashboard.set_config(config);
        dashboard
    }

    let mut dashboard = ascii_dashboard();
    dashboard.begin_setup();
    let roots = setup_dialog_mut(&mut dashboard.mode).unwrap().keys();
    assert_ascii("settings root", &drawn(&mut dashboard, 120, 40));
    for root in &roots {
        let mut dashboard = ascii_dashboard();
        dashboard.begin_settings_section(root, None);
        assert_ascii(&format!("settings {root}"), &drawn(&mut dashboard, 120, 40));
        assert_ascii(
            &format!("settings {root} narrow"),
            &drawn(&mut dashboard, 80, 30),
        );
    }
    // A field's popup and its header.
    for (section, field) in [
        ("advanced", "symbols"),
        ("advanced", "session_order"),
        ("interface", "theme"),
        ("interface", "spinner"),
        ("interface", "sessions_side"),
    ] {
        let mut dashboard = ascii_dashboard();
        dashboard.begin_settings_section(section, None);
        choose(&mut dashboard, field);
        let lines = drawn(&mut dashboard, 100, 30);
        assert!(
            lines.iter().any(|line| line.contains("^/v select")),
            "{field}: the popup header is drawn: {lines:#?}"
        );
        assert_ascii(&format!("settings {section} {field} popup"), &lines);
    }

    // The dashboard's own dialogs.
    for command in [
        crate::CommandId::Palette,
        crate::CommandId::NewSessionWizard,
        crate::CommandId::Workspaces,
        crate::CommandId::ResumeDialog,
    ] {
        let mut dashboard = ascii_dashboard();
        dashboard.dispatch_command(command);
        assert_ascii(&format!("{command:?}"), &drawn(&mut dashboard, 120, 40));
        assert_ascii(
            &format!("{command:?} narrow"),
            &drawn(&mut dashboard, 80, 30),
        );
    }
}
