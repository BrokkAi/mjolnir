//! Container settings composed from reusable terminal controls.

use std::cell::{Cell, RefCell};

use crossterm::event::{Event, KeyEventKind};
use mj_chat::components::{
    ButtonRow, Checkbox, ChoiceList, ControlKind, Form, FormViewport, Interaction, Outcome,
    TextField,
};
use ratatui::layout::Margin;

use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ContainerEditor {
    pub(crate) session_id: String,
    pub(crate) cpus: TextInput,
    pub(crate) memory: TextInput,
    pub(crate) mounts: Vec<AdditionalMount>,
    pub(crate) suggestions: Vec<PathBuf>,
    pub(crate) source: TextInput,
    pub(crate) destination: TextInput,
    pub(crate) read_only: bool,
    pub(crate) form: RefCell<Form<ContainerEditFocus>>,
    pub(crate) mount_index: usize,
    pub(crate) suggestion_index: usize,
    pub(crate) error: Option<String>,
    scroll: Cell<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContainerEditFocus {
    Cpus,
    Memory,
    Source,
    Destination,
    ReadOnly,
    Mounts,
    Suggestions,
    Cancel,
    Save,
}

pub(crate) const CONTAINER_EDIT_SCOPE: &str = "Applies when the container is next recreated.";

enum Row<'a> {
    Text(Line<'a>),
    Field(ContainerEditFocus, &'static str, &'a TextInput),
    Check,
    List(ContainerEditFocus, Vec<Line<'static>>, usize),
}

impl Row<'_> {
    fn height(&self) -> u16 {
        match self {
            Self::List(_, lines, _) => u16::try_from(lines.len()).unwrap_or(u16::MAX).max(1),
            _ => 1,
        }
    }

    fn control(&self) -> Option<(ContainerEditFocus, ControlKind)> {
        match self {
            Self::Field(id, ..) => Some((*id, ControlKind::TextField)),
            Self::Check => Some((ContainerEditFocus::ReadOnly, ControlKind::Checkbox)),
            Self::List(id, rows, selected) => Some((
                *id,
                ControlKind::ChoiceList {
                    len: rows.len(),
                    selected: *selected,
                },
            )),
            Self::Text(_) => None,
        }
    }
}

impl ContainerEditor {
    pub(crate) fn focused(&self) -> ContainerEditFocus {
        self.form
            .borrow()
            .focused()
            .unwrap_or(ContainerEditFocus::Cpus)
    }

    pub(crate) fn field(&self) -> Option<&TextInput> {
        match self.focused() {
            ContainerEditFocus::Cpus => Some(&self.cpus),
            ContainerEditFocus::Memory => Some(&self.memory),
            ContainerEditFocus::Source => Some(&self.source),
            ContainerEditFocus::Destination => Some(&self.destination),
            _ => None,
        }
    }

    fn field_mut(&mut self, id: ContainerEditFocus) -> Option<&mut TextInput> {
        match id {
            ContainerEditFocus::Cpus => Some(&mut self.cpus),
            ContainerEditFocus::Memory => Some(&mut self.memory),
            ContainerEditFocus::Source => Some(&mut self.source),
            ContainerEditFocus::Destination => Some(&mut self.destination),
            _ => None,
        }
    }

    fn rows(&self) -> Vec<Row<'_>> {
        use ContainerEditFocus::*;
        let mut rows = vec![
            Row::Text(Line::raw(format!("Session: {}", self.session_id))),
            Row::Text(Line::styled(
                CONTAINER_EDIT_SCOPE,
                Style::default().fg(Color::DarkGray),
            )),
            Row::Text(Line::raw("")),
            Row::Field(Cpus, "CPUs", &self.cpus),
            Row::Field(Memory, "Memory", &self.memory),
            Row::Text(Line::styled(
                "Empty keeps the target's value.",
                Style::default().fg(Color::DarkGray),
            )),
            Row::Text(Line::raw("")),
            Row::Text(Line::raw("Attached directories")),
        ];
        if self.mounts.is_empty() {
            rows.push(Row::Text(Line::styled(
                "  none",
                Style::default().fg(Color::DarkGray),
            )));
        } else {
            rows.push(Row::List(
                Mounts,
                self.mounts
                    .iter()
                    .map(|mount| {
                        Line::raw(format!(
                            "{} -> {}{}",
                            mount.source.display(),
                            mount.destination.display(),
                            read_only_marker(mount.read_only)
                        ))
                    })
                    .collect(),
                self.mount_index,
            ));
        }
        rows.extend([
            Row::Text(Line::raw("")),
            Row::Field(Source, "Attach host directory", &self.source),
            Row::Field(Destination, "Container destination", &self.destination),
            Row::Check,
        ]);
        if !self.suggestions.is_empty() {
            rows.push(Row::Text(Line::raw("")));
            rows.push(Row::Text(Line::raw("Remembered directories")));
            rows.push(Row::List(
                Suggestions,
                self.suggestions
                    .iter()
                    .map(|path| Line::raw(path.display().to_string()))
                    .collect(),
                self.suggestion_index,
            ));
        }
        if let Some(error) = &self.error {
            rows.push(Row::Text(Line::styled(
                error.clone(),
                Style::default().fg(Color::Yellow),
            )));
        }
        rows
    }

    /// Refresh eligibility after a domain change, including before the first frame.
    fn prepare(&self) {
        let mut form = self.form.borrow_mut();
        form.begin_frame();
        for row in self.rows() {
            if let Some((id, kind)) = row.control() {
                form.register(id, kind, Rect::default(), true);
            }
        }
        form.register(
            ContainerEditFocus::Cancel,
            ControlKind::Button,
            Rect::default(),
            true,
        );
        form.register(
            ContainerEditFocus::Save,
            ControlKind::Button,
            Rect::default(),
            true,
        );
        form.end_frame(ContainerEditFocus::Cpus);
    }
    /// Add the typed mount, filling in a default destination. Returns the
    /// reason it was rejected, if it was.
    fn add_mount(&mut self) -> Option<String> {
        let source = PathBuf::from(self.source.trim());
        if source.as_os_str().is_empty() {
            return Some("Enter a host directory to attach.".into());
        }
        let destination = if self.destination.trim().is_empty() {
            default_mount_destination(&source, &self.mounts)
        } else {
            PathBuf::from(self.destination.trim())
        };
        let mount = AdditionalMount {
            source,
            destination,
            read_only: self.read_only,
        };
        let mut mounts = self.mounts.clone();
        mounts.push(mount);
        if let Err(error) = validate_additional_mounts(&mounts) {
            return Some(error.to_string());
        }
        self.mounts = mounts;
        self.source.clear();
        self.destination.clear();
        self.read_only = false;
        self.mount_index = self.mounts.len() - 1;
        None
    }

    /// Toggle read-only for the entry being typed, or for the selected row.
    fn toggle_read_only(&mut self) {
        match self.focused() {
            ContainerEditFocus::ReadOnly => self.read_only = !self.read_only,
            ContainerEditFocus::Mounts => {
                if let Some(mount) = self.mounts.get_mut(self.mount_index) {
                    mount.read_only = !mount.read_only;
                }
            }
            _ => {}
        }
    }

    fn take_suggestion(&mut self) {
        let Some(source) = self.suggestions.get(self.suggestion_index) else {
            return;
        };
        self.source = source.to_string_lossy().into_owned().into();
        self.destination = default_mount_destination(source, &self.mounts)
            .to_string_lossy()
            .into_owned()
            .into();
        self.form.get_mut().focus(ContainerEditFocus::Source);
    }

    fn remove_selected(&mut self) {
        match self.focused() {
            ContainerEditFocus::Mounts if !self.mounts.is_empty() => {
                self.mounts.remove(self.mount_index);
                self.mount_index = self.mount_index.min(self.mounts.len().saturating_sub(1));
                if self.mounts.is_empty() {
                    self.form.get_mut().focus(ContainerEditFocus::Source);
                }
            }
            ContainerEditFocus::Suggestions if !self.suggestions.is_empty() => {
                self.suggestions.remove(self.suggestion_index);
                self.suggestion_index = self
                    .suggestion_index
                    .min(self.suggestions.len().saturating_sub(1));
                if self.suggestions.is_empty() {
                    self.form.get_mut().focus(ContainerEditFocus::Source);
                }
            }
            _ => {}
        }
    }

    fn save(&self) -> Result<DashboardAction, String> {
        validate_additional_mounts(&self.mounts).map_err(|error| error.to_string())?;
        let value = |text: &str| {
            let trimmed = text.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_owned())
        };
        Ok(DashboardAction::SaveContainerSettings {
            session_id: self.session_id.clone(),
            cpus: value(&self.cpus),
            memory: value(&self.memory),
            additional_mounts: self.mounts.clone(),
            mount_history: self.suggestions.clone(),
        })
    }
}

