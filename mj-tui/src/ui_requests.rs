//! Applying dashboard requests an agent made on the person's behalf (`mj ui`).
//!
//! A request is input, like a keypress: it goes through the same methods and
//! [`DashboardAction`]s the person's own keys use, so it can only do what they
//! could. One that would cover or replace an open dialog waits until the
//! dialog closes, rather than discarding what the person was doing.

use super::*;
use mj_client::ui_request::{SessionPlacement, UiAction, UiPane, UiRequest};
use mj_core::workspace::PaneSize;

/// How many requests may wait behind an open dialog. Older ones are dropped
/// first: the newest is what the person asked for last.
const HELD_UI_REQUESTS: usize = 8;

impl DashboardState {
    /// Apply one request now, or hold it behind the open dialog and the
    /// requests already waiting, so requests always apply in the order made.
    pub fn apply_ui_request(&mut self, request: UiRequest) -> DashboardAction {
        // A pane size touches nothing the person is working in, so it never
        // waits.
        let waits = !matches!(request.action, UiAction::PaneSize { .. })
            && (!self.held_ui_requests.is_empty() || !self.ui_request_ready(&request.action));
        if !waits {
            return self.run_ui_request(&request);
        }
        if self.modal_open() {
            let who = self.ui_requester(&request);
            self.set_notice(format!(
                "{who} asked to change the dashboard; that happens when this dialog closes."
            ));
        }
        self.held_ui_requests.push_back(request);
        while self.held_ui_requests.len() > HELD_UI_REQUESTS {
            self.held_ui_requests.pop_front();
        }
        DashboardAction::None
    }

    /// The oldest waiting request, once it can apply.
    pub fn take_ui_followup(&mut self) -> Option<DashboardAction> {
        let ready = self
            .held_ui_requests
            .front()
            .is_some_and(|request| self.ui_request_ready(&request.action));
        if !ready {
            return None;
        }
        let request = self.held_ui_requests.pop_front()?;
        Some(self.run_ui_request(&request))
    }

    /// Whether `action` can apply without covering or replacing something the
    /// person has open. Staged settings join an open Settings unless the
    /// person is typing a value, searching, or saving there.
    fn ui_request_ready(&self, action: &UiAction) -> bool {
        match action {
            UiAction::PaneSize { .. } => true,
            UiAction::StageSettings { .. } => match &self.mode {
                Mode::Setup(dialog) => {
                    dialog.accepts_staged_edits() && self.away_dialog().is_none()
                }
                _ => !self.modal_open(),
            },
            UiAction::ShowSession { .. } => !self.modal_open(),
        }
    }

    fn run_ui_request(&mut self, request: &UiRequest) -> DashboardAction {
        match &request.action {
            UiAction::PaneSize { pane, size } => {
                let (pane, label) = match pane {
                    UiPane::Sessions => (SupportPane::Sessions, "Sessions"),
                    UiPane::Targets => (SupportPane::Targets, "Targets"),
                    UiPane::Profiles => (SupportPane::Quota, "Profiles"),
                };
                let size_label = match size {
                    PaneSize::Minimized => "minimized",
                    PaneSize::Standard => "standard",
                    PaneSize::Maximized => "maximized",
                };
                self.set_pane_size(pane, *size);
                let who = self.ui_requester(request);
                self.set_notice(format!("{who} set the {label} pane to {size_label}."));
                DashboardAction::None
            }
            UiAction::StageSettings { .. } => self.stage_requested_settings(request),
            UiAction::ShowSession {
                session_id,
                placement,
            } => {
                let Some(workspace_id) = self
                    .state
                    .sessions
                    .get(session_id)
                    .map(|session| session.workspace_id.clone())
                else {
                    let who = self.ui_requester(request);
                    self.set_notice(format!(
                        "{who} asked to show session {session_id}, which this dashboard does not list."
                    ));
                    return DashboardAction::None;
                };
                let direction = match placement {
                    SessionPlacement::Main => None,
                    SessionPlacement::Right => Some(ratatui::layout::Direction::Horizontal),
                    SessionPlacement::Below => Some(ratatui::layout::Direction::Vertical),
                };
                // Panes belong to a workspace; a session in another one opens
                // in its own workspace's conversation instead.
                match direction {
                    Some(direction)
                        if self.active_workspace_id.as_deref() == Some(workspace_id.as_str()) =>
                    {
                        DashboardAction::OpenSessionInSplit {
                            session_id: session_id.clone(),
                            direction,
                        }
                    }
                    _ => self.focus_session_anywhere(&workspace_id, session_id),
                }
            }
        }
    }

    /// Who asked, as the notice names them.
    fn ui_requester(&self, request: &UiRequest) -> String {
        request
            .from_session_id
            .as_deref()
            .and_then(|id| self.state.sessions.get(id))
            .map_or_else(
                || "An agent".to_owned(),
                |session| format!("The agent in {}", crate::render::session_name(session)),
            )
    }

    fn stage_requested_settings(&mut self, request: &UiRequest) -> DashboardAction {
        let UiAction::StageSettings { edits } = &request.action else {
            return DashboardAction::None;
        };
        let who = self.ui_requester(request);
        let listed = edits
            .iter()
            .map(|edit| format!("{} = {}", edit.dotted(), edit.value))
            .collect::<Vec<_>>()
            .join(", ");
        let notice =
            format!("{who} staged {listed}. Review and press Save to keep, or Esc to discard.");
        if let Err(error) = self.begin_staged_settings(edits, notice) {
            self.set_notice(format!("{who} asked to change settings, but {error}"));
        }
        DashboardAction::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{dashboard_with_session, open_new_session_wizard, running_session};
    use mj_client::ui_request::SettingEdit;

    fn bell_off(id: u64) -> UiRequest {
        UiRequest {
            incarnation: "daemon".into(),
            id,
            from_session_id: None,
            action: UiAction::StageSettings {
                edits: vec![SettingEdit {
                    path: vec!["notify".into(), "bell".into()],
                    value: serde_json::json!(!crate::test_support::config().notify.bell),
                }],
            },
        }
    }

    // Agents hold the API token and must not write the person's
    // configuration: a staged edit opens Settings unsaved, on its page.
    #[test]
    fn staged_settings_open_unsaved_on_the_page_that_holds_them() {
        let mut dashboard = dashboard_with_session(running_session());
        let saved = dashboard.config.clone();

        dashboard.apply_ui_request(bell_off(1));

        let Mode::Setup(dialog) = &dashboard.mode else {
            panic!("Settings did not open");
        };
        assert_eq!(dialog.page(), ["notify"]);
        assert!(dialog.is_dirty());
        assert_eq!(dashboard.config, saved);
    }

    #[test]
    fn a_request_made_behind_an_open_dialog_applies_once_the_dialog_closes() {
        let mut dashboard = dashboard_with_session(running_session());
        open_new_session_wizard(&mut dashboard);
        let wizard = std::mem::discriminant(&dashboard.mode);

        dashboard.apply_ui_request(bell_off(1));
        assert_eq!(std::mem::discriminant(&dashboard.mode), wizard);
        assert!(dashboard.take_ui_followup().is_none());

        dashboard.cancel_modal();
        assert!(dashboard.take_ui_followup().is_some());
        assert!(matches!(dashboard.mode, Mode::Setup(_)));
        assert!(dashboard.take_ui_followup().is_none());
    }
}
