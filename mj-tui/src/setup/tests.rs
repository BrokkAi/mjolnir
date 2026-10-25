use super::*;
use crate::test_support::{
    buffer_lines, cell_column, chord, config, dashboard_with_session, drawn, key, point,
    running_session, stopped_session,
};
use crossterm::event::{KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{Terminal, backend::TestBackend};

// Managed onboarding must not overwrite existing detected profiles or require
// the user to choose a home path.
#[test]
fn managed_setup_creates_a_collision_free_profile_and_requests_login() {
    let mut dashboard = dashboard_with_session(stopped_session());
    let mut existing = dashboard.config.profiles.values().next().unwrap().clone();
    existing.kind = mj_core::config::HarnessKind::Claude;
    dashboard.config.profiles.insert("claude".into(), existing);
    dashboard.begin_settings_section("profiles", None);
    setup_dialog_mut(&mut dashboard.mode)
        .unwrap()
        .begin_managed_setup();
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    dialog.managed_setup.as_mut().unwrap().selected = 1;

    let action = dialog.create_managed_profile_and_login(&Default::default());

    let DashboardAction::SaveSetupAndLogin {
        updated,
        profile_id,
        ..
    } = action
    else {
        panic!("managed setup did not request save and login: {action:?}");
    };
    assert_eq!(profile_id, "claude-2");
    let saved: Value = serde_json::from_str(&updated).unwrap();
    let profile = &saved["profiles"][&profile_id];
    assert_eq!(profile["kind"], "claude");
    let home = profile["home"].as_str().unwrap();
    assert!(std::path::Path::new(home).is_absolute(), "{home}");
    assert!(
        std::path::Path::new(home).ends_with("profiles/claude-2/claude"),
        "{home}"
    );
}

// Hard-won: 8d16bf96: unrelated settings saves failed when a configured API-key file disappeared.
#[test]
fn settings_save_api_key_profiles_without_resolving_credentials_on_the_ui_thread() {
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path().join("codex");
    std::fs::create_dir(&home).unwrap();
    std::fs::write(
        home.join("config.toml"),
        "model_provider = 'deepseek'\n[model_providers.deepseek]\nname = 'DeepSeek'\nbase_url = 'https://api.deepseek.com/v1'\nenv_key = 'DEEPSEEK_API_KEY'\nwire_api = 'responses'\n",
    )
    .unwrap();
    let path = directory.path().join("config.toml");
    let secrets = directory.path().join("secrets.toml");
    std::fs::write(&secrets, "DEEPSEEK_API_KEY = 'test-secret'\n").unwrap();
    let mut stored = serde_json::to_value(Config::default()).unwrap();
    stored["profiles"] = json!({"deepseek": {
        "kind": "codex", "home": home,
        "environment": {"DEEPSEEK_API_KEY": {"from_secret": "DEEPSEEK_API_KEY"}}
    }});
    let config: Config = mj_core::config::with_secret_resolver(
        mj_core::config::SecretResolver::beside(&path),
        || serde_json::from_value(stored),
    )
    .unwrap();
    config.validate().unwrap();
    // Opening and saving must work even when credentials are unavailable to
    // the UI. The background saver resolves and validates before writing.
    std::fs::remove_file(secrets).unwrap();
    let mut dialog = SetupDialog::new(&config);
    assert!(!dialog.is_dirty());
    dialog.draft["notify"]["bell"] = json!(!config.notify.bell);
    assert!(dialog.is_dirty());
    let DashboardAction::SaveSetup { updated, .. } = dialog.save(&Default::default()) else {
        panic!("expected background save: {:?}", dialog.notice);
    };
    assert!(dialog.saving);
    assert!(!updated.contains("test-secret"));
    let updated: Value = serde_json::from_str(&updated).unwrap();
    assert_eq!(updated["notify"]["bell"], !config.notify.bell);
    assert_eq!(
        updated["profiles"]["deepseek"]["environment"]["DEEPSEEK_API_KEY"],
        json!({"from_secret": "DEEPSEEK_API_KEY"})
    );
}

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

/// Workers read the Jev switch when they start, so a running session keeps
/// classifying until it is resumed or restarted. The page and the save
/// notice say so instead of "Off: nothing is sent". Launch re-verification
/// finding R3-8.
// Hard-won: 8dccf5db: the save notice omitted when running sessions would follow the jev change.
#[test]
fn turning_jev_off_says_running_sessions_follow_after_a_resume_or_restart() {
    let jev = schema::help(&["jev".to_owned()]);
    assert!(!jev.contains("nothing is sent"), "{jev}");
    assert!(jev.contains("resume or restart"), "{jev}");

    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_settings_section("jev", None);
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    dialog.draft["jev"]["enabled"] = json!(false);
    let generation = dialog.generation;
    let saved = saved_config(dialog);
    assert!(!saved.jev.enabled);
    dashboard.setup_saved(generation, Ok(saved));
    let notice = dashboard.notice().unwrap();
    assert!(
        notice.contains("running sessions follow it after their next resume or restart"),
        "{notice}"
    );

    // A save that leaves Jev alone does not mention it.
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    let generation = dialog.generation;
    let saved = saved_config(dialog);
    dashboard.setup_saved(generation, Ok(saved));
    assert!(!dashboard.notice().unwrap().contains("Jev"));
}

#[test]
fn clicking_a_checkbox_setting_label_toggles_once_without_keyboard_selection_toggling() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_settings_section("profiles", None);
    choose(&mut dashboard, "codex-1");
    for expected in [false, true] {
        let lines = drawn(&mut dashboard, 140, 40);
        let (column, row) = point(&lines, "Enabled");
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            dashboard.handle_mouse(MouseEvent {
                kind,
                column,
                row,
                modifiers: KeyModifiers::NONE,
            });
            drawn(&mut dashboard, 140, 40);
        }
        let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
        assert_eq!(dialog.draft["profiles"]["codex-1"]["enabled"], expected);
        assert!(dialog.editor.is_none());
    }
    dashboard.handle_key(key(KeyCode::Down));
    dashboard.handle_key(key(KeyCode::Up));
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    assert_eq!(dialog.draft["profiles"]["codex-1"]["enabled"], true);
}

fn click(dashboard: &mut DashboardState, label: &str) {
    let lines = drawn(dashboard, 140, 40);
    let (column, row) = point(&lines, label);
    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        dashboard.handle_mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        });
        drawn(dashboard, 140, 40);
    }
}

/// A row whose value is picked from a list opens its dropdown on one click,
/// while a row that leads to another page still only selects on one click.
#[test]
fn one_click_opens_a_dropdown_setting_but_only_selects_a_page_row() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    click(&mut dashboard, "Interface");
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    assert!(dialog.path.is_empty(), "one click only selects a page row");

    for (section, task) in [("interface", None), ("subagents", Some("straightforward"))] {
        let mut dashboard = dashboard_with_session(stopped_session());
        dashboard.begin_settings_section(section, None);
        if let Some(task) = task {
            choose(&mut dashboard, task);
        }
        let label = if task.is_some() { "Model" } else { "Theme" };
        click(&mut dashboard, label);
        let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
        let editor = dialog
            .editor
            .as_ref()
            .expect("one click opens the dropdown");
        assert!(editor.combo.is_open(SetupControl::Choices));
    }
}

#[test]
fn signing_in_to_the_selected_profile_is_a_settings_action() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_settings_section("profiles", None);
    let Mode::Setup(dialog) = &mut dashboard.mode else {
        panic!("settings");
    };
    dialog.selected = dialog
        .keys()
        .iter()
        .position(|key| key == "codex-1")
        .expect("the test profile is listed");
    dialog.prepare();
    dialog.form.get_mut().focus(SetupControl::SignIn);
    let generation = dialog.generation;
    let DashboardAction::SetupLogin {
        generation: anchored,
        profile_id,
    } = dashboard.handle_key(key(KeyCode::Enter))
    else {
        panic!("Sign in starts a login for the selected profile");
    };
    assert_eq!(anchored, generation);
    assert_eq!(profile_id, "codex-1");
}

#[test]
fn the_profiles_list_shows_each_profiles_sign_in_status() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.set_profile_login_statuses(
        0,
        std::collections::BTreeMap::from([
            (
                "codex-1".to_owned(),
                mj_core::credentials::LoginStatus::SignedOut,
            ),
            (
                "claude-1".to_owned(),
                mj_core::credentials::LoginStatus::SignedIn,
            ),
        ]),
    );
    dashboard.begin_settings_section("profiles", None);
    let lines = drawn(&mut dashboard, 140, 40);
    let codex = lines
        .iter()
        .find(|line| line.contains("codex-1"))
        .unwrap_or_else(|| panic!("missing codex row: {lines:#?}"));
    assert!(codex.contains("signed out"), "{codex:?}");
    let claude = lines
        .iter()
        .find(|line| line.contains("claude-1"))
        .unwrap_or_else(|| panic!("missing claude row: {lines:#?}"));
    assert!(claude.contains("signed in"), "{claude:?}");
}

#[test]
fn an_older_sign_in_status_read_does_not_replace_a_newer_one() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.set_profile_login_statuses(
        2,
        std::collections::BTreeMap::from([(
            "codex-1".to_owned(),
            mj_core::credentials::LoginStatus::SignedOut,
        )]),
    );
    dashboard.set_profile_login_statuses(
        1,
        std::collections::BTreeMap::from([(
            "codex-1".to_owned(),
            mj_core::credentials::LoginStatus::SignedIn,
        )]),
    );
    assert_eq!(
        dashboard.profile_login_status("codex-1"),
        Some(&mj_core::credentials::LoginStatus::SignedOut)
    );
}

#[test]
fn editing_a_profile_disables_sign_in_until_the_change_is_saved() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_settings_section("profiles", None);
    let Mode::Setup(dialog) = &mut dashboard.mode else {
        panic!("settings");
    };
    dialog.draft["profiles"]["codex-1"]["home"] = json!("/tmp/another-home");
    dialog.selected = dialog
        .keys()
        .iter()
        .position(|key| key == "codex-1")
        .expect("the test profile is listed");
    dialog.prepare();
    let sign_in = dialog
        .page_actions()
        .into_iter()
        .find(|(control, _, _)| *control == SetupControl::SignIn)
        .expect("the profiles page offers sign-in");
    assert!(!sign_in.2, "Sign in must wait for the edit to be saved");
}