pub(crate) fn render_container_editor(
    frame: &mut Frame,
    area: Rect,
    editor: &ContainerEditor,
    surfaces: &mut FrameSurfaces,
) {
    let rows = editor.rows();
    let total = rows
        .iter()
        .fold(0_u16, |height, row| height.saturating_add(row.height()));
    let popup = centered_modal(frame, surfaces, 70, total.saturating_add(5).max(18), area);
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .title(" Edit container size and mounts "),
        popup,
    );
    let inner = popup.inner(Margin {
        horizontal: 1,
        vertical: 1,
    });
    let body = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner.height.saturating_sub(2),
    );
    let focused = editor.focused();
    let mut focus_row = None;
    let mut row_y = 0_u16;
    for row in &rows {
        if row.control().is_some_and(|(id, _)| id == focused) {
            let offset = match row {
                Row::List(_, _, selected) => u16::try_from(*selected).unwrap_or(u16::MAX),
                _ => 0,
            };
            focus_row = Some(row_y.saturating_add(offset));
        }
        row_y = row_y.saturating_add(row.height());
    }
    let viewport = FormViewport::new(body, total, editor.scroll.get(), focus_row);
    editor.scroll.set(viewport.offset());
    let mut form = editor.form.borrow_mut();
    form.begin_frame();
    row_y = 0;
    for row in rows {
        let height = row.height();
        let rect = viewport.row(row_y, height);
        let visible_height = rect.height;
        match row {
            Row::Text(line) => {
                if visible_height > 0 {
                    frame.render_widget(line, rect);
                }
            }
            Row::Field(id, label, input) => {
                let label = format!("{label}: ");
                let label_width =
                    (Line::raw(label.clone()).width() as u16).min(rect.width.saturating_sub(1));
                if visible_height > 0 {
                    frame.render_widget(
                        Line::raw(label),
                        Rect::new(rect.x, rect.y, label_width, rect.height),
                    );
                }
                TextField::render(
                    frame,
                    Rect::new(
                        rect.x.saturating_add(label_width),
                        rect.y,
                        rect.width.saturating_sub(label_width),
                        rect.height,
                    ),
                    input,
                    &mut form,
                    id,
                );
            }
            Row::Check => Checkbox::render(
                frame,
                rect,
                "Read-only",
                editor.read_only,
                true,
                &mut form,
                ContainerEditFocus::ReadOnly,
            ),
            Row::List(id, lines, selected) => {
                // ChoiceList keeps the selected row visible within the supplied viewport.
                ChoiceList::render(frame, rect, &lines, selected, &mut form, id);
            }
        }
        row_y = row_y.saturating_add(height);
    }
    if inner.height > 1 {
        frame.render_widget(
            Line::styled(
                "Enter attaches/accepts · Space toggles · d removes · Tab moves",
                Style::default().fg(Color::DarkGray),
            ),
            Rect::new(inner.x, inner.bottom() - 2, inner.width, 1),
        );
    }
    let footer = Rect::new(
        inner.x,
        inner.bottom().saturating_sub(1),
        inner.width,
        u16::from(inner.height > 0),
    );
    ButtonRow::render(
        frame,
        footer,
        &[
            (ContainerEditFocus::Cancel, "Cancel", true),
            (ContainerEditFocus::Save, "Save", true),
        ],
        &mut form,
    );
    form.end_frame(ContainerEditFocus::Cpus);
}

