//! Workspace input state shared by keyboard, pointer, and rendered forms.

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};
use mj_chat::components::{ButtonRow, ChoiceList, ControlKind, Form, Interaction, TextField};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Wrap};

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Control {
    Workspaces,
    Open,
    New,
    Rename,
    Recover,
    Delete,
    Back,
    Name,
    Submit,
    Cancel,
}

const ACTIONS: [(Control, &str); 6] = [
    (Control::Open, "Open"),
    (Control::New, "New"),
    (Control::Rename, "Rename"),
    (Control::Recover, "Recover"),
    (Control::Delete, "Delete"),
    (Control::Back, "Back"),
];

pub(super) struct WorkspaceControls {
    pub(super) selected: usize,
    editing: Option<EditMode>,
    confirming: Option<ConfirmDelete>,
    input: TextInput,
    form: Form<Control>,
    suggested_name: String,
}

impl WorkspaceControls {
    pub(super) fn new(selected: usize, suggested_name: &str) -> Self {
        Self {
            selected,
            editing: None,
            confirming: None,
            input: TextInput::new().with_max_chars(64),
            form: Form::default(),
            suggested_name: suggested_name.to_owned(),
        }
    }

    pub(super) fn modal_open(&self) -> bool {
        self.editing.is_some() || self.confirming.is_some()
    }