fn choose(dashboard: &mut DashboardState, name: &str) -> DashboardAction {
    let Mode::Setup(dialog) = &mut dashboard.mode else {
        panic!("settings");
    };
    dialog.selected = dialog.keys().iter().position(|key| key == name).unwrap();
    dialog.form.get_mut().focus(SetupControl::List);
    dialog.prepare();
    dashboard.handle_key(key(KeyCode::Enter))
}

fn activate(dashboard: &mut DashboardState, control: SetupControl) {
    let Mode::Setup(dialog) = &mut dashboard.mode else {
        panic!("settings");
    };
    dialog.form.get_mut().focus(control);
    dashboard.handle_key(key(KeyCode::Enter));
}

fn choose_light_theme(dashboard: &mut DashboardState) {
    chord(dashboard, crate::CommandId::OpenConfig);
    choose(dashboard, "interface");
    choose(dashboard, "theme");
    dashboard.handle_key(key(KeyCode::Down));
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.handle_key(key(KeyCode::Backspace));
}

fn assert_rendered_theme(dashboard: &mut DashboardState, selected: theme::UiTheme) {
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal
        .draw(|frame| crate::render::render(frame, dashboard))
        .unwrap();
    let colors = theme::palette_for(selected);
    let buffer = terminal.backend().buffer();
    let surface = if dashboard.modal_open() {
        colors.surface_raised
    } else {
        colors.surface
    };
    assert!(
        buffer
            .content
            .iter()
            .any(|cell| { cell.bg == surface && cell.fg == colors.text && cell.symbol() != " " })
    );
    assert!(buffer.content.iter().any(|cell| cell.fg == colors.accent));
}

fn choose_by_keyboard(dashboard: &mut DashboardState, name: &str) -> DashboardAction {
    let (current, target) = {
        let Mode::Setup(dialog) = &dashboard.mode else {
            panic!("settings");
        };
        let target = dialog
            .keys()
            .iter()
            .position(|key| key == name)
            .unwrap_or_else(|| panic!("missing {name:?} in {:?}", dialog.keys()));
        (dialog.selected, target)
    };
    if current > target {
        dashboard.handle_key(key(KeyCode::Home));
    }
    let current = match &dashboard.mode {
        Mode::Setup(dialog) => dialog.selected,
        _ => panic!("settings"),
    };
    for _ in current..target {
        dashboard.handle_key(key(KeyCode::Down));
    }
    dashboard.handle_key(key(KeyCode::Enter))
}

fn setup_at(path: &[&str]) -> DashboardState {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    for name in path {
        let _ = choose_by_keyboard(&mut dashboard, name);
    }
    dashboard
}

fn append_setup_state(
    output: &mut String,
    label: &str,
    dashboard: &mut DashboardState,
    width: u16,
    height: u16,
) -> Vec<String> {
    use std::fmt::Write as _;

    let lines = drawn(dashboard, width, height);
    if !output.is_empty() {
        output.push('\n');
    }
    writeln!(output, "=== {label} ({width}x{height}) ===").expect("write state header");
    output.push_str(&lines.join("\n"));
    output.push('\n');
    lines
}

fn append_setup_body_size(output: &mut String, dashboard: &DashboardState) {
    use std::fmt::Write as _;

    let rect = dashboard
        .frame_surfaces()
        .surface(mj_chat::selection::SurfaceId::ModalBody)
        .expect("rendered Settings modal body")
        .rect;
    writeln!(output, "modal body: {rect:?}").expect("write modal body size");
}

fn rendered_theme_colors(dashboard: &mut DashboardState, selected: theme::UiTheme) -> String {
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).expect("terminal");
    terminal
        .draw(|frame| crate::render::render(frame, dashboard))
        .expect("draw themed Settings");
    let colors = theme::palette_for(selected);
    let surface = if dashboard.modal_open() {
        colors.surface_raised
    } else {
        colors.surface
    };
    let buffer = terminal.backend().buffer();
    let text = buffer
        .content
        .iter()
        .find(|cell| cell.bg == surface && cell.fg == colors.text && cell.symbol() != " ")
        .expect("rendered text uses the selected theme");
    let accent = buffer
        .content
        .iter()
        .find(|cell| cell.fg == colors.accent)
        .expect("rendered accent uses the selected theme");
    format!(
        "rendered colors: text fg={:?} bg={:?}; accent fg={:?}",
        text.fg, text.bg, accent.fg
    )
}

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
    size_journey.handle_key(key(KeyCode::Backspace));
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
    let editor = dialog.editor.as_ref().expect("theme choices");
    let selected = editor
        .combo
        .selection(SetupControl::Choices, editor.selected);
    writeln!(
        output,
        "reopened selected theme: {}",
        editor.choices[selected]
    )
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
            let editor = dialog.editor.as_ref().expect("theme choices");
            (
                editor
                    .combo
                    .selection(SetupControl::Choices, editor.selected),
                editor
                    .choices
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
        setup_dialog_mut(&mut cache.mode)
            .unwrap()
            .editor
            .as_ref()
            .unwrap()
            .input
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
        *dialog.editor.as_mut().unwrap().input =
            mj_chat::text_input::TextInput::from("0".to_owned());
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
    activate(&mut saved_size, SetupControl::Clear);
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
    tabbed.handle_key(key(KeyCode::Tab));
    for expected in [SetupControl::Back, SetupControl::Save, SetupControl::List] {
        append_setup_state(
            &mut output,
            &format!("Build cache Tab focus: {expected:?}"),
            &mut tabbed,
            140,
            30,
        );
        writeln!(
            output,
            "focused control: {:?}",
            setup_dialog_mut(&mut tabbed.mode)
                .unwrap()
                .form
                .borrow()
                .focused()
        )
        .expect("write focus state");
        if expected != SetupControl::List {
            tabbed.handle_key(key(KeyCode::Tab));
        }
    }
    tabbed.handle_key(key(KeyCode::BackTab));
    append_setup_state(
        &mut output,
        "Build cache BackTab returns to Save",
        &mut tabbed,
        140,
        30,
    );
    writeln!(
        output,
        "focused control: {:?}",
        setup_dialog_mut(&mut tabbed.mode)
            .unwrap()
            .form
            .borrow()
            .focused()
    )
    .expect("write focus state");
    let action = tabbed.handle_key(key(KeyCode::Enter));
    writeln!(
        output,
        "Tab-reached Save action: {}",
        matches!(action, DashboardAction::SaveSetup { .. })
    )
    .expect("write Save action");

    let mut arrows = setup_at(&["machines", "local", "build_cache"]);
    arrows.handle_key(key(KeyCode::End));
    for expected in [SetupControl::Back, SetupControl::Save] {
        arrows.handle_key(key(KeyCode::Down));
        append_setup_state(
            &mut output,
            &format!("Build cache Down focus: {expected:?}"),
            &mut arrows,
            140,
            30,
        );
        writeln!(
            output,
            "focused control: {:?}",
            setup_dialog_mut(&mut arrows.mode)
                .unwrap()
                .form
                .borrow()
                .focused()
        )
        .expect("write focus state");
    }
    arrows.handle_key(key(KeyCode::Up));
    let action = arrows.handle_key(key(KeyCode::Enter));
    append_setup_state(
        &mut output,
        "Up and Enter return from Save to the machine page",
        &mut arrows,
        140,
        30,
    );
    writeln!(output, "action after focus return: {action:?}").expect("write focus action");
    writeln!(
        output,
        "returned page path: {:?}",
        setup_dialog_mut(&mut arrows.mode).unwrap().path
    )
    .expect("write returned page");

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
    let Some(DashboardAction::CheckMachineRuntimes { generation, .. }) =
        dashboard.take_machine_runtime_check()
    else {
        panic!("a machine without a runtime is checked");
    };
    dashboard.apply_machine_runtime_check(
        generation,
        Ok(crate::MachineRuntimesChecked {
            probes: vec![crate::MachineRuntimeProbe {
                machine_id: "nas".into(),
                machine: machine("nas.example.test"),
                finding: crate::MachineFinding::Unreachable {
                    detail: "`ssh dev@nas.example.test true` failed: ssh: connect to host nas.example.test port 22: Connection timed out".into(),
                    remediation: "Check that nas.example.test is up and reachable from this machine.".into(),
                },
            }],
            added: std::collections::BTreeMap::new(),
        }),
    );
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
        "memory result opens its nested editor",
        &mut memory,
        140,
        30,
    );
    let landed_page = setup_dialog_mut(&mut memory.mode).unwrap().path.clone();
    let landed_editor = setup_dialog_mut(&mut memory.mode)
        .unwrap()
        .editor
        .as_ref()
        .map(|editor| editor.path.clone());
    writeln!(
        output,
        "landed page and editor: {:?} / {:?}",
        landed_page, landed_editor
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
fn account_path_apply_expands_home_before_config_and_quota_use() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    choose(&mut dashboard, "profiles");
    choose(&mut dashboard, "codex-1");
    choose(&mut dashboard, "home");
    let Mode::Setup(dialog) = &mut dashboard.mode else {
        panic!("settings");
    };
    let editor = dialog.editor.as_mut().unwrap();
    assert!(matches!(editor.input, EditorInput::Path(_)));
    editor.input.clear();
    dashboard.handle_paste("~/.codex4");
    dashboard.handle_key(key(KeyCode::Enter));
    let Mode::Setup(dialog) = &dashboard.mode else {
        panic!("settings");
    };
    assert!(dialog.editor.is_none(), "{:?}", dialog.notice);
    let config: Config = config_from_draft(dialog.draft.clone()).unwrap();
    let expected = mj_core::path_input::expand_local(std::path::Path::new("~/.codex4")).unwrap();
    let profile = &config.profiles["codex-1"];
    assert_eq!(profile.home, expected);
    let mut environment = profile.environment.resolved().clone();
    profile.kind.configure_profile_home_environment(
        &profile.home,
        mj_core::config::HarnessHost::current(),
        &mut environment,
    );
    assert_eq!(environment["CODEX_HOME"], expected.to_string_lossy());
}

