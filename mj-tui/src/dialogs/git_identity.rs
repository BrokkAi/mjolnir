//! Asks for the host's Git author identity before the first new session of a
//! run, when Git on this host has none. Sessions inherit the host's global
//! identity, so without it no session can commit.
//!
//! The dashboard never touches Git itself: it returns
//! [`DashboardAction::CheckGitIdentity`] and [`DashboardAction::SaveGitIdentity`]
//! for the binary to run in the background, and takes the results through
//! [`DashboardState::apply_git_identity_check`] and
//! [`DashboardState::apply_git_identity_save`].

use std::cell::RefCell;

use crossterm::event::Event;
use mj_chat::components::{ControlKind, Dialog, Interaction, TextField};
use mj_chat::selection::FrameSurfaces;
use mj_chat::text_input::TextInput;
use mj_chat::theme;
use ratatui::Frame;
use ratatui::layout::{Margin, Rect};
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Wrap};

use crate::widgets::{centered_modal, dismissible_modal_title};
use crate::{DashboardAction, DashboardState, Mode};

const EXPLANATION: &str = "Git on this host has no commit author, so sessions cannot commit. \
     Mjolnir saves this to your global Git configuration.";
const SKIPPED_NOTICE: &str =
    "Sessions cannot commit until Git has an identity; Mjolnir will ask again next start.";

