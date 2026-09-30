//! Change Workspace: pick a workspace for one session from the shared
//! combobox, then confirm the move.
use std::cell::RefCell;

use crossterm::event::Event;
use mj_chat::components::{ComboBox, ComboBoxState, ControlKind, Dialog, Interaction, PopupSide};

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkspacePickerControl {
    Workspace,
    Cancel,
}

/// The dialog behind the session menu's "Change Workspace ›".
///
/// It opens with the combobox already expanded on the session's current
/// workspace, so one Enter or click chooses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkspacePicker {
    pub(crate) session_id: String,
    /// The session's listed title, which the dialogs name it by.
    pub(crate) session_name: String,
    /// Workspace ids and names, in the workspace manager's order.
    pub(crate) choices: Vec<(String, String)>,
    /// Index into `choices` of the session's current workspace.
    pub(crate) current: usize,
    pub(crate) combo: ComboBoxState<WorkspacePickerControl>,
    pub(crate) form: RefCell<Dialog<WorkspacePickerControl>>,
}

impl WorkspacePicker {
    fn selected(&self) -> usize {
        self.combo
            .selection(WorkspacePickerControl::Workspace, self.current)
    }

    /// The combobox's rows: names only, the current workspace marked.
    fn options(&self) -> Vec<Line<'static>> {
        self.choices
            .iter()
            .enumerate()
            .map(|(index, (_, name))| {
                if index == self.current {
                    Line::raw(format!("{name} (current)"))
                } else {
                    Line::raw(name.clone())
                }
            })
            .collect()
    }

    /// Declares the controls before an event or a frame, with the popup's
    /// state, so the form reads keys against the right kind.
    pub(crate) fn prepare_dialog_state(&mut self) {
        let kind = ControlKind::ComboBox {
            len: self.choices.len(),
            selected: self.selected(),
            expanded: self.combo.is_open(WorkspacePickerControl::Workspace),
        };
        let form = self.form.get_mut();
        form.set_dismiss_actions(&[WorkspacePickerControl::Cancel]);
        form.begin_frame();
        form.register(
            WorkspacePickerControl::Workspace,
            kind,
            Rect::default(),
            true,
        );
        form.register(
            WorkspacePickerControl::Cancel,
            ControlKind::Button,
            Rect::default(),
            true,
        );
        form.end_frame(WorkspacePickerControl::Workspace);
    }
}

pub(crate) fn render_workspace_picker(
    frame: &mut Frame,
    area: Rect,
    picker: &WorkspacePicker,
    surfaces: &mut FrameSurfaces,
) {
    // Tall enough for the open list to sit above the Cancel button.
    let height = u16::try_from(picker.choices.len().saturating_add(9).min(24)).unwrap_or(24);
    let popup = centered_modal(frame, surfaces, 60, height, area);
    let inner = popup.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 1,
    });
    if inner.height == 0 {
        let mut form = picker.form.borrow_mut();
        form.cancel_pointer();
        form.reset_geometry();
        return;
    }
    frame.render_widget(
        Paragraph::new(format!("Session: {}", picker.session_name)),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    let label = "Workspace: ";
    let label_width = u16::try_from(label.len())
        .unwrap_or(u16::MAX)
        .min(inner.width.saturating_sub(1));
    let label_area = Rect::new(inner.x, inner.y.saturating_add(2), label_width, 1);
    let field = Rect::new(
        inner.x.saturating_add(label_width),
        inner.y.saturating_add(2),
        inner.width.saturating_sub(label_width),
        1,
    );
    let footer = Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1);
    let mut form = picker.form.borrow_mut();
    form.begin_frame();
    let title = dismissible_modal_title(
        &mut form,
        popup,
        "Change workspace",
        theme::title(true),
        true,
    );
    frame.render_widget(theme::modal().title(title), popup);
    frame.render_widget(Paragraph::new(label), label_area);
    Dialog::render_actions(
        frame,
        footer,
        &[(WorkspacePickerControl::Cancel, "Cancel", true)],
        &mut form,
    );
    // Drawn last so the anchored list lies over the rows below the field.
    let selected = picker.selected();
    let value = picker
        .choices
        .get(selected)
        .map(|(_, name)| name.as_str())
        .unwrap_or_default();
    ComboBox::render(
        frame,
        area,
        field,
        value,
        &picker.options(),
        selected,
        picker.combo.is_open(WorkspacePickerControl::Workspace),
        true,
        " workspace · ↑/↓ select · Enter choose ",
        PopupSide::Below,
        &mut form,
        WorkspacePickerControl::Workspace,
    );
    form.end_frame(WorkspacePickerControl::Workspace);
}