#[test]
fn remote_path_apply_preserves_failed_and_newer_drafts() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.config.machines.insert(
        "builder".into(),
        serde_json::from_value(json!({"kind":"ssh","host":"builder"})).unwrap(),
    );
    dashboard.config.targets.insert(
        "remote-path".into(),
        serde_json::from_value(
            json!({"kind":"ssh-bare","host":"builder","permissions":"guardian"}),
        )
        .unwrap(),
    );
    dashboard.begin_setup();
    choose(&mut dashboard, "machines");
    choose(&mut dashboard, "builder");
    choose(&mut dashboard, "workspace_prefix");
    let Mode::Setup(dialog) = &mut dashboard.mode else {
        panic!("settings");
    };
    dialog.editor.as_mut().unwrap().input.set_value("~/work");
    let DashboardAction::ResolveSetupPath {
        generation,
        draft,
        path,
        value,
        ..
    } = dashboard.handle_key(key(KeyCode::Enter))
    else {
        panic!("resolve path");
    };
    dashboard.setup_path_resolved(
        generation,
        &draft,
        &path,
        &value,
        Err("SSH unavailable".into()),
    );
    let Mode::Setup(dialog) = &mut dashboard.mode else {
        panic!("settings");
    };
    assert_eq!(dialog.editor.as_ref().unwrap().input.value(), "~/work");
    assert_eq!(dialog.notice.as_deref(), Some("SSH unavailable"));
    dialog.editor.as_mut().unwrap().input.set_value("~/newer");
    dashboard.setup_path_resolved(generation, &draft, &path, &value, Ok("/remote/work".into()));
    let Mode::Setup(dialog) = &mut dashboard.mode else {
        panic!("settings");
    };
    assert_eq!(dialog.editor.as_ref().unwrap().input.value(), "~/newer");
    dialog.editor.as_mut().unwrap().input.set_value(&value);
    dashboard.setup_path_resolved(generation, &draft, &path, &value, Ok("/remote/work".into()));
    let Mode::Setup(dialog) = &dashboard.mode else {
        panic!("settings");
    };
    assert_eq!(
        dialog.draft["machines"]["builder"]["workspace_prefix"],
        "/remote/work"
    );
}

#[test]
fn choice_popup_escape_preserves_the_draft_and_background_click_is_inert() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    choose(&mut dashboard, "interface");
    choose(&mut dashboard, "theme");
    dashboard.handle_key(key(KeyCode::Down));
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal
        .draw(|frame| crate::render::render(frame, &mut dashboard))
        .unwrap();

    // A click away from the popup must not activate the inert mirror of the
    // page's stacked controls or commit the pending choice.
    let (row, column) = buffer_lines(terminal.backend().buffer())
        .iter()
        .enumerate()
        .find_map(|(row, line)| line.find("Save and Close").map(|column| (row, column)))
        .expect("mirrored Save row");
    let background_button = (column as u16 + 1, row as u16);
    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        dashboard.handle_event_result(Event::Mouse(MouseEvent {
            kind,
            column: background_button.0,
            row: background_button.1,
            modifiers: KeyModifiers::NONE,
        }));
    }
    let Mode::Setup(dialog) = &dashboard.mode else {
        panic!("settings");
    };
    assert!(dialog.editor.is_some());
    assert_eq!(dialog.draft["theme"], "midnight");
    dashboard.handle_key(key(KeyCode::Esc));
    let Mode::Setup(dialog) = &dashboard.mode else {
        panic!("settings");
    };
    assert!(dialog.editor.is_none(), "escape closes the popup");
    assert_eq!(dialog.draft["theme"], "midnight");

    // Reopen it for the pointer-commit part of the behavior.
    choose(&mut dashboard, "theme");
    terminal
        .draw(|frame| crate::render::render(frame, &mut dashboard))
        .unwrap();

    // Find one popup content cell from the rendered form and click it.
    let (row, column) = buffer_lines(terminal.backend().buffer())
        .iter()
        .enumerate()
        .find_map(|(row, line)| line.find("Light").map(|column| (row, column)))
        .expect("Light popup row");
    let point = (column as u16, row as u16);
    assert!(
        setup_dialog_mut(&mut dashboard.mode)
            .is_some_and(|dialog| dialog.form.borrow().contains(point.0, point.1))
    );
    dashboard.handle_event_result(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: point.0,
        row: point.1,
        modifiers: KeyModifiers::NONE,
    }));
    dashboard.handle_event_result(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Up(MouseButton::Left),
        column: point.0,
        row: point.1,
        modifiers: KeyModifiers::NONE,
    }));
    let Mode::Setup(dialog) = &dashboard.mode else {
        panic!("settings");
    };
    assert!(dialog.editor.is_none(), "click commits the popup choice");
    assert_eq!(dialog.draft["theme"], "light");
}

#[test]
fn cancelling_or_failing_to_save_a_theme_keeps_the_active_colors() {
    let mut dashboard = dashboard_with_session(stopped_session());
    let original = dashboard.config.clone();
    choose_light_theme(&mut dashboard);
    dashboard.handle_key(key(KeyCode::Esc));
    assert!(dashboard.modal_open());
    dashboard.handle_key(key(KeyCode::Right));
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::None
    );
    assert!(!dashboard.modal_open());
    assert_eq!(dashboard.config, original);
    assert_rendered_theme(&mut dashboard, theme::UiTheme::Midnight);

    choose_light_theme(&mut dashboard);
    let action = dashboard.handle_key(crossterm::event::KeyEvent::new(
        KeyCode::Char('s'),
        KeyModifiers::CONTROL,
    ));
    let DashboardAction::SaveSetup { generation, .. } = action else {
        panic!("{action:?}");
    };
    dashboard.setup_saved(generation, Err("disk full".into()));
    assert_eq!(dashboard.config, original);
    assert_rendered_theme(&mut dashboard, theme::UiTheme::Midnight);
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    assert_eq!(dialog.draft["theme"], "light");
    assert!(dialog.notice.as_ref().unwrap().contains("disk full"));
}

#[test]
fn disabling_a_profile_clears_references_and_reports_the_cleanup() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.config.review.trigger = mj_core::config::ReviewTrigger::Turn;
    dashboard.config.review.profile = Some("codex-1".into());
    dashboard.config.review.model = Some("review-model".into());
    dashboard.config.review.effort = Some("high".into());
    dashboard.begin_setup();
    choose(&mut dashboard, "profiles");
    choose(&mut dashboard, "codex-1");

    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal
        .draw(|frame| crate::render::render(frame, &mut dashboard))
        .unwrap();
    let enabled = buffer_lines(terminal.backend().buffer()).join("\n");
    assert!(enabled.contains('☒'), "{enabled}");

    choose(&mut dashboard, "enabled");
    let Mode::Setup(dialog) = &dashboard.mode else {
        panic!("settings");
    };
    assert_eq!(dialog.draft["profiles"]["codex-1"]["enabled"], false);
    assert!(dialog.draft["review"]["profile"].is_null());
    assert_eq!(dialog.draft["review"]["trigger"], "off");
    assert_eq!(dialog.draft["review"]["model"], "review-model");
    assert_eq!(dialog.draft["review"]["effort"], "high");
    let notice = dialog.notice.as_deref().unwrap();
    assert!(notice.contains("Code Review"), "{notice}");

    terminal
        .draw(|frame| crate::render::render(frame, &mut dashboard))
        .unwrap();
    let disabled = buffer_lines(terminal.backend().buffer()).join("\n");
    assert!(disabled.contains('☐'), "{disabled}");
}

#[test]
fn detection_adds_conflicting_installations_to_the_draft_without_losing_settings() {
    let mut dashboard = dashboard_with_session(stopped_session());
    let original = dashboard.config.clone();
    dashboard.begin_setup();
    let generation = setup_dialog_mut(&mut dashboard.mode).unwrap().generation;
    let mut discovered = original.clone();
    discovered.profiles.get_mut("codex-1").unwrap().home = "/profiles/new-codex".into();
    discovered
        .targets
        .insert("podman".into(), mj_core::config::TargetTemplate::LocalBare);
    discovered.bundles.get_mut("hel").unwrap().repositories[0].github =
        Some("owner/new-repository".into());
    for _ in 0..2 {
        for scope in [DetectScope::Profiles, DetectScope::Runtimes] {
            dashboard.setup_discovered(generation, Ok(detection(scope, discovered.clone())));
        }
        let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
        let draft: Config = config_from_draft(dialog.draft.clone()).unwrap();
        assert_eq!(draft.profiles["codex-1"], original.profiles["codex-1"]);
        assert_eq!(
            draft.profiles["codex-1-2"].home,
            std::path::PathBuf::from("/profiles/new-codex")
        );
        assert_eq!(draft.targets["podman"], original.targets["podman"]);
        assert!(matches!(
            draft.targets["podman-2"],
            mj_core::config::TargetTemplate::LocalBare
        ));
        // Neither detection touches projects, so the draft keeps only the
        // bundles the user already had.
        assert_eq!(draft.bundles, original.bundles);
        assert_eq!(draft.profiles.len(), original.profiles.len() + 1);
        assert_eq!(draft.targets.len(), original.targets.len() + 1);
    }
    assert_eq!(dashboard.config, original, "discovery only edits the draft");
}

/// On a machine with no coding agent, Detect profiles said "No agent
/// installation was found that this draft does not already have." "Draft" is
/// an internal word, and the sentence gave no next step (launch finding
/// R13-4).
// Hard-won: 60750d28: empty profile discovery did not explain that the machine had no agent.
#[test]
fn detecting_profiles_on_a_machine_without_an_agent_says_so_plainly() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    let generation = setup_dialog_mut(&mut dashboard.mode).unwrap().generation;

    dashboard.setup_discovered(
        generation,
        Ok(detection(DetectScope::Profiles, Config::default())),
    );

    let notice = setup_dialog_mut(&mut dashboard.mode)
        .unwrap()
        .notice
        .clone()
        .expect("detection reports what it did");
    assert_eq!(
        notice,
        "No coding agent installation was found on this machine. Add a profile, choose Sign in, \
         or install Codex, Claude Code, Kimi Code, Grok Build, Muse Code, OpenCode, MiMo Code, or \
         DSH, sign in to it once, and choose Detect profiles again."
    );
}

