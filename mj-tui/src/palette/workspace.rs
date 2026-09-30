//! Change Workspace, as a combobox anchored on the palette row that opened it.
//!
//! The palette stays open underneath. The popup is the same
//! [`ComboBox`] the wizard's Subagents, Model and Effort fields use: the
//! item's own text stays where it is, the dropdown glyph replaces its
//! submenu glyph, and the list of workspaces opens below it. `Enter` picks,
//! and `Esc` closes only the list.

use crossterm::event::Event;
use mj_chat::components::{ComboBox, ComboBoxState, ControlKind, Dialog, Interaction, PopupSide};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Clear;

use super::{CommandPalette, PaletteControl};
use crate::actions::CommandId;
use crate::{DashboardAction, DashboardState, Mode};

/// The workspace list open on the "Change workspace" row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct WorkspacePopup {
    session_id: String,
    /// Workspace ids and names, in the workspace manager's order.
    choices: Vec<(String, String)>,
    /// Index into `choices` of the session's current workspace.
    current: usize,
    combo: ComboBoxState<PaletteControl>,
}

impl WorkspacePopup {
    fn selected(&self) -> usize {
        self.combo
            .selection(PaletteControl::Workspace, self.current)
    }

    pub(super) fn kind(&self) -> ControlKind {
        ControlKind::ComboBox {
            len: self.choices.len(),
            selected: self.selected(),
            expanded: self.combo.is_open(PaletteControl::Workspace),
        }
    }

    /// The list's rows: names only, the current workspace marked.
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

    /// Draws the anchored list on `row`, the palette row of the command.
    /// `bounds` is what the popup may cover.
    pub(super) fn render(
        &self,
        frame: &mut Frame,
        bounds: Rect,
        row: Rect,
        session_menu: bool,
        form: &mut Dialog<PaletteControl>,
    ) {
        // The row's own words, so the field reads as the item opened. The
        // menu spells it in title case, the palette in sentence case.
        let label = if session_menu {
            "Change Workspace"
        } else {
            "Change workspace"
        };
        let field_width =
            u16::try_from(Line::raw(label).width() + 1 + Line::raw(ComboBox::glyph()).width())
                .unwrap_or(u16::MAX)
                .min(row.width);
        let field = Rect::new(row.x, row.y, field_width, 1);
        frame.render_widget(Clear, field);
        ComboBox::render(
            frame,
            bounds,
            field,
            label,
            &self.options(),
            self.selected(),
            self.combo.is_open(PaletteControl::Workspace),
            true,
            " workspace · ↑/↓ select · Enter choose ",
            PopupSide::Below,
            form,
            PaletteControl::Workspace,
        );
    }
}

impl DashboardState {
    /// Opens Change Workspace from a keybinding or another command, which
    /// has no palette open: the session menu appears with the list already
    /// open on its row.
    pub(crate) fn begin_change_workspace(&mut self) {
        if !matches!(self.mode, Mode::Palette(_)) {
            self.begin_session_palette();
            if let Mode::Palette(palette) = &mut self.mode
                && let Some(index) = palette
                    .entries
                    .iter()
                    .position(|entry| entry.id == CommandId::ChangeWorkspace)
            {
                palette.selected = index;
                palette.prepare();
            }
        }
        self.open_workspace_popup();
    }

    /// Opens the workspace list on the open palette's Change workspace row,
    /// on the session's current workspace.
    pub(super) fn open_workspace_popup(&mut self) {
        let Mode::Palette(palette) = &self.mode else {
            return;
        };
        let session = palette
            .session_id
            .as_deref()
            .or_else(|| self.command_session_id())
            .and_then(|id| self.state.sessions.get(id));
        let Some(session) = session else {
            self.cancel_modal();
            self.set_notice("This session is no longer available.");
            return;
        };
        let session_id = session.id.clone();
        let workspace_id = session.workspace_id.clone();
        let choices = self
            .workspace_ids()
            .into_iter()
            .map(|id| {
                let name = self.workspace_display_name(&id).to_owned();
                (id, name)
            })
            .collect::<Vec<_>>();
        if choices.len() < 2 {
            self.cancel_modal();
            self.set_notice("There is no other workspace to move this session to.");
            return;
        }
        let current = choices
            .iter()
            .position(|(id, _)| *id == workspace_id)
            .unwrap_or(0);
        let mut combo = ComboBoxState::default();
        combo.open(PaletteControl::Workspace, current);
        let Mode::Palette(palette) = &mut self.mode else {
            return;
        };
        palette.workspace_popup = Some(WorkspacePopup {
            session_id,
            choices,
            current,
            combo,
        });
        palette.prepare();
        palette.form.get_mut().focus(PaletteControl::Workspace);
    }