    fn buttons(
        &self,
        workspaces: &[WorkspaceListing],
        snapshot: Option<&WorkspaceSnapshot>,
    ) -> [(Control, &'static str, bool); 6] {
        let selected = self.selected < workspaces.len();
        ACTIONS.map(|(id, label)| {
            let enabled = match id {
                Control::Open | Control::Rename => selected,
                Control::Recover => {
                    selected && snapshot.is_some_and(|snapshot| !snapshot.drafts.is_empty())
                }
                Control::Delete => selected && snapshot.is_some(),
                _ => true,
            };
            (id, label, enabled)
        })
    }

    fn submit_enabled(&self) -> bool {
        if let Some(confirm) = &self.confirming {
            confirm_delete_allows_enter(confirm, &self.input)
        } else {
            !self.input.trim().is_empty()
        }
    }

    /// Metadata updates preserve drawn geometry and cancel newly disabled presses.
    fn prepare(&mut self, workspaces: &[WorkspaceListing], snapshot: Option<&WorkspaceSnapshot>) {
        self.form.begin_update();
        let initial = if self.modal_open() {
            if self.editing.is_some()
                || self
                    .confirming
                    .as_ref()
                    .is_some_and(confirm_delete_requires_typed_name)
            {
                self.form
                    .declare_with_enabled(Control::Name, ControlKind::TextField, true);
            }
            self.form.declare_with_enabled(
                Control::Submit,
                ControlKind::Button,
                self.submit_enabled(),
            );
            self.form
                .declare_with_enabled(Control::Cancel, ControlKind::Button, true);
            Control::Name
        } else {
            self.form.declare_with_enabled(
                Control::Workspaces,
                ControlKind::ChoiceList {
                    len: workspaces.len() + 1,
                    selected: self.selected,
                },
                true,
            );
            for (id, _, enabled) in self.buttons(workspaces, snapshot) {
                self.form
                    .declare_with_enabled(id, ControlKind::Button, enabled);
            }
            Control::Workspaces
        };
        self.form.end_frame(initial);
    }

    pub(super) fn begin_frame(
        &mut self,
        workspaces: &[WorkspaceListing],
        snapshot: Option<&WorkspaceSnapshot>,
    ) {
        self.prepare(workspaces, snapshot);
        self.form.begin_frame();
    }

    pub(super) fn render_list(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        workspaces: &[WorkspaceListing],
    ) {
        let block = theme::panel(!self.modal_open()).title(" ✦ Workspaces ");
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let mut rows = workspaces
            .iter()
            .map(|candidate| {
                let attached = if candidate.attached_pids.is_empty() {
                    String::new()
                } else {
                    format!(
                        " [attached to {}]",
                        candidate
                            .attached_pids
                            .iter()
                            .map(u32::to_string)
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                };
                Line::from(vec![
                    Span::raw(candidate.workspace.name.clone()),
                    Span::styled(attached, theme::muted()),
                ])
            })
            .collect::<Vec<_>>();
        rows.push(Line::styled(
            "＋ Create new",
            Style::default().fg(theme::palette().accent),
        ));
        if self.modal_open() {
            frame.render_widget(Paragraph::new(rows), inner);
        } else {
            ChoiceList::render(
                frame,
                inner,
                &rows,
                self.selected,
                &mut self.form,
                Control::Workspaces,
            );
        }
    }

    pub(super) fn footer_height(width: u16) -> u16 {
        // Include a notice/shortcut row above the wrapping action buttons.
        action_rows(width).len() as u16 + 1
    }

    pub(super) fn render_footer(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        workspaces: &[WorkspaceListing],
        snapshot: Option<&WorkspaceSnapshot>,
        notices: &Notices,
    ) {
        let notice = notices.current();
        let style = if notice.is_some() {
            Style::default().fg(theme::palette().warning)
        } else {
            theme::muted()
        };
        let text = notice.unwrap_or_else(|| SELECTOR_HINTS.to_owned());
        frame.render_widget(
            Paragraph::new(text).style(style),
            Rect::new(area.x, area.y, area.width, u16::from(area.height > 0)),
        );
        if self.modal_open() {
            return;
        }
        let buttons = self.buttons(workspaces, snapshot);
        for (row, range) in action_rows(area.width).into_iter().enumerate() {
            if row + 1 >= usize::from(area.height) {
                break;
            }
            ButtonRow::render(
                frame,
                Rect::new(area.x, area.y + row as u16 + 1, area.width, 1),
                &buttons[range],
                &mut self.form,
            );
        }
    }

    pub(super) fn render_modal(&mut self, frame: &mut Frame, area: Rect) {
        if !self.modal_open() {
            self.form.end_frame(Control::Workspaces);
            return;
        }
        let (title, message, submit, field) = match (&self.editing, &self.confirming) {
            (Some(EditMode::Create), _) => (
                " New workspace ",
                "Workspace name".to_owned(),
                "Create",
                true,
            ),
            (Some(EditMode::Rename { .. }), _) => (
                " Rename workspace ",
                "Workspace name".to_owned(),
                "Save",
                true,
            ),
            (_, Some(confirm)) => {
                let typed = confirm_delete_requires_typed_name(confirm);
                let message = if typed {
                    format!(
                        "Delete {} and its {} active session(s) and {} draft(s)? Type the workspace name to confirm.",
                        confirm.name, confirm.active, confirm.drafts
                    )
                } else {
                    format!("Delete workspace {}?", confirm.name)
                };
                (" Delete workspace ", message, "Delete", typed)
            }
            _ => unreachable!("a workspace modal is open"),
        };
        let popup =
            mj_chat::components::dialog_rect(area, if area.width < 72 { 95 } else { 62 }, 9);
        frame.render_widget(Clear, popup);
        let block = theme::modal().title(title);
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        let rows = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(u16::from(field)),
            Constraint::Length(1),
        ])
        .split(inner);
        frame.render_widget(Paragraph::new(message).wrap(Wrap { trim: false }), rows[0]);
        if field {
            TextField::render(frame, rows[1], &self.input, &mut self.form, Control::Name);
        }
        let enabled = self.submit_enabled();
        ButtonRow::render(
            frame,
            rows[2],
            &[
                (Control::Submit, submit, enabled),
                (Control::Cancel, "Cancel", true),
            ],
            &mut self.form,
        );
        self.form.end_frame(if field {
            Control::Name
        } else {
            Control::Cancel
        });
    }