/// An agent that is installed but already has a profile is not "not found".
// Hard-won: 60750d28: discovery gave no distinct outcome when every agent profile already existed.
#[test]
fn detecting_profiles_that_all_exist_says_each_agent_already_has_one() {
    let mut dashboard = dashboard_with_session(stopped_session());
    let original = dashboard.config.clone();
    dashboard.begin_setup();
    let generation = setup_dialog_mut(&mut dashboard.mode).unwrap().generation;

    dashboard.setup_discovered(generation, Ok(detection(DetectScope::Profiles, original)));

    let notice = setup_dialog_mut(&mut dashboard.mode)
        .unwrap()
        .notice
        .clone()
        .expect("detection reports what it did");
    assert_eq!(
        notice,
        "Every coding agent found on this machine already has a profile."
    );
}

fn detection(scope: DetectScope, config: Config) -> crate::setup::SetupDetection {
    crate::setup::SetupDetection {
        scope,
        config,
        rejected_runtimes: Vec::new(),
        machines: Vec::new(),
    }
}

#[test]
fn results_from_a_closed_setup_do_not_change_the_new_draft() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    let old = setup_dialog_mut(&mut dashboard.mode).unwrap().generation;
    dashboard.cancel_modal();
    dashboard.begin_setup();
    let new = setup_dialog_mut(&mut dashboard.mode).unwrap().generation;
    let original = setup_dialog_mut(&mut dashboard.mode).unwrap().draft.clone();
    let mut detected = dashboard.config.clone();
    detected.targets.insert(
        "stale-discovery".into(),
        mj_core::config::TargetTemplate::LocalBare,
    );
    dashboard.setup_discovered(old, Ok(detection(DetectScope::Runtimes, detected)));
    dashboard.setup_saved(old, Ok(dashboard.config.clone()));
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    assert_eq!(dialog.generation, new);
    assert_eq!(dialog.draft, original);
}

/// Launch campaign finding A-3: once `ascii` was chosen, the popup offered no
/// way back to "Follows the terminal", and saving that choice must remove the
/// key rather than write a third value.
// Hard-won: 688cfd2a: the ASCII choice could not be cleared back to terminal-following behavior.
#[test]
fn symbols_can_return_to_the_unset_state_and_saving_removes_the_key() {
    let mut dashboard = dashboard_with_session(stopped_session());
    let mut config = dashboard.config.clone();
    config.advanced.symbols = Some(mj_core::config::SymbolSet::Ascii);
    dashboard.set_config(config);
    dashboard.begin_setup();
    choose(&mut dashboard, "advanced");
    choose(&mut dashboard, "symbols");
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal
        .draw(|frame| crate::render::render(frame, &mut dashboard))
        .unwrap();
    let popup = buffer_lines(terminal.backend().buffer()).join("\n");
    assert!(popup.contains("Follows the terminal"), "{popup}");
    assert!(
        popup.contains("unicode") && popup.contains("ascii"),
        "{popup}"
    );

    dashboard.handle_key(key(KeyCode::Up));
    dashboard.handle_key(key(KeyCode::Up));
    dashboard.handle_key(key(KeyCode::Enter));
    assert_eq!(
        setup_dialog_mut(&mut dashboard.mode).unwrap().draft["advanced"]["symbols"],
        Value::Null
    );
    let action = dashboard.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
    let DashboardAction::SaveSetup { updated, .. } = action else {
        panic!("expected Settings save, got {action:?}")
    };
    let saved: Config = serde_json::from_str(&updated).unwrap();
    assert_eq!(saved.advanced.symbols, None);
    assert!(!updated.contains("symbols"), "{updated}");
}

#[test]
fn setup_adds_a_remote_runtime_and_reports_invalid_fields_without_losing_the_draft() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    choose(&mut dashboard, "machines");
    dashboard.handle_key(key(KeyCode::Char('a')));
    dashboard.handle_paste("builder");
    dashboard.handle_key(key(KeyCode::Enter));
    let action = dashboard.handle_key(crossterm::event::KeyEvent::new(
        KeyCode::Char('s'),
        KeyModifiers::CONTROL,
    ));
    let DashboardAction::SaveSetup {
        generation,
        updated,
        ..
    } = action
    else {
        panic!("expected background validation, got {action:?}");
    };
    let invalid: Config = serde_json::from_str(&updated).unwrap();
    dashboard.setup_saved(generation, Err(invalid.validate().unwrap_err().to_string()));
    let Mode::Setup(dialog) = &dashboard.mode else {
        panic!("settings");
    };
    assert!(dialog.notice.as_ref().unwrap().contains("SSH host"));
    choose(&mut dashboard, "host");
    dashboard.handle_paste("builder.example.test");
    dashboard.handle_key(key(KeyCode::Enter));
    activate(&mut dashboard, SetupControl::Back);
    activate(&mut dashboard, SetupControl::Back);

    // A runtime on the new machine: Docker, chosen from the runtime kinds,
    // and the machine, chosen from the machines the draft now has.
    choose(&mut dashboard, "targets");
    dashboard.handle_key(key(KeyCode::Char('a')));
    dashboard.handle_paste("builder-docker");
    dashboard.handle_key(key(KeyCode::Enter));
    choose(&mut dashboard, "kind");
    select_choice(&mut dashboard, "docker");
    choose(&mut dashboard, "machine");
    select_choice(&mut dashboard, "builder");

    let action = dashboard.handle_key(crossterm::event::KeyEvent::new(
        KeyCode::Char('s'),
        KeyModifiers::CONTROL,
    ));
    let DashboardAction::SaveSetup { updated, .. } = action else {
        panic!("{action:?}");
    };
    let saved: Config = serde_json::from_str(&updated).unwrap();
    assert!(
        matches!(&saved.targets["builder-docker"], mj_core::config::TargetTemplate::SshDocker { ssh, .. } if ssh.host == "builder.example.test")
    );
    let generation = setup_dialog_mut(&mut dashboard.mode).unwrap().generation;
    dashboard.setup_saved(generation, Err("disk full".into()));
    let Mode::Setup(dialog) = &dashboard.mode else {
        panic!("settings");
    };
    assert!(!dialog.saving);
    assert_eq!(
        dialog.draft["machines"]["builder"]["host"],
        "builder.example.test"
    );
    assert!(dialog.notice.as_ref().unwrap().contains("disk full"));
}

/// Open the selected field's choice list and commit `wanted`.
fn select_choice(dashboard: &mut DashboardState, wanted: &str) {
    let Mode::Setup(dialog) = &mut dashboard.mode else {
        panic!("settings");
    };
    let editor = dialog.editor.as_mut().expect("a choice editor is open");
    let selected = editor
        .choices
        .iter()
        .position(|value| value == wanted)
        .unwrap_or_else(|| panic!("{wanted:?} is not offered: {:?}", editor.choices));
    assert!(editor.combo.preview(SetupControl::Choices, selected));
    dialog.prepare();
    dashboard.handle_key(key(KeyCode::Enter));
}

#[test]
fn this_machine_is_always_listed_and_cannot_be_removed() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    choose(&mut dashboard, "machines");
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    assert_eq!(dialog.keys(), ["local"]);
    dialog.selected = 0;
    activate(&mut dashboard, SetupControl::Remove);
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    assert_eq!(dialog.keys(), ["local"]);
    assert_eq!(
        dialog.notice.as_deref(),
        Some("This machine is always available.")
    );
    // Its type is not a choice, and it carries the shared build cache.
    choose(&mut dashboard, "local");
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    assert_eq!(dialog.keys(), ["build_cache"]);
}

#[test]
fn backspace_at_the_root_with_a_dirty_draft_asks_before_discarding() {
    let mut dashboard = dashboard_with_session(stopped_session());
    let original = dashboard.config.clone();
    dashboard.begin_setup();
    choose(&mut dashboard, "phone");
    choose(&mut dashboard, "enabled");
    dashboard.handle_key(key(KeyCode::Backspace));
    dashboard.handle_key(key(KeyCode::Backspace));
    let Mode::Confirm(_) = &dashboard.mode else {
        panic!(
            "dirty settings must confirm before closing: {:?}",
            dashboard.modal_open()
        );
    };
    // Esc keeps editing with the draft intact.
    dashboard.handle_key(key(KeyCode::Esc));
    let Mode::Setup(dialog) = &dashboard.mode else {
        panic!("settings restored");
    };
    assert!(dialog.is_dirty());
    assert_eq!(dashboard.config, original);
}