    /// Keys and clicks while the list is open belong to it. `Esc` closes the
    /// list and leaves the menu.
    pub(super) fn handle_workspace_popup_event(&mut self, event: Event) -> DashboardAction {
        let Mode::Palette(palette) = &mut self.mode else {
            return DashboardAction::None;
        };
        let Some(popup) = palette.workspace_popup.as_mut() else {
            return DashboardAction::None;
        };
        let result = palette.form.get_mut().handle(&event);
        self.last_event_consumed.set(result.consumed);
        let routed = popup.combo.route(result.action);
        if let Some(Interaction::ComboBoxCommit(_, index)) = routed {
            return self.commit_workspace_choice(index);
        }
        if matches!(routed, Some(Interaction::Cancel)) {
            self.cancel_modal();
            return DashboardAction::None;
        }
        if popup.combo.is_open(PaletteControl::Workspace) {
            palette.prepare();
        } else {
            close_popup(palette);
        }
        DashboardAction::None
    }

    /// Moves the session at once (it is reversible); picking its own
    /// workspace only says so.
    fn commit_workspace_choice(&mut self, index: usize) -> DashboardAction {
        let Mode::Palette(palette) = &mut self.mode else {
            return DashboardAction::None;
        };
        let Some(popup) = palette.workspace_popup.take() else {
            return DashboardAction::None;
        };
        let Some((workspace_id, workspace_name)) = popup.choices.get(index).cloned() else {
            close_popup(palette);
            return DashboardAction::None;
        };
        self.cancel_modal();
        let Some(session) = self.state.sessions.get(&popup.session_id) else {
            self.set_notice("This session is no longer available.");
            return DashboardAction::None;
        };
        let name = crate::render::session_name(session).to_owned();
        if index == popup.current {
            self.set_notice(format!(
                "Session \"{}\" is already in workspace \"{}\".",
                crate::fit_session_name(&name, crate::NOTICE_NAME_CELLS),
                crate::fit_session_name(&workspace_name, crate::NOTICE_NAME_CELLS),
            ));
            return DashboardAction::None;
        }
        DashboardAction::ChangeWorkspace {
            session_id: popup.session_id,
            workspace_id,
            workspace_name,
        }
    }
}