    pub(super) fn handle(
        &mut self,
        event: Event,
        workspaces: &[WorkspaceListing],
        snapshot: Option<&WorkspaceSnapshot>,
        notices: &Notices,
    ) -> Option<SelectorOutcome> {
        self.prepare(workspaces, snapshot);
        let mut shortcut = None;
        if matches!(&event, Event::Mouse(mouse) if matches!(mouse.kind, MouseEventKind::Down(_))) {
            notices.dismiss(std::time::Instant::now());
        }
        if let Event::Key(key) = &event {
            if key.kind == KeyEventKind::Release {
                return None;
            }
            notices.dismiss(std::time::Instant::now());
            if key.code == KeyCode::Esc
                || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
            {
                shortcut = Some(if self.modal_open() {
                    Control::Cancel
                } else {
                    Control::Back
                });
            } else if !self.modal_open()
                && !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
            {
                shortcut = match key.code {
                    KeyCode::Char('n' | 'N') => Some(Control::New),
                    KeyCode::Char('r' | 'R') => Some(Control::Rename),
                    KeyCode::Char('d' | 'D') => Some(Control::Delete),
                    KeyCode::Char('v' | 'V') => Some(Control::Recover),
                    KeyCode::Char('j') => {
                        self.selected = (self.selected + 1).min(workspaces.len());
                        None
                    }
                    KeyCode::Char('k') => {
                        self.selected = self.selected.saturating_sub(1);
                        None
                    }
                    _ => None,
                };
            }
        }
        let interaction = shortcut
            .map(Interaction::Activate)
            .or_else(|| self.form.handle(&event).action);
        let action = match interaction {
            Some(Interaction::Select(Control::Workspaces, index)) => {
                self.selected = index;
                if index == workspaces.len()
                    && matches!(event, Event::Mouse(mouse) if matches!(mouse.kind, MouseEventKind::Up(_)))
                {
                    Some(Control::New)
                } else {
                    None
                }
            }
            Some(Interaction::Edit(Control::Name, edit)) => {
                TextField::apply(&mut self.input, edit);
                None
            }
            Some(Interaction::Activate(Control::Workspaces)) => {
                Some(if self.selected < workspaces.len() {
                    Control::Open
                } else {
                    Control::New
                })
            }
            Some(Interaction::Activate(Control::Name)) => Some(Control::Submit),
            Some(Interaction::Activate(id)) => Some(id),
            Some(Interaction::Cancel) => Some(if self.modal_open() {
                Control::Cancel
            } else {
                Control::Back
            }),
            _ => None,
        };
        let result = action.and_then(|id| self.activate(id, workspaces, snapshot, notices));
        self.prepare(workspaces, snapshot);
        result
    }

    fn activate(
        &mut self,
        id: Control,
        workspaces: &[WorkspaceListing],
        snapshot: Option<&WorkspaceSnapshot>,
        notices: &Notices,
    ) -> Option<SelectorOutcome> {
        let candidate = workspaces.get(self.selected);
        match id {
            Control::Back => return Some(SelectorOutcome::Cancel),
            Control::Cancel => {
                self.editing = None;
                self.confirming = None;
                self.input.clear();
                self.form.focus(Control::Workspaces);
            }
            Control::Open => {
                return candidate
                    .map(|candidate| SelectorOutcome::Select(candidate.workspace.id.clone()));
            }
            Control::New => {
                self.editing = Some(EditMode::Create);
                self.input.set_value(&self.suggested_name);
                self.form.focus(Control::Name);
            }
            Control::Rename => {
                let candidate = candidate?;
                self.editing = Some(EditMode::Rename {
                    workspace_id: candidate.workspace.id.clone(),
                });
                self.input.set_value(&candidate.workspace.name);
                self.form.focus(Control::Name);
            }
            Control::Delete | Control::Recover => {
                let candidate = candidate?;
                let Some(snapshot) = snapshot else {
                    notices
                        .set("Workspace details are unavailable or still loading; retry shortly.");
                    return None;
                };
                if id == Control::Recover {
                    if let Some(draft) = snapshot.drafts.first() {
                        return Some(SelectorOutcome::RecoverDraft {
                            workspace_id: candidate.workspace.id.clone(),
                            draft_id: draft.id.clone(),
                        });
                    }
                    notices.set("This workspace has no recoverable drafts.");
                } else {
                    self.confirming = Some(ConfirmDelete {
                        workspace_id: candidate.workspace.id.clone(),
                        name: candidate.workspace.name.clone(),
                        active: snapshot
                            .sessions
                            .iter()
                            .filter(|session| session.active)
                            .count(),
                        drafts: snapshot.drafts.len(),
                    });
                    self.input.clear();
                    self.form.focus(Control::Name);
                }
            }
            Control::Submit if self.submit_enabled() => {
                if let Some(confirm) = &self.confirming {
                    return Some(if confirm_delete_requires_typed_name(confirm) {
                        SelectorOutcome::ForceDelete(confirm.workspace_id.clone())
                    } else {
                        SelectorOutcome::Delete(confirm.workspace_id.clone())
                    });
                }
                return self.editing.as_ref().map(|edit| match edit {
                    EditMode::Create => SelectorOutcome::Create(self.input.value().to_owned()),
                    EditMode::Rename { workspace_id } => SelectorOutcome::Rename {
                        workspace_id: workspace_id.clone(),
                        name: self.input.value().to_owned(),
                    },
                });
            }
            _ => {}
        }
        None
    }
}