impl DashboardState {
    pub(crate) fn begin_container_edit(&mut self) {
        let Some(session) = self.selected_container_session() else {
            self.notices
                .set("Container size and mounts apply to container targets only.");
            return;
        };
        let suggestions = self
            .config
            .targets
            .get(&session.target_template_id)
            .and_then(mount_history_host)
            .and_then(|host| self.state.mount_history.get(host))
            .cloned()
            .unwrap_or_default();
        let editor = ContainerEditor {
            session_id: session.id.clone(),
            cpus: session.container_cpus.clone().unwrap_or_default().into(),
            memory: session.container_memory.clone().unwrap_or_default().into(),
            mounts: session.additional_mounts.clone(),
            suggestions,
            source: TextInput::new(),
            destination: TextInput::new(),
            read_only: false,
            form: RefCell::new(Form::default()),
            mount_index: 0,
            suggestion_index: 0,
            error: None,
            scroll: Cell::new(0),
        };
        editor.prepare();
        self.mode = Mode::EditContainer(editor);
    }

    pub(crate) fn handle_container_edit_event(
        &mut self,
        event: Event,
        mut editor: ContainerEditor,
    ) -> DashboardAction {
        use ContainerEditFocus::*;
        // Removing a directory is a domain operation on the focused list.
        if matches!(&event, Event::Key(key) if key.kind == KeyEventKind::Press && key.modifiers.is_empty() && matches!(key.code, KeyCode::Delete | KeyCode::Char('d')))
            && matches!(editor.focused(), Mounts | Suggestions)
        {
            editor.remove_selected();
            editor.prepare();
            self.mode = Mode::EditContainer(editor);
            return DashboardAction::None;
        }
        let interaction = editor.form.get_mut().handle(&event).action;
        let mut changed_structure = false;
        match interaction {
            Some(Interaction::Cancel | Interaction::Activate(Cancel)) => {
                self.cancel_modal();
                return DashboardAction::None;
            }
            Some(Interaction::Edit(id, edit)) => {
                if editor
                    .field_mut(id)
                    .is_some_and(|field| TextField::apply(field, edit) == Outcome::Changed)
                {
                    editor.error = None;
                }
            }
            Some(Interaction::Select(Mounts, index)) => editor.mount_index = index,
            Some(Interaction::Select(Suggestions, index)) => editor.suggestion_index = index,
            Some(Interaction::Toggle(ReadOnly | Mounts)) => editor.toggle_read_only(),
            Some(Interaction::Activate(Suggestions)) => {
                editor.take_suggestion();
                editor.error = None;
            }
            Some(Interaction::Activate(Source | Destination)) => {
                editor.error = editor.add_mount();
                changed_structure = true;
            }
            Some(Interaction::Activate(Cpus | Memory | Mounts | Save)) => match editor.save() {
                Ok(action) => {
                    self.cancel_modal();
                    return action;
                }
                Err(error) => editor.error = Some(error),
            },
            _ => {}
        }
        if changed_structure {
            editor.prepare();
        }
        self.mode = Mode::EditContainer(editor);
        DashboardAction::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        buffer_lines, cell_column, dashboard_with_session, key, running_session,
    };
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    use ratatui::{Terminal, backend::TestBackend};

