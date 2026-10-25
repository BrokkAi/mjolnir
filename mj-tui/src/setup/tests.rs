use super::*;
use crate::test_support::{
    buffer_lines, cell_column, chord, config, dashboard_with_session, drawn, key, point,
    running_session, stopped_session,
};
use crossterm::event::{KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{Terminal, backend::TestBackend};

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

fn choose(dashboard: &mut DashboardState, name: &str) -> DashboardAction {
    let target = {
        let dialog = setup_dialog_mut(&mut dashboard.mode).expect("settings");
        dialog.prepare();
        dialog
            .descriptors
            .iter()
            .find(|c| c.path.last().is_some_and(|key| key == name))
            .unwrap_or_else(|| panic!("missing {name:?} on {:?}", dialog.path))
            .id
    };
    for _ in 0..100 {
        let focused = setup_dialog_mut(&mut dashboard.mode)
            .unwrap()
            .form
            .borrow()
            .focused();
        if focused == Some(target) {
            break;
        }
        dashboard.handle_key(key(KeyCode::Tab));
    }
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    assert!(dialog.form.borrow().is_focused(target));
    let field = dialog.descriptor(target).unwrap().kind == SettingKind::Field;
    if field {
        DashboardAction::None
    } else {
        dashboard.handle_key(key(KeyCode::Enter))
    }
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
    choose(dashboard, name)
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

fn detection(scope: DetectScope, config: Config) -> crate::setup::SetupDetection {
    crate::setup::SetupDetection {
        scope,
        config,
        rejected_runtimes: Vec::new(),
        machines: Vec::new(),
    }
}

/// Open the selected field's choice list and commit `wanted`.
fn select_choice(dashboard: &mut DashboardState, wanted: &str) {
    select_value(dashboard, &Value::String(wanted.to_owned()));
}

fn select_value(dashboard: &mut DashboardState, wanted: &Value) {
    let (current, target) = {
        let dialog = setup_dialog_mut(&mut dashboard.mode).expect("settings");
        let (values, selected) = open_choices(dialog);
        (
            selected,
            values
                .iter()
                .position(|value| value == wanted)
                .unwrap_or_else(|| panic!("{wanted:?} is not offered: {values:?}")),
        )
    };
    let step = if current > target {
        KeyCode::Up
    } else {
        KeyCode::Down
    };
    for _ in 0..current.abs_diff(target) {
        dashboard.handle_key(key(step));
    }
    dashboard.handle_key(key(KeyCode::Enter));
}

fn open_choices(dialog: &SetupDialog) -> (&[Value], usize) {
    let id = dialog.combo.open_id().expect("dropdown open");
    let control = dialog.descriptor(id).unwrap();
    let SettingKind::Choices(values) = &control.kind else {
        panic!("dropdown choices");
    };
    let selected = values
        .iter()
        .position(|v| dialog.draft.pointer(&pointer(&control.path)) == Some(v))
        .unwrap_or(0);
    (values, dialog.combo.selection(id, selected))
}

fn focused_field(dialog: &SetupDialog) -> &PendingField {
    &dialog.fields[&dialog.focused_field_path().expect("focused field")]
}

fn focused_field_mut(dialog: &mut SetupDialog) -> &mut PendingField {
    let path = dialog.focused_field_path().expect("focused field");
    dialog.fields.get_mut(&path).expect("field buffer")
}

fn focus_setting(dialog: &mut SetupDialog, name: &str) {
    dialog.prepare();
    let mut path = dialog.path.clone();
    path.push(name.to_owned());
    dialog.focus_path(&path);
}

fn replace_text(dashboard: &mut DashboardState, text: &str) {
    dashboard.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
    dashboard.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
    dashboard.handle_paste(text);
    if text.is_empty() {
        dashboard.handle_key(key(KeyCode::Backspace));
    }
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

fn ctrl_space() -> KeyEvent {
    KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL)
}

/// Opens the field `key` on the page `section` and types `text` into it.
fn edit_field(dialog: &mut SetupDialog, section: &str, field: &str, text: &str) {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.mode = Mode::Setup(dialog.clone());
    loop {
        let setup = setup_dialog_mut(&mut dashboard.mode).unwrap();
        setup.saving = false;
        if setup.path.is_empty() {
            break;
        }
        dashboard.handle_key(key(KeyCode::Esc));
    }
    choose(&mut dashboard, section);
    choose(&mut dashboard, field);
    replace_text(&mut dashboard, text);
    let Mode::Setup(updated) = dashboard.mode else {
        panic!("settings");
    };
    *dialog = updated;
}

fn apply_focused(dialog: &mut SetupDialog, clear: bool) -> Result<(), String> {
    let path = dialog.focused_path().expect("focused field");
    if clear {
        focused_field_mut(dialog).input.clear();
    }
    dialog.apply_pending(&path)
}

fn saved_config(dialog: &mut SetupDialog) -> Config {
    let action = dialog.save(&Default::default());
    dialog.saving = false;
    match action {
        DashboardAction::SaveSetup { updated, .. } => serde_json::from_str(&updated).unwrap(),
        other => panic!("save refused: {other:?}, notice {:?}", dialog.notice),
    }
}

mod build_cache;
mod collections;
mod editing;
mod goldens;
mod navigation;
mod paths;
mod profiles;
mod rendering;
mod results;
mod review;
mod search;
mod settings;