#[test]
fn cancelling_setup_preserves_configuration_and_render_keeps_controls_visible() {
    let mut dashboard = dashboard_with_session(stopped_session());
    let original = dashboard.config.clone();
    for (width, height) in [(80, 18), (100, 30), (140, 42)] {
        dashboard.begin_setup();
        choose(&mut dashboard, "phone");
        choose(&mut dashboard, "enabled");
        dashboard.handle_key(key(KeyCode::Backspace));
        choose(&mut dashboard, "targets");
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| crate::render::render(frame, &mut dashboard))
            .unwrap();
        let lines = buffer_lines(terminal.backend().buffer());
        let text = lines.join("\n");
        assert!(text.contains("Settings › Runtimes"), "{text}");
        // Every page action keeps its own row in one column at the dialog's
        // right edge, on top of each other rather than spread along a row.
        let labels = ["Add", "Remove", "Detect runtimes"];
        let width = labels
            .iter()
            .map(|label| label.len())
            .max()
            .expect("labels");
        let mut rows = Vec::new();
        for label in labels {
            // The button's padding separates it from prose that happens to
            // use the same word, such as the page's help line.
            let padded = format!("  {label}  ");
            let (row, line) = lines
                .iter()
                .enumerate()
                .find(|(_, line)| line.contains(&padded))
                .unwrap_or_else(|| panic!("missing {label:?} in\n{text}"));
            // Every button is as wide as the longest of them, so a shorter
            // label is followed by its share of that width, the button's
            // padding, the inner margin, and then the modal border.
            let after = &line[line.find(&padded).unwrap() + 2 + label.len()..];
            let gap = format!("{}│", " ".repeat(3 + width - label.len()));
            assert!(
                after.starts_with(&gap),
                "{label} is not packed against the dialog's right edge: {line}"
            );
            rows.push((row, cell_column(line, &padded) + 2, label));
        }
        assert!(
            rows.windows(2).all(|pair| pair[0].0 < pair[1].0),
            "the buttons are not stacked in order: {rows:?}\n{text}"
        );
        let (_, first_column, _) = rows[0];
        assert!(
            rows.iter().all(|(_, column, _)| *column == first_column),
            "the stacked buttons do not share a column: {rows:?}\n{text}"
        );
        assert!(
            rows.windows(2).all(|pair| pair[0].0 + 1 == pair[1].0),
            "the stacked buttons leave gaps between them: {rows:?}\n{text}"
        );
        // Back and the commit share the footer row below the column.
        let (back_column, back_row) = point(&lines, "  Back  ");
        let (save_column, save_row) = point(&lines, "  Save and Close  ");
        assert_eq!(back_row, save_row, "{text}");
        assert!(back_column < save_column, "{text}");
        assert!(
            rows.iter().all(|(row, _, _)| (*row as u16) < back_row),
            "the column overlaps the footer row: {rows:?}\n{text}"
        );
        assert!(!text.contains("Cancel"), "{text}");
        // Backspace from the root dismisses through the dirty guard.
        dashboard.handle_key(key(KeyCode::Backspace));
        dashboard.handle_key(key(KeyCode::Backspace));
        assert!(matches!(dashboard.mode, Mode::Confirm(_)), "{text}");
        dashboard.handle_key(key(KeyCode::Right));
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::None
        );
        assert!(!dashboard.modal_open());
        assert_eq!(dashboard.config, original);
    }
}

/// Campaign finding W-1: the second Esc out of a dirty Settings draft opens
/// the "Discard Settings changes?" prompt, which looks like the dashboard
/// behind it. Esc on that prompt means "Keep editing", so a third Esc returns
/// to Settings. Once the draft is discarded, Esc on the dashboard is inert.
#[test]
fn escape_on_the_discard_prompt_keeps_editing_and_dashboard_escape_stays_inert() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    choose(&mut dashboard, "review");
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.handle_key(key(KeyCode::Down));
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.handle_key(key(KeyCode::Esc));
    dashboard.handle_key(key(KeyCode::Esc));
    assert!(matches!(dashboard.mode, Mode::Confirm(_)));
    dashboard.handle_key(key(KeyCode::Esc));
    let Mode::Setup(dialog) = &dashboard.mode else {
        panic!("Esc on the discard prompt keeps editing")
    };
    assert_eq!(dialog.draft["review"]["trigger"], "turn");
    // Discard, then Esc on the plain dashboard does nothing.
    dashboard.handle_key(key(KeyCode::Esc));
    assert!(matches!(dashboard.mode, Mode::Confirm(_)));
    dashboard.handle_key(key(KeyCode::Right));
    dashboard.handle_key(key(KeyCode::Enter));
    assert!(!dashboard.modal_open());
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Esc)),
        DashboardAction::None
    );
    assert!(!dashboard.modal_open());
}

#[test]
fn review_changes_stay_in_setup_draft_until_save_and_cancel_discards_them() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    choose(&mut dashboard, "review");
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.handle_key(key(KeyCode::Down));
    dashboard.handle_key(key(KeyCode::Enter));
    // Esc goes back to the first page and keeps the change in the draft, as
    // the page says it will. Launch campaign finding C-19.
    dashboard.handle_key(key(KeyCode::Esc));
    assert!(!dashboard.dialog_confirmation_open());
    assert!(!dashboard.config.review.arms_turn_review());
    let Mode::Setup(dialog) = &dashboard.mode else {
        panic!("settings remains open after leaving review")
    };
    assert!(dialog.review_editor.is_none(), "Esc left Code Review");
    assert_eq!(dialog.draft["review"]["trigger"], "turn");
    // The Back button does the same.
    choose(&mut dashboard, "review");
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.handle_key(key(KeyCode::Up));
    dashboard.handle_key(key(KeyCode::Enter));
    let Mode::Setup(dialog) = &mut dashboard.mode else {
        panic!("settings")
    };
    dialog
        .review_editor
        .as_mut()
        .unwrap()
        .form
        .get_mut()
        .focus(crate::review_settings::ReviewSettingsFocus::Back);
    dashboard.handle_key(key(KeyCode::Enter));
    assert!(!dashboard.dialog_confirmation_open());
    let Mode::Setup(dialog) = &dashboard.mode else {
        panic!("settings remains open after leaving review")
    };
    assert!(dialog.review_editor.is_none(), "Back left Code Review");
    assert_eq!(dialog.draft["review"]["trigger"], "off");
    choose(&mut dashboard, "review");
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.handle_key(key(KeyCode::Down));
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.handle_key(key(KeyCode::Esc));
    // Leaving Settings with the review change still drafted asks once.
    dashboard.handle_key(key(KeyCode::Esc));
    assert!(matches!(dashboard.mode, Mode::Confirm(_)));
    dashboard.handle_key(key(KeyCode::Right));
    dashboard.handle_key(key(KeyCode::Enter));
    assert!(!dashboard.modal_open());
    assert!(!dashboard.config.review.arms_turn_review());

    dashboard.begin_setup();
    choose(&mut dashboard, "review");
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.handle_key(key(KeyCode::Down));
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.handle_key(key(KeyCode::Tab));
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.handle_key(key(KeyCode::Down));
    dashboard.handle_key(key(KeyCode::Enter));
    let action = dashboard.handle_key(crossterm::event::KeyEvent::new(
        KeyCode::Char('s'),
        KeyModifiers::CONTROL,
    ));
    assert!(matches!(action, DashboardAction::SaveSetup { .. }));
    let Mode::Setup(dialog) = &dashboard.mode else {
        panic!("settings save remains pending")
    };
    let generation = dialog.generation;
    let updated: Config = serde_json::from_str(match &action {
        DashboardAction::SaveSetup { updated, .. } => updated,
        _ => unreachable!(),
    })
    .unwrap();
    assert!(updated.review.arms_turn_review());
    dashboard.setup_saved(generation, Ok(updated));
    assert!(!dashboard.modal_open());
    assert!(dashboard.config.review.arms_turn_review());
}

#[test]
fn unsaved_account_edits_block_review_cache_and_refresh_until_setup_is_saved() {
    use crate::review_settings::{ReviewSettingsChoices, ReviewSettingsFocus};

    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.config.review.profile = Some("codex-1".into());
    dashboard.config.review.trigger = mj_core::config::ReviewTrigger::Turn;
    dashboard
        .review_settings_choices
        .insert(("codex-1".into(), None), ReviewSettingsChoices::default());
    dashboard.begin_setup();
    choose(&mut dashboard, "profiles");
    choose(&mut dashboard, "codex-1");
    choose(&mut dashboard, "home");
    dashboard.handle_paste("-changed");
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.handle_key(key(KeyCode::Backspace));
    dashboard.handle_key(key(KeyCode::Backspace));
    choose(&mut dashboard, "review");
    let setup = setup_dialog_mut(&mut dashboard.mode).unwrap();
    let review = setup.review_editor.as_mut().unwrap();
    assert!(!review.probing);
    assert!(
        !review.model_choices_discovered,
        "old account cache must not apply"
    );
    assert!(
        review
            .discovery_error
            .as_deref()
            .unwrap()
            .contains("Save account changes")
    );
    review.form.get_mut().focus(ReviewSettingsFocus::Refresh);
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::CancelReviewSettingsDiscovery
    );
    let setup = setup_dialog_mut(&mut dashboard.mode).unwrap();
    let review = setup.review_editor.as_mut().unwrap();
    review.form.get_mut().focus(ReviewSettingsFocus::Profile);
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::None
    );
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Down)),
        DashboardAction::None
    );
    assert!(matches!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::DiscoverReviewSettings { profile_id, .. } if profile_id == "codex-2"
    ));
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::None
    );
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Up)),
        DashboardAction::None
    );
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::CancelReviewSettingsDiscovery
    );
    let DashboardAction::SaveSetup { updated, .. } =
        dashboard.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
    else {
        panic!("unverified capabilities must not prevent saving account changes")
    };
    let saved: Config = serde_json::from_str(&updated).unwrap();
    assert_eq!(saved.review.profile.as_deref(), Some("codex-1"));
    assert_ne!(
        saved.profiles["codex-1"].home,
        dashboard.config.profiles["codex-1"].home
    );
}