impl DashboardState {
    pub(crate) fn begin_change_workspace(&mut self) {
        let Some(session) = self.command_session() else {
            return;
        };
        let session_id = session.id.clone();
        let session_name = session.listed_title().to_owned();
        let choices = self
            .workspace_ids()
            .into_iter()
            .map(|id| {
                let name = self.workspace_display_name(&id).to_owned();
                (id, name)
            })
            .collect::<Vec<_>>();
        if choices.len() < 2 {
            self.set_notice("There is no other workspace to move this session to.");
            return;
        }
        let current = choices
            .iter()
            .position(|(id, _)| *id == session.workspace_id)
            .unwrap_or(0);
        let mut combo = ComboBoxState::default();
        combo.open(WorkspacePickerControl::Workspace, current);
        let mut picker = WorkspacePicker {
            session_id,
            session_name,
            choices,
            current,
            combo,
            form: RefCell::new(Dialog::default()),
        };
        picker.prepare_dialog_state();
        self.mode = Mode::ChangeWorkspace(picker);
    }

    pub(crate) fn handle_change_workspace_event(
        &mut self,
        event: Event,
        mut picker: WorkspacePicker,
    ) -> DashboardAction {
        use WorkspacePickerControl::*;
        let result = picker.form.get_mut().handle(&event);
        self.last_event_consumed.set(result.consumed);
        match picker.combo.route(result.action) {
            Some(Interaction::Cancel | Interaction::Activate(Cancel)) => {
                self.cancel_modal();
                return DashboardAction::None;
            }
            // Esc on the open list leaves the whole dialog: the list is the
            // dialog's only content.
            Some(Interaction::ComboBoxDismiss(_)) => {
                self.cancel_modal();
                return DashboardAction::None;
            }
            Some(Interaction::Activate(Workspace)) => {
                picker.combo.open(Workspace, picker.current);
            }
            Some(Interaction::ComboBoxCommit(_, index)) => {
                let Some((workspace_id, workspace_name)) = picker.choices.get(index).cloned()
                else {
                    self.cancel_modal();
                    return DashboardAction::None;
                };
                if index == picker.current {
                    self.cancel_modal();
                    self.set_notice(format!(
                        "Session \"{}\" is already in workspace \"{workspace_name}\".",
                        picker.session_name
                    ));
                    return DashboardAction::None;
                }
                self.mode = Mode::Confirm(
                    ConfirmDialog::new(Confirmation::ChangeWorkspace {
                        session_id: picker.session_id,
                        workspace_id,
                        workspace_name,
                    })
                    .naming_session(&picker.session_name),
                );
                return DashboardAction::None;
            }
            _ => {}
        }
        picker.prepare_dialog_state();
        self.mode = Mode::ChangeWorkspace(picker);
        DashboardAction::None
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crossterm::event::KeyCode;

    use super::*;
    use crate::test_support::{dashboard_with_session, drawn, key, running_session};
    use crate::{CommandId, DashboardAction};

    /// A dashboard with one running session in "Default" and two other
    /// workspaces, focused on its row.
    fn dashboard_with_workspaces() -> DashboardState {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.set_workspace_names(BTreeMap::from([
            ("default".to_owned(), "Default".to_owned()),
            ("other".to_owned(), "Other".to_owned()),
            ("third".to_owned(), "Third".to_owned()),
        ]));
        dashboard.order_workspaces(&["default".into(), "other".into(), "third".into()]);
        dashboard.focus_sessions();
        dashboard
    }

    #[test]
    fn the_session_menu_lists_change_workspace_under_organize() {
        let mut dashboard = dashboard_with_workspaces();
        dashboard.begin_session_palette();
        let lines = drawn(&mut dashboard, 120, 40);
        let row = |needle: &str| {
            lines
                .iter()
                .position(|line| line.contains(needle))
                .unwrap_or_else(|| panic!("{needle:?} is drawn in {lines:#?}"))
        };
        let (organize, rename, change, lifecycle) = (
            row("Organize"),
            row("Rename…"),
            row("Change Workspace ›"),
            row("Lifecycle"),
        );
        assert!(
            organize < rename && rename < change && change < lifecycle,
            "{lines:#?}"
        );
    }

    #[test]
    fn the_menu_item_opens_the_shared_combobox_with_the_current_workspace_marked() {
        let mut dashboard = dashboard_with_workspaces();
        dashboard.begin_session_palette();
        drawn(&mut dashboard, 120, 40);
        while !matches!(
            &dashboard.mode,
            Mode::Palette(palette) if palette.entries[palette.selected].id == CommandId::ChangeWorkspace
        ) {
            dashboard.handle_key(key(KeyCode::Down));
            drawn(&mut dashboard, 120, 40);
        }
        dashboard.handle_key(key(KeyCode::Enter));
        assert!(matches!(dashboard.mode, Mode::ChangeWorkspace(_)));

        let lines = drawn(&mut dashboard, 120, 40);
        let joined = lines.join("\n");
        assert!(joined.contains("Change workspace"), "{joined}");
        assert!(joined.contains("Session: ACP pretty name"), "{joined}");
        // The field carries the combobox glyph; the list is names only, in
        // the workspace manager's order, with the current one marked.
        assert!(joined.contains("Default ▾"), "{joined}");
        let row = |needle: &str| {
            lines
                .iter()
                .position(|line| line.contains(needle))
                .unwrap_or_else(|| panic!("{needle:?} is listed in {joined}"))
        };
        let (default, other, third) = (row("│Default (current)"), row("│Other"), row("│Third"));
        assert!(default < other && other < third, "{joined}");
        assert!(
            row("Cancel") > third,
            "the list sits above Cancel: {joined}"
        );
    }

    #[test]
    fn choosing_a_workspace_asks_first_and_enter_moves() {
        let mut dashboard = dashboard_with_workspaces();
        dashboard.dispatch_command(CommandId::ChangeWorkspace);
        drawn(&mut dashboard, 120, 40);
        dashboard.handle_key(key(KeyCode::Down));
        drawn(&mut dashboard, 120, 40);
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::None
        );
        let Mode::Confirm(dialog) = &dashboard.mode else {
            panic!("a chosen workspace is confirmed first");
        };
        assert_eq!(
            dialog.confirmation,
            Confirmation::ChangeWorkspace {
                session_id: "session-1".into(),
                workspace_id: "other".into(),
                workspace_name: "Other".into(),
            }
        );
        let joined = drawn(&mut dashboard, 120, 40).join("\n");
        assert!(
            joined.contains("Move session \"ACP pretty name\" to workspace \"Other\"?"),
            "{joined}"
        );
        assert!(
            joined.contains("Cancel") && joined.contains("Move"),
            "{joined}"
        );

        // Move is focused, so Enter confirms.
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::ChangeWorkspace {
                session_id: "session-1".into(),
                workspace_id: "other".into(),
                workspace_name: "Other".into(),
            }
        );
        assert!(matches!(dashboard.mode, Mode::Dashboard));
    }

    #[test]
    fn escape_cancels_the_confirmation_and_the_session_stays() {
        let mut dashboard = dashboard_with_workspaces();
        dashboard.dispatch_command(CommandId::ChangeWorkspace);
        drawn(&mut dashboard, 120, 40);
        dashboard.handle_key(key(KeyCode::Down));
        drawn(&mut dashboard, 120, 40);
        dashboard.handle_key(key(KeyCode::Enter));
        assert!(matches!(dashboard.mode, Mode::Confirm(_)));
        drawn(&mut dashboard, 120, 40);
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Esc)),
            DashboardAction::None
        );
        assert!(matches!(dashboard.mode, Mode::Dashboard));
        assert_eq!(
            dashboard.state.sessions["session-1"].workspace_id,
            "default"
        );
    }

    #[test]
    fn escape_leaves_the_combobox_without_a_confirmation() {
        let mut dashboard = dashboard_with_workspaces();
        dashboard.dispatch_command(CommandId::ChangeWorkspace);
        drawn(&mut dashboard, 120, 40);
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Esc)),
            DashboardAction::None
        );
        assert!(matches!(dashboard.mode, Mode::Dashboard));
    }

    #[test]
    fn the_command_waits_for_a_second_workspace() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.set_workspace_names(BTreeMap::from([(
            "default".to_owned(),
            "Default".to_owned(),
        )]));
        dashboard.focus_sessions();
        assert_eq!(
            (crate::actions::spec(CommandId::ChangeWorkspace).available)(&dashboard),
            crate::actions::Availability::Blocked("there is no other workspace")
        );
    }
}