fn action_rows(width: u16) -> Vec<std::ops::Range<usize>> {
    let widths = ACTIONS.map(|(_, label)| Line::raw(label).width() as u16 + 4);
    let mut rows = Vec::new();
    let mut start = 0;
    let mut used = 0;
    for (index, button) in widths.into_iter().enumerate() {
        let needed = button + u16::from(index > start);
        if used + needed > width && index > start {
            rows.push(start..index);
            start = index;
            used = button;
        } else {
            used += needed;
        }
    }
    rows.push(start..widths.len());
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::DraftPreview;
    use crossterm::event::{KeyEvent, MouseButton, MouseEvent};
    use hel::hel_workspace::WorkspaceRecord;
    use ratatui::{Terminal, backend::TestBackend};

    fn candidate(name: &str) -> WorkspaceListing {
        WorkspaceListing {
            workspace: WorkspaceRecord {
                id: name.into(),
                name: name.into(),
                created_at: "2026-09-08T00:00:00Z".into(),
                last_opened_at: "2026-09-08T00:00:00Z".into(),
                session_count: 0,
            },
            attached_pids: vec![],
        }
    }

    fn snapshot(candidate: &WorkspaceListing, drafts: bool) -> WorkspaceSnapshot {
        WorkspaceSnapshot {
            workspace: candidate.workspace.clone(),
            sessions: vec![],
            drafts: if drafts {
                vec![DraftPreview {
                    id: "draft-1".into(),
                    session_id: None,
                    source: "test".into(),
                    owner_pid: None,
                    saved_at: "2026-09-08T00:00:00Z".into(),
                }]
            } else {
                vec![]
            },
        }
    }

    struct Harness {
        controls: WorkspaceControls,
        workspaces: Vec<WorkspaceListing>,
        snapshot: Option<WorkspaceSnapshot>,
        notices: Notices,
        size: (u16, u16),
    }

    impl Harness {
        fn new(size: (u16, u16)) -> Self {
            let workspaces = vec![candidate("Bifrost"), candidate("Asgard")];
            let snapshot = Some(snapshot(&workspaces[0], false));
            Self {
                controls: WorkspaceControls::new(0, "New project"),
                workspaces,
                snapshot,
                notices: Notices::default(),
                size,
            }
        }

        fn draw(&mut self) -> Vec<String> {
            let mut terminal = Terminal::new(TestBackend::new(self.size.0, self.size.1)).unwrap();
            terminal
                .draw(|frame| {
                    self.controls
                        .begin_frame(&self.workspaces, self.snapshot.as_ref());
                    let footer_height = WorkspaceControls::footer_height(frame.area().width);
                    let [body, footer] =
                        Layout::vertical([Constraint::Min(1), Constraint::Length(footer_height)])
                            .areas(frame.area());
                    self.controls.render_list(frame, body, &self.workspaces);
                    self.controls.render_footer(
                        frame,
                        footer,
                        &self.workspaces,
                        self.snapshot.as_ref(),
                        &self.notices,
                    );
                    self.controls.render_modal(frame, frame.area());
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            (0..self.size.1)
                .map(|y| (0..self.size.0).map(|x| buffer[(x, y)].symbol()).collect())
                .collect()
        }

        fn point(&mut self, text: &str) -> (u16, u16) {
            let lines = self.draw();
            let (row, line) = lines
                .iter()
                .enumerate()
                .find(|(_, line)| line.contains(text))
                .unwrap_or_else(|| panic!("missing {text:?}: {lines:#?}"));
            (
                line[..line.find(text).unwrap()].chars().count() as u16,
                row as u16,
            )
        }

        fn event(&mut self, event: Event) -> Option<SelectorOutcome> {
            self.controls.handle(
                event,
                &self.workspaces,
                self.snapshot.as_ref(),
                &self.notices,
            )
        }

        fn mouse(
            &mut self,
            kind: MouseEventKind,
            (column, row): (u16, u16),
        ) -> Option<SelectorOutcome> {
            self.event(Event::Mouse(MouseEvent {
                kind,
                column,
                row,
                modifiers: KeyModifiers::NONE,
            }))
        }

        fn click(&mut self, label: &str) -> Option<SelectorOutcome> {
            let point = self.point(label);
            assert!(
                self.mouse(MouseEventKind::Down(MouseButton::Left), point)
                    .is_none()
            );
            self.draw();
            self.mouse(MouseEventKind::Up(MouseButton::Left), point)
        }

        fn replace_name(&mut self, name: &str) {
            self.event(Event::Key(KeyEvent::new(
                KeyCode::Char('a'),
                KeyModifiers::CONTROL,
            )));
            self.event(Event::Key(KeyEvent::new(
                KeyCode::Char('k'),
                KeyModifiers::CONTROL,
            )));
            self.event(Event::Paste(name.into()));
        }
    }

    #[test]
    fn mouse_can_preview_open_create_rename_and_leave_workspaces() {
        for size in [(32, 10), (40, 10), (72, 18), (140, 40)] {
            let mut h = Harness::new(size);
            assert!(h.click("Asgard").is_none());
            assert_eq!(h.controls.selected, 1);
            assert!(
                matches!(h.click("  Open  "), Some(SelectorOutcome::Select(id)) if id == "Asgard")
            );
            assert!(h.click("Create new").is_none());
            assert!(h.controls.modal_open());
            h.replace_name("Mý workspace");
            assert!(
                matches!(h.click("  Create  "), Some(SelectorOutcome::Create(name)) if name == "Mý workspace")
            );
            h.click("  Cancel  ");
            h.click("Bifrost");
            h.click("  Rename  ");
            h.replace_name("Renamed");
            assert!(
                matches!(h.click("  Save  "), Some(SelectorOutcome::Rename { workspace_id, name }) if workspace_id == "Bifrost" && name == "Renamed")
            );
            h.click("  Cancel  ");
            assert!(matches!(h.click("  Back  "), Some(SelectorOutcome::Cancel)));
        }
    }

    #[test]
    fn workspace_delete_and_recovery_use_ready_metadata_and_existing_confirmation() {
        let mut h = Harness::new((72, 18));
        h.snapshot = None;
        h.click("  Delete  ");
        assert!(!h.controls.modal_open());
        h.snapshot = Some(snapshot(&h.workspaces[0], true));
        assert!(
            matches!(h.click("  Recover  "), Some(SelectorOutcome::RecoverDraft { workspace_id, draft_id }) if workspace_id == "Bifrost" && draft_id == "draft-1")
        );
        h.click("  Delete  ");
        assert!(h.draw().join("\n").contains("Type the workspace name"));
        h.replace_name("Wrong name");
        assert!(h.click("  Delete  ").is_none());
        assert!(h.controls.modal_open());
        h.replace_name("Bifrost");
        assert!(
            matches!(h.click("  Delete  "), Some(SelectorOutcome::ForceDelete(id)) if id == "Bifrost")
        );
        h.click("  Cancel  ");
        h.snapshot = Some(snapshot(&h.workspaces[0], false));
        h.click("  Delete  ");
        assert!(
            matches!(h.click("  Delete  "), Some(SelectorOutcome::Delete(id)) if id == "Bifrost")
        );
    }

    #[test]
    fn workspace_pointer_capture_survives_redraw_and_cancels_outside_or_when_disabled() {
        let mut h = Harness::new((72, 18));
        let new = h.point("  New  ");
        h.mouse(MouseEventKind::Down(MouseButton::Left), new);
        h.draw();
        assert!(
            h.mouse(MouseEventKind::Up(MouseButton::Left), (71, 0))
                .is_none()
        );
        assert!(!h.controls.modal_open());
        let delete = h.point("  Delete  ");
        h.mouse(MouseEventKind::Down(MouseButton::Left), delete);
        h.snapshot = None;
        h.draw();
        h.mouse(MouseEventKind::Up(MouseButton::Left), delete);
        assert!(!h.controls.modal_open());
        h.click("  New  ");
        let before = h.controls.input.value().to_owned();
        h.mouse(MouseEventKind::Down(MouseButton::Left), new);
        h.mouse(MouseEventKind::Up(MouseButton::Left), new);
        assert_eq!(h.controls.input.value(), before);
        h.size = (40, 10);
        h.click("  Cancel  ");
        assert!(!h.controls.modal_open());
    }

    #[test]
    fn workspace_failures_remain_visible_above_clickable_actions() {
        let mut h = Harness::new((100, 18));
        h.notices.set_failure("Could not delete workspace: busy");
        let lines = h.draw();
        assert!(
            lines
                .iter()
                .any(|line| line.contains("Could not delete workspace: busy"))
        );
        assert!(lines.iter().any(|line| line.contains("  New  ")));
        assert!(!h.notices.dismiss(std::time::Instant::now()));
    }
}