#[test]
fn unrelated_account_edits_do_not_allow_saving_a_known_unavailable_review_model() {
    use crate::review_settings::{ReviewSettingsChoices, ReviewSettingsDiscoveryResult};

    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.config.review.profile = Some("codex-1".into());
    dashboard.config.review.trigger = mj_core::config::ReviewTrigger::Turn;
    dashboard.config.review.model = Some("unavailable-model".into());
    let DashboardAction::DiscoverReviewSettings {
        generation,
        profile_id,
        model,
    } = dashboard.begin_review_settings()
    else {
        panic!("expected capability discovery")
    };
    dashboard.apply_review_settings_discovery(
        generation,
        &profile_id,
        model.as_deref(),
        Ok(ReviewSettingsDiscoveryResult::Available {
            choices: ReviewSettingsChoices::default(),
            cleanup_warning: None,
        }),
    );
    if let Mode::Setup(setup) = &mut dashboard.mode {
        setup
            .review_editor
            .as_mut()
            .expect("review editor")
            .form
            .get_mut()
            .focus(crate::review_settings::ReviewSettingsFocus::Back);
    } else {
        panic!("settings remains open after discovery");
    }
    dashboard.handle_key(key(KeyCode::Enter));
    choose(&mut dashboard, "profiles");
    choose(&mut dashboard, "codex-2");
    choose(&mut dashboard, "home");
    dashboard.handle_paste("-changed");
    dashboard.handle_key(key(KeyCode::Enter));
    let action = dashboard.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
    assert_eq!(action, DashboardAction::None);
    let setup = setup_dialog_mut(&mut dashboard.mode).unwrap();
    assert!(setup.notice.as_deref().unwrap().contains("unavailable"));
    dashboard.handle_key(key(KeyCode::Backspace));
    choose(&mut dashboard, "codex-1");
    choose(&mut dashboard, "home");
    dashboard.handle_key(key(KeyCode::Enter));
    assert_eq!(
        dashboard.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
        DashboardAction::None,
        "applying an unchanged account field must retain known validation"
    );
}

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
    dialog.selected = dialog
        .keys()
        .iter()
        .position(|key| key == "build_cache")
        .unwrap();
    dialog.form.get_mut().focus(SetupControl::List);
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
        "☒",
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
    dialog.form.get_mut().focus(SetupControl::List);
    dialog.selected = dialog
        .keys()
        .iter()
        .position(|key| key == "build_cache")
        .unwrap();
    dialog.prepare();
    dashboard.handle_key(key(KeyCode::Enter));

    // Native tool caches remain selectable without mbx.
    let dialog = setup_dialog_mut(&mut dashboard.mode).expect("settings");
    dialog.selected = dialog
        .keys()
        .iter()
        .position(|key| key == "enabled")
        .unwrap();
    dialog.form.get_mut().focus(SetupControl::List);
    dialog.prepare();
    dashboard.handle_key(key(KeyCode::Enter));
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
fn the_build_cache_switch_is_a_checkbox_on_a_host_that_supports_it() {
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
    dialog.selected = dialog
        .keys()
        .iter()
        .position(|key| key == "build_cache")
        .unwrap();
    dialog.form.get_mut().focus(SetupControl::List);
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
    assert!(checked.contains("☒"), "an unset switch is on:\n{checked}");
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

    let dialog = setup_dialog_mut(&mut dashboard.mode).expect("settings");
    dialog.selected = dialog
        .keys()
        .iter()
        .position(|key| key == "enabled")
        .unwrap();
    dialog.form.get_mut().focus(SetupControl::List);
    dialog.prepare();
    dashboard.handle_key(key(KeyCode::Enter));
    let dialog = setup_dialog_mut(&mut dashboard.mode).expect("settings");
    assert_eq!(
        dialog.draft["machines"]["local"]["build_cache"]["enabled"],
        Value::Bool(false)
    );
    assert!(dialog.editor.is_none(), "the switch opens no text editor");
    let unchecked = drawn(&mut dashboard, 140, 30).join("\n");
    assert!(
        unchecked.contains("☐"),
        "turning it off unchecks it:\n{unchecked}"
    );

    dashboard.handle_key(key(KeyCode::Enter));
    let dialog = setup_dialog_mut(&mut dashboard.mode).expect("settings");
    assert_eq!(
        dialog.draft["machines"]["local"]["build_cache"]["enabled"],
        Value::Null,
        "turning it back on writes no override"
    );
}

/// The cache size is typed, stored and shown as a whole number of GB.
/// The saved file keeps only the build cache fields that are set, so the page
/// has to fill the rest back in: all three stay listed and editable after a
/// save, and each one can still be handed back to the host.
/// The border promises `Esc back`, and below the first page that is what it
/// must do: return to the parent with the draft intact.
// Hard-won: 0cc7c1e6: Escape on a subpage discarded the entire settings draft.
#[test]
fn escape_returns_from_a_settings_subpage_and_closes_only_from_the_first_page() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    choose(&mut dashboard, "machines");
    choose(&mut dashboard, "local");
    dashboard.handle_key(key(KeyCode::Esc));
    let Mode::Setup(dialog) = &dashboard.mode else {
        panic!("Esc on a sub-page must keep Settings open");
    };
    assert_eq!(dialog.path, ["machines"]);
    dashboard.handle_key(key(KeyCode::Esc));
    let Mode::Setup(dialog) = &dashboard.mode else {
        panic!("Esc on the Machines page must return to the first page");
    };
    assert!(dialog.path.is_empty());

    // A sub-page edit survives the way back out.
    choose(&mut dashboard, "phone");
    choose(&mut dashboard, "enabled");
    dashboard.handle_key(key(KeyCode::Esc));
    let Mode::Setup(dialog) = &dashboard.mode else {
        panic!("Esc must not discard the draft from a sub-page");
    };
    assert!(dialog.path.is_empty());
    assert_eq!(dialog.draft["phone"]["enabled"], Value::Bool(false));
    assert!(dialog.is_dirty());

    // From the first page Esc closes the dialog, through the discard guard.
    dashboard.handle_key(key(KeyCode::Esc));
    assert!(
        matches!(dashboard.mode, Mode::Confirm(_)),
        "a dirty draft asks before closing"
    );
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

/// A refused value is reported with the field it was typed into, not on the
/// dialog's bottom rows a page below it.
// Hard-won: 9e96756a: field validation errors appeared far below the edited field.
#[test]
fn a_rejected_field_value_is_reported_under_the_field() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    choose(&mut dashboard, "machines");
    choose(&mut dashboard, "local");
    choose(&mut dashboard, "build_cache");
    choose(&mut dashboard, "max_total_size");
    for typed in ['1', '.', '5'] {
        dashboard.handle_key(key(KeyCode::Char(typed)));
    }
    dashboard.handle_key(key(KeyCode::Enter));
    let dialog = setup_dialog_mut(&mut dashboard.mode).expect("settings");
    assert_eq!(
        dialog.notice.as_deref(),
        Some("Enter a whole number of gigabytes.")
    );
    let lines = drawn(&mut dashboard, 140, 30);
    let (_, field) = point(&lines, "1.5");
    let (_, message) = point(&lines, "Enter a whole number of gigabytes.");
    assert_eq!(
        message,
        field + 1,
        "the message is {} rows from the field:\n{}",
        i32::from(message) - i32::from(field),
        lines.join("\n")
    );
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
    dialog.selected = dialog
        .keys()
        .iter()
        .position(|key| key == "sessionwiki")
        .unwrap();
    dialog.form.get_mut().focus(SetupControl::List);
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
    dialog.selected = dialog
        .keys()
        .iter()
        .position(|key| key == "archive_after_days")
        .unwrap();
    dialog.form.get_mut().focus(SetupControl::List);
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
    // The open editor covers the page, so while typing the estimate sits
    // under the input, without repeating the number being typed; closing the
    // editor puts the estimate, with the value, back in the row.
    let editing = rendered(&mut dashboard);
    assert!(
        editing.contains("would reclaim 1.2G of 4.8G (12 of 40 sessions)")
            && !editing.contains("30 · would reclaim"),
        "the editor must show what the typed value would reclaim:\n{editing}"
    );
    dashboard.handle_key(key(KeyCode::Enter));
    let reclaim = rendered(&mut dashboard);
    assert!(
        reclaim.contains("30 · would reclaim 1.2G of 4.8G (12 of 40 sessions)"),
        "the row must report what the saved value would reclaim:\n{reclaim}"
    );
}

/// Opens the settings search and types `query` into it.
fn search(dashboard: &mut DashboardState, query: &str) {
    dashboard.handle_key(key(KeyCode::Char('/')));
    if !query.is_empty() {
        dashboard.handle_paste(query);
    }
}

fn search_state(dashboard: &mut DashboardState) -> &SearchState {
    setup_dialog_mut(&mut dashboard.mode)
        .expect("settings")
        .search
        .as_ref()
        .expect("an open search")
}

fn result_paths(dashboard: &mut DashboardState) -> Vec<Vec<String>> {
    let search = search_state(dashboard);
    search
        .matches
        .iter()
        .filter_map(|index| search.entries.get(*index))
        .map(|entry| entry.path.clone())
        .collect()
}

#[test]
fn search_selects_a_switch_without_flipping_it() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    search(&mut dashboard, "detailed activity");
    dashboard.handle_key(key(KeyCode::Enter));
    let dialog = setup_dialog_mut(&mut dashboard.mode).expect("settings");
    assert_eq!(dialog.path, vec!["advanced".to_owned()]);
    assert_eq!(
        dialog.keys().get(dialog.selected).map(String::as_str),
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
fn moving_the_selection_does_not_change_the_text_a_page_draws() {
    // A list identifies its contents by the text it draws, so a row that drew
    // itself differently while selected would read as a new list on every
    // arrow key and cancel the gesture a double-click is halfway through.
    // Selection is the highlight's job, never the row's.
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    let first = drawn(&mut dashboard, 140, 40);
    dashboard.handle_key(key(KeyCode::Down));
    dashboard.handle_key(key(KeyCode::Down));
    let moved = drawn(&mut dashboard, 140, 40);
    assert_eq!(
        first,
        moved,
        "the selection changed the drawn text:\n{}",
        moved.join("\n")
    );
    let dialog = setup_dialog_mut(&mut dashboard.mode).expect("settings");
    assert_ne!(dialog.selected, 0, "the arrows must have moved the row");
}

fn ctrl_space() -> KeyEvent {
    KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL)
}

/// An identity file is a file, not a directory, and it lives on the machine
/// running Mjolnir rather than on the machine it configures.
#[test]
fn setup_identity_file_completes_files_locally() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.config.machines.insert(
        "builder".into(),
        serde_json::from_value(json!({"kind":"ssh","host":"builder"})).unwrap(),
    );
    dashboard.begin_setup();
    choose(&mut dashboard, "machines");
    choose(&mut dashboard, "builder");
    choose(&mut dashboard, "identity_file");
    let Mode::Setup(dialog) = &mut dashboard.mode else {
        panic!("settings");
    };
    dialog.editor.as_mut().unwrap().input.set_value("/keys/id");
    dialog.prepare();

    assert_eq!(
        dashboard.handle_key(ctrl_space()),
        DashboardAction::CompletePath {
            host: mj_core::path_completion::CompletionHost::Local,
            kind: mj_core::path_completion::CompletionKind::Any,
            prefix: "/keys/id".into(),
        }
    );
    let context = dashboard.path_input_context();
    dashboard.apply_path_completions(
        &context,
        "/keys/id",
        mj_core::path_completion::PathCompletion {
            candidates: vec!["/keys/id_ed25519".into(), "/keys/id_rsa".into()],
            insert: None,
            truncated: false,
        },
    );
    let Mode::Setup(dialog) = &dashboard.mode else {
        panic!("settings");
    };
    let EditorInput::Path(input) = &dialog.editor.as_ref().unwrap().input else {
        panic!("path editor");
    };
    assert!(input.is_completing());

    dashboard.handle_key(key(KeyCode::Down));
    dashboard.handle_key(key(KeyCode::Enter));
    let Mode::Setup(dialog) = &dashboard.mode else {
        panic!("settings");
    };
    assert_eq!(
        dialog.editor.as_ref().unwrap().input.value(),
        "/keys/id_rsa"
    );
}