/// Closes the list and returns the focus to the menu behind it.
fn close_popup(palette: &mut CommandPalette) {
    palette.workspace_popup = None;
    palette.prepare();
    palette.form.get_mut().focus(if palette.session_only {
        PaletteControl::Commands
    } else {
        PaletteControl::Query
    });
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crossterm::event::KeyCode;

    use super::*;
    use crate::test_support::{dashboard_with_session, drawn, key, open_palette, running_session};
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

    fn selected_command(dashboard: &DashboardState) -> Option<CommandId> {
        match &dashboard.mode {
            Mode::Palette(palette) => palette.entries.get(palette.selected).map(|e| e.id),
            _ => None,
        }
    }

    fn row(lines: &[String], needle: &str) -> usize {
        lines
            .iter()
            .position(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("{needle:?} is drawn in {lines:#?}"))
    }

    /// Opens the session menu and moves to its Change Workspace item.
    fn menu_on_change_workspace(dashboard: &mut DashboardState, width: u16, height: u16) {
        dashboard.begin_session_palette();
        drawn(dashboard, width, height);
        while selected_command(dashboard) != Some(CommandId::ChangeWorkspace) {
            dashboard.handle_key(key(KeyCode::Down));
            drawn(dashboard, width, height);
        }
    }

    #[test]
    fn the_session_menu_lists_change_workspace_under_organize() {
        let mut dashboard = dashboard_with_workspaces();
        dashboard.begin_session_palette();
        let lines = drawn(&mut dashboard, 120, 40);
        let (organize, rename, change, lifecycle) = (
            row(&lines, "Organize"),
            row(&lines, "Rename…"),
            row(&lines, "Change Workspace ›"),
            row(&lines, "Lifecycle"),
        );
        assert!(
            organize < rename && rename < change && change < lifecycle,
            "{lines:#?}"
        );
    }

    fn assert_popup_on_menu_item(width: u16, height: u16) {
        let mut dashboard = dashboard_with_workspaces();
        menu_on_change_workspace(&mut dashboard, width, height);
        let before = drawn(&mut dashboard, width, height);
        let item_row = row(&before, "Change Workspace ›");
        let item_column = before[item_row].find("Change Workspace").unwrap();

        dashboard.handle_key(key(KeyCode::Enter));
        // The menu stays: the list is a popup on the item, not a dialog.
        assert!(
            matches!(&dashboard.mode, Mode::Palette(p) if p.workspace_popup.is_some()),
            "the list opens inside the menu"
        );
        let lines = drawn(&mut dashboard, width, height);
        let joined = lines.join("\n");
        assert_eq!(row(&lines, "Change Workspace ▾"), item_row, "{joined}");
        assert_eq!(
            lines[item_row].find("Change Workspace"),
            Some(item_column),
            "the item stays where it was: {joined}"
        );
        // Names only, in the workspace manager's order, the current one
        // marked, directly under the item.
        let (default, other, third) = (
            row(&lines, "Default (current)"),
            row(&lines, "│Other"),
            row(&lines, "│Third"),
        );
        assert!(
            item_row < default && default < other && other < third,
            "{joined}"
        );
        assert!(!joined.contains("Session: "), "no second dialog: {joined}");
    }

    #[test]
    fn the_menu_item_opens_the_inline_combobox_at_140_columns() {
        assert_popup_on_menu_item(140, 40);
    }

    #[test]
    fn the_menu_item_opens_the_inline_combobox_at_80_columns() {
        assert_popup_on_menu_item(80, 40);
    }

    #[test]
    fn the_palette_opens_the_same_combobox_on_its_row() {
        let mut dashboard = dashboard_with_workspaces();
        open_palette(&mut dashboard);
        drawn(&mut dashboard, 100, 40);
        for character in "change workspace".chars() {
            dashboard.handle_key(key(KeyCode::Char(character)));
        }
        drawn(&mut dashboard, 100, 40);
        assert_eq!(
            selected_command(&dashboard),
            Some(CommandId::ChangeWorkspace)
        );
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::None
        );
        assert!(
            matches!(&dashboard.mode, Mode::Palette(p) if p.workspace_popup.is_some()),
            "the palette stays open under the list"
        );
        let lines = drawn(&mut dashboard, 100, 40);
        let anchor = row(&lines, "Change workspace ▾");
        assert!(
            anchor < row(&lines, "Default (current)")
                && row(&lines, "Default (current)") < row(&lines, "│Other"),
            "{lines:#?}"
        );
    }

    #[test]
    fn choosing_a_workspace_moves_at_once() {
        let mut dashboard = dashboard_with_workspaces();
        dashboard.dispatch_command(CommandId::ChangeWorkspace);
        drawn(&mut dashboard, 120, 40);
        dashboard.handle_key(key(KeyCode::Down));
        drawn(&mut dashboard, 120, 40);
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::ChangeWorkspace {
                session_id: "session-1".into(),
                workspace_id: "other".into(),
                workspace_name: "Other".into(),
            }
        );
        assert!(matches!(dashboard.mode, Mode::Dashboard));
        let joined = drawn(&mut dashboard, 120, 40).join("\n");
        assert!(!joined.contains("Change workspace?"), "{joined}");
    }

    /// A session titled by the prompt it started with: a long handoff, over
    /// many lines.
    fn dashboard_with_handoff_title() -> DashboardState {
        let mut session = running_session();
        session.session_title_override = None;
        session.acp_session_title = None;
        session.title = "Continue the handoff below and finish the migration.\n\n"
            .repeat(2000 / 50 + 1)
            .chars()
            .take(2000)
            .collect();
        assert_eq!(session.title.chars().count(), 2000);
        let mut dashboard = dashboard_with_session(session);
        dashboard.set_workspace_names(BTreeMap::from([
            ("default".to_owned(), "Default".to_owned()),
            ("other".to_owned(), "Other".to_owned()),
        ]));
        dashboard.order_workspaces(&["default".into(), "other".into()]);
        dashboard.focus_sessions();
        dashboard
    }

    #[test]
    fn other_confirmations_truncate_a_long_title_the_same_way() {
        let mut dashboard = dashboard_with_handoff_title();
        dashboard.dispatch_command(CommandId::DestroySession);
        assert!(matches!(dashboard.mode, Mode::Confirm(_)));
        let lines = drawn(&mut dashboard, 80, 40);
        let at = row(&lines, "Session: Continue the handoff");
        assert!(
            lines[at].contains(mj_chat::theme::glyphs().ellipsis),
            "{}",
            lines[at]
        );
        assert!(!lines[at + 1].contains("Continue"), "{lines:#?}");
    }

    #[test]
    fn escape_closes_only_the_list_then_the_menu() {
        let mut dashboard = dashboard_with_workspaces();
        dashboard.dispatch_command(CommandId::ChangeWorkspace);
        drawn(&mut dashboard, 120, 40);
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Esc)),
            DashboardAction::None
        );
        assert!(
            matches!(&dashboard.mode, Mode::Palette(p) if p.workspace_popup.is_none()),
            "the menu is still open"
        );
        let joined = drawn(&mut dashboard, 120, 40).join("\n");
        assert!(joined.contains("Change Workspace ›"), "{joined}");
        dashboard.handle_key(key(KeyCode::Esc));
        assert!(matches!(dashboard.mode, Mode::Dashboard));
    }

    #[test]
    fn picking_the_current_workspace_only_says_so() {
        let mut dashboard = dashboard_with_workspaces();
        dashboard.dispatch_command(CommandId::ChangeWorkspace);
        drawn(&mut dashboard, 120, 40);
        dashboard.handle_key(key(KeyCode::Enter));
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