    fn open() -> DashboardState {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.begin_container_edit();
        assert!(matches!(dashboard.mode, Mode::EditContainer(_)));
        dashboard
    }

    fn draw(dashboard: &mut DashboardState, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| crate::render::render(frame, dashboard))
            .unwrap();
        buffer_lines(terminal.backend().buffer())
    }

    fn point(lines: &[String], label: &str) -> (u16, u16) {
        let y = lines.iter().position(|line| line.contains(label)).unwrap();
        (cell_column(&lines[y], label), y as u16)
    }

    fn pointer(kind: MouseEventKind, point: (u16, u16)) -> MouseEvent {
        MouseEvent {
            kind,
            column: point.0,
            row: point.1,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn save_click_requires_release_inside_and_focuses_on_first_press() {
        let mut dashboard = open();
        dashboard.handle_key(key(KeyCode::Char('4')));
        let lines = draw(&mut dashboard, 100, 40);
        let save = point(&lines, "[ Save ]");
        assert_eq!(
            dashboard.handle_mouse(pointer(MouseEventKind::Down(MouseButton::Left), save)),
            DashboardAction::None
        );
        let Mode::EditContainer(editor) = &dashboard.mode else {
            panic!("editor closed on press")
        };
        assert_eq!(editor.focused(), ContainerEditFocus::Save);
        assert!(
            dashboard
                .component_handles_mouse(pointer(MouseEventKind::Up(MouseButton::Left), (0, 0)))
        );
        assert_eq!(
            dashboard.handle_mouse(pointer(MouseEventKind::Up(MouseButton::Left), (0, 0))),
            DashboardAction::None
        );
        assert!(matches!(dashboard.mode, Mode::EditContainer(_)));
        dashboard.handle_mouse(pointer(MouseEventKind::Down(MouseButton::Left), save));
        assert!(
            matches!(dashboard.handle_mouse(pointer(MouseEventKind::Up(MouseButton::Left), save)), DashboardAction::SaveContainerSettings { cpus: Some(value), .. } if value == "4")
        );
    }

    #[test]
    fn small_form_scrolls_to_field_and_help_restores_its_focus() {
        let mut dashboard = open();
        dashboard.handle_key(key(KeyCode::Tab));
        dashboard.handle_key(key(KeyCode::Tab));
        let Mode::EditContainer(editor) = &dashboard.mode else {
            panic!("editor")
        };
        assert_eq!(editor.focused(), ContainerEditFocus::Source);
        dashboard.handle_paste("/srv/資料 with spaces");
        let lines = draw(&mut dashboard, 72, 18);
        assert!(
            lines
                .iter()
                .any(|line| line.contains("Attach host directory:"))
        );
        dashboard.begin_help();
        dashboard.handle_key(key(KeyCode::Esc));
        let Mode::EditContainer(editor) = &dashboard.mode else {
            panic!("help did not restore editor")
        };
        assert_eq!(editor.focused(), ContainerEditFocus::Source);
        assert_eq!(editor.source.value(), "/srv/資料 with spaces");
        let lines = draw(&mut dashboard, 40, 10);
        assert!(lines.iter().any(|line| line.contains("Terminal too small")));
        let lines = draw(&mut dashboard, 72, 18);
        assert!(lines.iter().any(|line| line.contains("[ Save ]")));
    }

    #[test]
    fn too_small_frame_removes_stale_button_hitboxes() {
        let mut dashboard = open();
        let save = point(&draw(&mut dashboard, 100, 40), "[ Save ]");
        let lines = draw(&mut dashboard, 20, 8);
        assert!(lines.iter().any(|line| line.contains("Terminal too small")));
        let down = pointer(MouseEventKind::Down(MouseButton::Left), save);
        assert!(!dashboard.component_handles_mouse(down));
        assert_eq!(dashboard.handle_mouse(down), DashboardAction::None);
        assert_eq!(
            dashboard.handle_mouse(pointer(MouseEventKind::Up(MouseButton::Left), save)),
            DashboardAction::None
        );
        assert!(matches!(dashboard.mode, Mode::EditContainer(_)));
    }
}