/// A workspace prefix belongs to the machine it is configured on, so its
/// completions come from that machine and list directories only.
#[test]
fn setup_workspace_prefix_completes_on_its_machine() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.config.machines.insert(
        "builder".into(),
        serde_json::from_value(json!({"kind":"ssh","host":"builder"})).unwrap(),
    );
    dashboard.begin_setup();
    choose(&mut dashboard, "machines");
    choose(&mut dashboard, "builder");
    choose(&mut dashboard, "workspace_prefix");
    let Mode::Setup(dialog) = &mut dashboard.mode else {
        panic!("settings");
    };
    dialog.editor.as_mut().unwrap().input.set_value("/srv/w");
    dialog.prepare();

    let action = dashboard.handle_key(ctrl_space());
    let DashboardAction::CompletePath { host, kind, prefix } = action else {
        panic!("expected a completion request, got {action:?}");
    };
    assert_eq!(prefix, "/srv/w");
    assert_eq!(kind, mj_core::path_completion::CompletionKind::Directories);
    let mj_core::path_completion::CompletionHost::Machine(machine) = host else {
        panic!("a target path completes on its own machine");
    };
    assert!(matches!(*machine, mj_core::config::Machine::Ssh { .. }));
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

/// A row too wide for the page gives up its label before its value, and the
/// two never touch.
// Hard-won: 70f7a0f5: long row labels truncated the value that users needed to see.
#[test]
fn a_setting_row_truncates_its_label_before_its_value() {
    let line = setting_row(
        "A label far too long to fit beside its value on this page",
        "On · 3 continuations plus quota recovery",
        60,
    );
    let text: String = line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    assert!(text.chars().count() <= 60, "{text}");
    assert!(
        text.contains("… On · 3 continuations plus quota recovery"),
        "{text}"
    );

    // A value wider than the row keeps a few label cells and a space.
    let line = setting_row("Name", &"x".repeat(80), 30);
    let text: String = line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    assert!(text.chars().count() <= 30, "{text}");
    assert!(text.starts_with("  Name x"), "{text}");
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

/// With the reviewer on Auto, the first page says so, and visiting Code
/// Review does not change what the row says. Launch campaign finding C-5.
// Hard-won: 77fc1dad: returning from reviewer setup changed the draft summary.
#[test]
fn an_automatic_reviewer_is_summarized_the_same_before_and_after_a_visit() {
    let mut configured = config();
    configured.review.trigger = mj_core::config::ReviewTrigger::Turn;
    let mut dashboard = dashboard_with_session(stopped_session());
    *dashboard.config = configured.clone();
    dashboard.mode = Mode::Setup(SetupDialog::new(&configured));
    let row = |dashboard: &mut DashboardState| {
        drawn(dashboard, 140, 40)
            .into_iter()
            .find(|line| line.contains("Code Review"))
            .expect("a Code Review row")
    };
    let before = row(&mut dashboard);
    assert!(before.contains("Auto · picks by quota"), "{before}");
    assert!(!before.contains("no reviewer"), "{before}");
    choose(&mut dashboard, "review");
    dashboard.handle_key(key(KeyCode::Esc));
    assert_eq!(row(&mut dashboard), before);
}

/// The first page describes the draft, not the saved file, and a limit put
/// back to its default still shows the limit. Launch campaign finding C-22.
// Hard-won: e5bdf191: the first-page summary read saved values instead of the active draft.
#[test]
fn the_first_page_summarizes_drafted_values_before_they_are_saved() {
    let mut dialog = SetupDialog::new(&config());
    edit_field(&mut dialog, "sessionwiki", "archive_after_days", "30");
    dialog.apply_editor(false).unwrap();
    edit_field(&mut dialog, "subagents", "max_concurrent", "4");
    dialog.apply_editor(false).unwrap();
    let summary = |dialog: &SetupDialog, key: &str| {
        row_summary(&[], key, &dialog.draft[key], &dialog.draft, None)
    };
    assert_eq!(summary(&dialog, "sessionwiki"), "Archives after 30 days");
    assert_eq!(summary(&dialog, "subagents"), "Up to 4 at once");

    edit_field(&mut dialog, "subagents", "max_concurrent", "");
    dialog.apply_editor(true).unwrap();
    assert_eq!(summary(&dialog, "subagents"), "Up to 6 at once");
}

/// A long save error is shown whole: the notice grows to fit it instead of
/// stopping after three lines. Launch campaign finding C-20.
// Hard-won: 006bb14c: save notices were truncated after three rows.
#[test]
fn a_long_save_error_is_shown_whole() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    let error = format!(
        "Could not save: {} closing words",
        "Setup would change the project that a running session uses. ".repeat(5)
    );
    setup_dialog_mut(&mut dashboard.mode).unwrap().notice = Some(error);
    let lines = drawn(&mut dashboard, 120, 40);
    assert!(
        lines.iter().any(|line| line.contains("closing words")),
        "{}",
        lines.join("\n")
    );
    // The footer's controls are still drawn below it.
    assert!(
        lines.iter().any(|line| line.contains("Save")),
        "{}",
        lines.join("\n")
    );
}

/// Leaving an edited field asks about that field's change, by its name, and
/// says the rest of the draft is kept. Launch campaign finding C-15.
// Hard-won: 27dfd170: the discard prompt did not identify the field being reverted.
#[test]
fn discarding_a_field_edit_names_the_field() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_settings_section("phone", None);
    choose(&mut dashboard, "bind");
    dashboard.handle_key(key(KeyCode::Char('9')));
    dashboard.handle_key(key(KeyCode::Esc));
    assert!(dashboard.dialog_confirmation_open());
    let text = drawn(&mut dashboard, 140, 40).join("\n");
    assert!(
        text.contains("Listen address and port"),
        "the prompt names the field:\n{text}"
    );
    assert!(text.contains("rest of the Settings draft"), "{text}");
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

/// A profile page lists only the settings its kind reads: the guardian review
/// model and Codex profile belong to Codex alone.
// Hard-won: DSH campaign 2026-10-23: Settings showed guardian and Codex fields for DSH
#[test]
fn a_profile_page_hides_codex_only_settings_for_other_kinds() {
    let keys = |kind: &str| {
        let mut draft = json!({"profiles": {"p": {"kind": kind, "home": "/h"}}});
        schema::expand(&mut draft, &mut Vec::new());
        let path = vec!["profiles".to_owned(), "p".to_owned()];
        visible_keys(&path, draft.pointer(&pointer(&path)).unwrap())
    };
    let codex_only = ["guardian_review_model", "codex_profile"];
    assert!(
        codex_only
            .iter()
            .all(|key| keys("codex").contains(&(*key).to_owned()))
    );
    for kind in ["claude", "kimi", "grok", "muse", "opencode", "mimo", "dsh"] {
        assert!(
            !keys(kind)
                .iter()
                .any(|key| codex_only.contains(&key.as_str())),
            "{kind}"
        );
    }
}

/// Opens the field `key` on the page `section` and types `text` into it.
fn edit_field(dialog: &mut SetupDialog, section: &str, key: &str, text: &str) {
    dialog.path = vec![section.to_owned()];
    dialog.selected = dialog
        .keys()
        .iter()
        .position(|candidate| candidate == key)
        .unwrap_or_else(|| panic!("{section} has no {key}"));
    dialog.open_selected();
    dialog.editor.as_mut().expect("a text editor").input = EditorInput::Text(TextInput::from(text));
}

fn saved_config(dialog: &mut SetupDialog) -> Config {
    let action = dialog.save(&Default::default());
    dialog.saving = false;
    match action {
        DashboardAction::SaveSetup { updated, .. } => serde_json::from_str(&updated).unwrap(),
        other => panic!("save refused: {other:?}, notice {:?}", dialog.notice),
    }
}