/// Where this run stands on the host's Git identity. The dialog being open is
/// not a state here: it is [`Mode::GitIdentity`], so closing it by any route
/// can never leave the gate waiting on a dialog that is gone.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum GitIdentityGate {
    /// No check is wanted, or this run already decided. Embedders that never
    /// arm the gate, tests among them, start here.
    #[default]
    Settled,
    /// The next new session asks for a check first.
    Unchecked,
    /// The check is running; another new-session request waits for it.
    Checking,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GitIdentityControl {
    Name,
    Email,
    Skip,
    Save,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GitIdentityDialog {
    pub(crate) name: TextInput,
    pub(crate) email: TextInput,
    /// A save is running; input waits for its result.
    pub(crate) saving: bool,
    pub(crate) error: Option<String>,
    pub(crate) form: RefCell<Dialog<GitIdentityControl>>,
}

impl GitIdentityDialog {
    fn new(name: Option<String>, email: Option<String>) -> Self {
        let initial = if name.is_some() {
            GitIdentityControl::Email
        } else {
            GitIdentityControl::Name
        };
        let mut form = Dialog::new();
        form.declare(GitIdentityControl::Name, ControlKind::TextField);
        form.declare(GitIdentityControl::Email, ControlKind::TextField);
        form.declare(GitIdentityControl::Skip, ControlKind::Button);
        form.declare(GitIdentityControl::Save, ControlKind::Button);
        form.end_frame(initial);
        Self {
            name: TextInput::from_value(name.unwrap_or_default()),
            email: TextInput::from_value(email.unwrap_or_default()),
            saving: false,
            error: None,
            form: RefCell::new(form),
        }
    }

    pub(crate) fn text_input_focused(&self) -> bool {
        !self.saving
            && self
                .form
                .borrow()
                .focused()
                .is_none_or(|id| matches!(id, GitIdentityControl::Name | GitIdentityControl::Email))
    }

    pub(crate) fn prepare_dialog_state(&mut self) {
        let values = vec![self.name.to_string(), self.email.to_string()];
        let form = self.form.get_mut();
        form.track_draft(values);
        form.set_dismiss_actions(&[GitIdentityControl::Skip]);
        form.set_submit(GitIdentityControl::Name, GitIdentityControl::Save);
        form.set_submit(GitIdentityControl::Email, GitIdentityControl::Save);
        form.set_default_action(GitIdentityControl::Save);
    }
}

impl DashboardState {
    /// Asks for a Git identity check before this run's first new session. The
    /// binary arms this; a dashboard that is never armed skips the check.
    pub fn require_git_identity_check(&mut self) {
        self.git_identity_gate = GitIdentityGate::Unchecked;
    }

    /// What a new-session request does first, or `None` once this run has
    /// settled the identity and the request can proceed.
    pub(crate) fn git_identity_gate_action(&mut self) -> Option<DashboardAction> {
        match self.git_identity_gate {
            GitIdentityGate::Settled => None,
            GitIdentityGate::Unchecked => {
                self.git_identity_gate = GitIdentityGate::Checking;
                Some(DashboardAction::CheckGitIdentity)
            }
            GitIdentityGate::Checking => Some(DashboardAction::None),
        }
    }

    /// Takes the identity the background check read: proceeds into the new
    /// session when it is complete, and otherwise asks for the missing part.
    /// A check that failed is reported and does not hold the session back.
    pub fn apply_git_identity_check(
        &mut self,
        result: Result<(Option<String>, Option<String>), String>,
    ) {
        if self.git_identity_gate != GitIdentityGate::Checking {
            return;
        }
        match result {
            Ok((Some(_), Some(_))) => self.settle_git_identity(),
            Ok((name, email)) => {
                // Until the dialog settles it, the next new session checks
                // again. That also covers a user who moved on while Git was
                // read: the dialog does not open over their work.
                self.git_identity_gate = GitIdentityGate::Unchecked;
                if matches!(self.mode, Mode::Dashboard) {
                    self.mode = Mode::GitIdentity(GitIdentityDialog::new(name, email));
                }
            }
            Err(error) => {
                self.set_failure_notice(format!(
                    "Could not read the Git identity on this host: {error}"
                ));
                self.settle_git_identity();
            }
        }
    }

    /// Takes the result of saving the identity. A failure keeps the dialog
    /// and what was typed, with the reason under the fields.
    pub fn apply_git_identity_save(&mut self, result: Result<(String, String), String>) {
        let dialog = match &mut self.mode {
            Mode::GitIdentity(dialog) if dialog.saving => Some(dialog),
            _ => None,
        };
        match (result, dialog) {
            (Ok((name, email)), dialog) => {
                let open = dialog.is_some();
                self.set_notice(format!("Git identity saved: {name} <{email}>"));
                if open {
                    self.mode = Mode::Dashboard;
                    self.settle_git_identity();
                } else {
                    self.git_identity_gate = GitIdentityGate::Settled;
                }
            }
            (Err(error), Some(dialog)) => {
                dialog.saving = false;
                dialog.error = Some(error);
            }
            (Err(error), None) => {
                self.set_failure_notice(format!("Could not save the Git identity: {error}"));
            }
        }
    }

    /// The new-session action a settled identity check let through, for the
    /// binary to run as though the user had just asked for it.
    pub fn take_git_identity_followup(&mut self) -> Option<DashboardAction> {
        self.git_identity_followup.take()
    }

    fn settle_git_identity(&mut self) {
        self.git_identity_gate = GitIdentityGate::Settled;
        if matches!(self.mode, Mode::Dashboard) {
            let action = self.begin_new();
            if action != DashboardAction::None {
                self.git_identity_followup = Some(action);
            }
        }
    }

    pub(crate) fn handle_git_identity_event(
        &mut self,
        event: Event,
        mut dialog: GitIdentityDialog,
    ) -> DashboardAction {
        if dialog.saving {
            self.last_event_consumed.set(true);
            self.mode = Mode::GitIdentity(dialog);
            return DashboardAction::None;
        }
        let result = dialog.form.get_mut().handle(&event);
        self.last_event_consumed.set(result.consumed);
        match result.action {
            Some(Interaction::Cancel | Interaction::Activate(GitIdentityControl::Skip)) => {
                self.git_identity_gate = GitIdentityGate::Settled;
                self.mode = Mode::Dashboard;
                self.set_notice(SKIPPED_NOTICE);
                self.begin_new()
            }
            Some(Interaction::Edit(id, edit)) => {
                let field = match id {
                    GitIdentityControl::Name => &mut dialog.name,
                    GitIdentityControl::Email => &mut dialog.email,
                    _ => {
                        self.mode = Mode::GitIdentity(dialog);
                        return DashboardAction::None;
                    }
                };
                TextField::apply(field, edit);
                self.mode = Mode::GitIdentity(dialog);
                DashboardAction::None
            }
            Some(Interaction::Activate(
                GitIdentityControl::Name | GitIdentityControl::Email | GitIdentityControl::Save,
            )) => {
                dialog.saving = true;
                dialog.error = None;
                let action = DashboardAction::SaveGitIdentity {
                    name: dialog.name.to_string(),
                    email: dialog.email.to_string(),
                };
                self.mode = Mode::GitIdentity(dialog);
                action
            }
            _ => {
                self.mode = Mode::GitIdentity(dialog);
                DashboardAction::None
            }
        }
    }
}

pub(crate) fn render_git_identity_dialog(
    frame: &mut Frame,
    area: Rect,
    dialog: &GitIdentityDialog,
    surfaces: &mut FrameSurfaces,
) {
    let popup = centered_modal(frame, surfaces, 60, 12, area);
    let inner = popup.inner(Margin {
        horizontal: 1,
        vertical: 1,
    });
    let mut form = dialog.form.borrow_mut();
    form.begin_frame();
    let title = dismissible_modal_title(
        &mut form,
        popup,
        "Set up Git identity",
        theme::title(true),
        !dialog.saving,
    );
    frame.render_widget(theme::modal().title(title), popup);
    let row = |offset: u16, height: u16| {
        Rect::new(
            inner.x,
            inner.y.saturating_add(offset),
            inner.width,
            height.min(inner.height.saturating_sub(offset)),
        )
    };
    frame.render_widget(
        Paragraph::new(EXPLANATION)
            .style(Style::default().fg(theme::palette().muted))
            .wrap(Wrap { trim: true }),
        row(0, 3),
    );
    for (offset, label, input, id) in [
        (4, "Name:  ", &dialog.name, GitIdentityControl::Name),
        (5, "Email: ", &dialog.email, GitIdentityControl::Email),
    ] {
        let rect = row(offset, 1);
        let label_width = (label.len() as u16).min(rect.width);
        frame.render_widget(
            Line::raw(label),
            Rect::new(rect.x, rect.y, label_width, rect.height),
        );
        TextField::render(
            frame,
            Rect::new(
                rect.x + label_width,
                rect.y,
                rect.width - label_width,
                rect.height,
            ),
            input,
            &mut form,
            id,
        );
    }
    let status = if dialog.saving {
        Some(Line::styled("Saving Git identity…", theme::muted()))
    } else {
        dialog.error.as_deref().map(|error| {
            Line::styled(
                error.to_owned(),
                Style::default().fg(theme::palette().warning),
            )
        })
    };
    if let Some(status) = status {
        frame.render_widget(status, row(7, 1));
    }
    let footer = Rect::new(
        inner.x,
        inner.bottom().saturating_sub(1),
        inner.width,
        u16::from(inner.height > 0),
    );
    Dialog::render_actions(
        frame,
        footer,
        &[
            (GitIdentityControl::Skip, "Skip", !dialog.saving),
            (GitIdentityControl::Save, "Save", !dialog.saving),
        ],
        &mut form,
    );
    form.end_frame(GitIdentityControl::Name);
}