// Hard-won: #1153: saving unrelated Settings fields overwrote named-instance web listener ports.
#[test]
fn named_instance_settings_preserve_the_default_and_explicit_web_listener() {
    const CHILD: &str = "MJ_TEST_SETTINGS_INSTANCE_CHILD";
    const INSTANCE: &str = "settings-1153";
    if std::env::var_os(CHILD).is_none() {
        // Instance selection is process-wide: exercise a named instance in
        // its own process rather than changing the other tests' environment.
        let output = mj_core::subprocess::run_with_input(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "setup::tests::named_instance_settings_preserve_the_default_and_explicit_web_listener",
                    "--exact",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env("MJ_INSTANCE", INSTANCE),
            &[],
        )
        .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let default = mj_core::config::default_phone_bind_for(Some(INSTANCE));
    for bind in [&default, &"127.0.0.1:39777".to_owned()] {
        let mut dashboard = dashboard_with_session(stopped_session());
        dashboard.config.phone = mj_core::config::PhoneConfig::default();
        dashboard.config.phone.bind.clone_from(bind);
        if bind == &default {
            assert!(
                serde_json::to_value(&*dashboard.config)
                    .unwrap()
                    .get("phone")
                    .is_none()
            );
        }
        dashboard.begin_setup();
        let rendered = drawn(&mut dashboard, 140, 48).join("\n");
        assert!(rendered.contains(&format!("On · {bind}")), "{rendered}");
        let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
        assert!(
            !dialog.is_dirty(),
            "opening Settings must preserve defaults"
        );
        dialog.draft["keys"]["prefix"] = json!("ctrl+]");
        let saved = saved_config(dialog);
        assert_eq!(&saved.phone.bind, bind);
        assert_eq!(saved.keys.prefix, "ctrl+]");
    }
}

/// The numeric settings are edited as text but saved as numbers, "Use
/// default" puts the default back, and text that is not a number is refused
/// under the field without losing the draft. Launch campaign finding C-23.
// Hard-won: ef88371a: numeric field text failed to convert during save.
#[test]
fn numeric_settings_round_trip_use_their_default_and_refuse_text() {
    type Read = fn(&Config) -> u64;
    let fields: [(&str, &str, Read, u64); 3] = [
        ("notify", "delay_seconds", |c| c.notify.delay_seconds, 2),
        (
            "sessionwiki",
            "archive_after_days",
            |c| c.sessionwiki.archive_after_days.map_or(0, u64::from),
            0,
        ),
        (
            "subagents",
            "max_concurrent",
            |c| u64::try_from(c.subagents.max_concurrent).unwrap(),
            6,
        ),
    ];
    for (section, key, read, default) in fields {
        let mut dialog = SetupDialog::new(&config());
        edit_field(&mut dialog, section, key, "5");
        dialog.apply_editor(false).unwrap();
        assert_eq!(dialog.draft[section][key], 5, "{section}.{key}");
        assert_eq!(read(&saved_config(&mut dialog)), 5, "{section}.{key}");

        edit_field(&mut dialog, section, key, "5");
        dialog.apply_editor(true).unwrap();
        assert_eq!(read(&saved_config(&mut dialog)), default, "{section}.{key}");

        edit_field(&mut dialog, section, key, "five");
        let error = dialog.apply_editor(false).unwrap_err();
        assert!(error.starts_with("Enter a whole number"), "{error}");
        assert!(
            dialog.editor.is_some(),
            "{section}.{key}: the field stays open"
        );
    }
}

#[test]
fn user_managed_budgets_show_host_values_and_cannot_be_edited() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    choose(&mut dashboard, "machines");
    choose(&mut dashboard, "local");
    choose(&mut dashboard, "build_cache");
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    let (_, preview_key) = dialog.build_cache_page().unwrap();
    dialog.build_cache_preview = Some(BuildCachePreviewState {
        key: preview_key,
        install_mbx_available: false,
        result: BuildCachePreviewResult::Ready(Some(Box::new(mj_core::state::BuildCachePreview {
            native_mbx: Some("1.21.0".into()),
            mbx_profile_file: None,
            mbx_profile_warning: None,
            mbx_manual_path_line: None,
            directory: Some("/native/cache".into()),
            max_total_size: Some(mj_core::state::BuildCacheLimit::HostConfiguration(Some(
                "500GB".into(),
            ))),
            user_managed: true,
            application: mj_core::state::BuildCacheApplication::Applied,
            budget_note: None,
            stats: None,
            off_reason: None,
        }))),
    });
    dialog.prepare();
    let text = drawn(&mut dashboard, 140, 30).join("\n");
    assert!(text.contains("User-managed mbx"), "{text}");
    assert!(text.contains("500 GB, host mbx config"), "{text}");
    choose(&mut dashboard, "max_total_size");
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    assert!(dialog.editor.is_none());
    assert_eq!(
        dialog.draft["machines"]["local"]["build_cache"]["max_total_size"],
        Value::Null
    );
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
        crate::CommandId::Help,
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
        dialog.build_cache_preview.as_ref().map(|p| &p.key),
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

    let preview = dialog.build_cache_preview.as_mut().unwrap();
    preview.install_mbx_available = false;
    dialog.prepare();
    assert!(
        !dialog
            .actions()
            .iter()
            .any(|(control, _, _)| *control == SetupControl::InstallMbx)
    );
    let preview = dialog.build_cache_preview.as_mut().unwrap();
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

/// A status line belongs to the page or prompt that set it. Finding T-4.
// Hard-won: 6094bb1c: a prompt status line remained after its owning settings page closed.
#[test]
fn a_status_line_does_not_outlive_the_prompt_that_set_it() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_settings_section("profiles", None);
    dashboard.handle_key(key(KeyCode::Char('a')));
    dashboard.handle_key(key(KeyCode::Enter));
    let shown = setup_dialog_mut(&mut dashboard.mode)
        .unwrap()
        .notice
        .clone();
    assert_eq!(shown.as_deref(), Some("Choose a name for the new entry."));
    dashboard.handle_key(key(KeyCode::Esc));
    choose(&mut dashboard, "codex-1");
    choose(&mut dashboard, "environment");
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    assert_eq!(dialog.notice, None, "the name prompt's status lingered");
    let page = drawn(&mut dashboard, 140, 40).join("\n");
    assert!(!page.contains("Choose a name"), "{page}");
}

#[tokio::test]
async fn settings_owns_the_terminal_cursor_and_restores_the_composer_after_dismissal() {
    use mj_chat::chat::{ActiveChat, Notices, SessionHeaderIdentity};
    let session = running_session();
    let id = session.id.clone();
    let mut dashboard = dashboard_with_session(session);
    dashboard.focus_prompt();
    let fixture = mj_client::session::replacement_session_test_fixture(&id, 1);
    let chat = ActiveChat::open(
        fixture.stopped,
        "hel",
        None,
        fixture.control,
        SessionHeaderIdentity::default(),
        String::new(),
        Notices::default(),
    );
    let mut chats = std::collections::BTreeMap::from([(id, chat)]);
    use ratatui::{TerminalOptions, Viewport, backend::CrosstermBackend};
    #[derive(Clone, Default)]
    struct Output(std::rc::Rc<RefCell<Vec<u8>>>);
    impl std::io::Write for Output {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let output = Output::default();
    let mut terminal = Terminal::with_options(
        CrosstermBackend::new(output.clone()),
        TerminalOptions {
            viewport: Viewport::Fixed(Rect::new(0, 0, 140, 40)),
        },
    )
    .unwrap();
    let draw = |terminal: &mut Terminal<CrosstermBackend<Output>>,
                dashboard: &mut DashboardState,
                chats: &mut std::collections::BTreeMap<String, ActiveChat>| {
        output.0.borrow_mut().clear();
        terminal
            .draw(|frame| {
                crate::combined::render_combined_for_test(
                    frame,
                    dashboard,
                    chats,
                    &Default::default(),
                    false,
                );
            })
            .unwrap();
    };
    draw(&mut terminal, &mut dashboard, &mut chats);
    assert!(
        output
            .0
            .borrow()
            .windows(6)
            .any(|bytes| bytes == b"\x1b[?25h"),
        "composer owns the initial cursor"
    );
    dashboard.begin_settings_section("notify", None);
    draw(&mut terminal, &mut dashboard, &mut chats);
    assert!(
        output
            .0
            .borrow()
            .windows(6)
            .any(|bytes| bytes == b"\x1b[?25l"),
        "settings controls cannot inherit the background cursor"
    );
    choose(&mut dashboard, "delay_seconds");
    draw(&mut terminal, &mut dashboard, &mut chats);
    assert!(
        output
            .0
            .borrow()
            .windows(6)
            .any(|bytes| bytes == b"\x1b[?25h"),
        "focused modal text field owns its cursor"
    );
    dashboard.handle_key(key(KeyCode::Esc));
    draw(&mut terminal, &mut dashboard, &mut chats);
    assert!(
        output
            .0
            .borrow()
            .windows(6)
            .any(|bytes| bytes == b"\x1b[?25l"),
        "closing the editor hides its cursor"
    );
    dashboard.handle_key(key(KeyCode::Esc));
    assert!(
        matches!(dashboard.mode, Mode::Setup(_)),
        "Esc returns to the Settings root"
    );
    dashboard.handle_key(key(KeyCode::Esc));
    draw(&mut terminal, &mut dashboard, &mut chats);
    assert!(
        output
            .0
            .borrow()
            .windows(6)
            .any(|bytes| bytes == b"\x1b[?25h"),
        "dismissal restores composer cursor"
    );
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
    for (task, model) in [("straightforward", "shared"), ("subtle", "smarter")] {
        choose(&mut dashboard, task);
        choose(&mut dashboard, "model");
        let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
        let editor = dialog.editor.as_mut().unwrap();
        assert_eq!(editor.choices.iter().filter(|c| **c == "shared").count(), 1);
        editor.selected = editor.choices.iter().position(|c| c == model).unwrap();
        dialog.apply_editor(false).unwrap();
        dashboard.take_setup_subagent_choices();
        choose(&mut dashboard, "effort");
        let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
        let editor = dialog.editor.as_mut().unwrap();
        editor.selected = editor.choices.iter().position(|c| c == "high").unwrap();
        dialog.apply_editor(false).unwrap();
        append_setup_state(&mut output, task, &mut dashboard, 140, 40);
        setup_dialog_mut(&mut dashboard.mode).unwrap().path.pop();
    }
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    let DashboardAction::SaveSetup { updated, .. } = dialog.save(&Default::default()) else {
        panic!("save");
    };
    let saved: Config = serde_json::from_str(&updated).unwrap();
    assert!(saved.subagents.is_configured());
    assert_eq!(saved.subagents.subtle.unwrap().model, "smarter");
    dialog.saving = false;
    dialog.path = vec!["subagents".into(), "straightforward".into()];
    // Discovery arriving while the picker is open must retain a saved selection.
    dashboard.profile_capabilities = Default::default();
    dashboard.take_setup_subagent_choices();
    choose(&mut dashboard, "effort");
    dashboard.set_profile_capabilities(snapshot);
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    let editor = dialog.editor.as_ref().unwrap();
    assert_eq!(editor.choices[editor.selected], "high");
    dialog.apply_editor(false).unwrap();
    choose(&mut dashboard, "model");
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    let editor = dialog.editor.as_mut().unwrap();
    editor.selected = editor.choices.iter().position(|c| c == "smarter").unwrap();
    dialog.apply_editor(false).unwrap();
    assert!(dialog.draft["subagents"]["straightforward"]["effort"].is_null());
    choose(&mut dashboard, "model");
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    dialog.apply_editor(true).unwrap();
    let config = config_from_draft(dialog.draft.clone()).unwrap();
    assert!(config.subagents.straightforward.is_none());
    assert!(!config.subagents.is_configured());
    assert!(dialog.draft["subagents"]["straightforward"]["effort"].is_null());
    append_setup_state(&mut output, "cleared choice", &mut dashboard, 140, 40);
    mj_core::golden::assert_golden(env!("CARGO_MANIFEST_DIR"), "global-subagents", &output);
}
