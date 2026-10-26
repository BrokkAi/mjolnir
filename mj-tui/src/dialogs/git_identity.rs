//! Asks for the host's Git author identity before the first new session of a
//! run, when Git on this host has none. Sessions inherit the host's global
//! identity, so without it no session can commit.
//!
//! The dashboard never touches Git itself: it returns
//! [`DashboardAction::CheckGitIdentity`] and [`DashboardAction::SaveGitIdentity`]
//! for the binary to ask the daemon in the background, since the daemon's Git
//! environment is the one sessions inherit, and takes the results through
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
use mj_client::daemon::GitIdentity;

use crate::{DashboardAction, DashboardState, Mode};

const EXPLANATION: &str = "Git on this host has no commit author, so sessions cannot commit. \
     Mjolnir saves this to your global Git configuration.";
const UNSET_NOTICE: &str = "Git on this host has no commit author, so sessions cannot commit; start a new session to set one.";
const SKIPPED_NOTICE: &str =
    "Sessions cannot commit until Git has an identity; Mjolnir will ask again next start.";

/// Where this run stands on the host's Git identity. The dialog being open is
/// not a state here: it is [`Mode::GitIdentity`], so closing it by any route
/// can never leave the gate waiting on a dialog that is gone.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) enum GitIdentityGate {
    /// No check is wanted, or this run already decided. Embedders that never
    /// arm the gate, tests among them, start here.
    #[default]
    Settled,
    /// The next new session asks for a check first.
    Unchecked,
    /// The check is running and holds the request that started it.
    Checking(GitIdentityResume),
}

/// The new-session request a check or the dialog holds, continued once the
/// identity is settled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GitIdentityResume {
    /// Open the new-session flow, as the new-session key does.
    BeginNew,
    /// Run a launch that was already decided, such as a fast-start recipe.
    Launch(Box<DashboardAction>),
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
    pub(crate) resume: GitIdentityResume,
    pub(crate) name: TextInput,
    pub(crate) email: TextInput,
    /// A save is running; input waits for its result.
    pub(crate) saving: bool,
    pub(crate) error: Option<String>,
    pub(crate) form: RefCell<Dialog<GitIdentityControl>>,
}

impl GitIdentityDialog {
    fn new(resume: GitIdentityResume, name: Option<String>, email: Option<String>) -> Self {
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
            resume,
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

    /// What the new-session key does first, or `None` once this run has
    /// settled the identity and the request can proceed.
    pub(crate) fn git_identity_gate_action(&mut self) -> Option<DashboardAction> {
        self.hold_for_git_identity(GitIdentityResume::BeginNew)
    }

    /// Every launch that bypasses the new-session key, such as a fast-start
    /// recipe, passes through here: it runs now once the identity is settled,
    /// and otherwise waits for the check.
    pub fn gate_launch_on_git_identity(&mut self, launch: DashboardAction) -> DashboardAction {
        self.hold_for_git_identity(GitIdentityResume::Launch(Box::new(launch.clone())))
            .unwrap_or(launch)
    }

    fn hold_for_git_identity(&mut self, resume: GitIdentityResume) -> Option<DashboardAction> {
        match self.git_identity_gate {
            GitIdentityGate::Settled => None,
            GitIdentityGate::Unchecked => {
                self.git_identity_gate = GitIdentityGate::Checking(resume);
                self.set_notice("Checking the Git identity on this host…");
                Some(DashboardAction::CheckGitIdentity)
            }
            GitIdentityGate::Checking(_) => {
                self.set_notice("Still checking the Git identity on this host…");
                Some(DashboardAction::None)
            }
        }
    }

    /// Takes the identity the daemon read: continues the held request when
    /// no setup is needed, and otherwise asks for the missing part. A check
    /// that failed is reported and does not hold the session back.
    pub fn apply_git_identity_check(&mut self, result: Result<GitIdentity, String>) {
        let GitIdentityGate::Checking(resume) = std::mem::take(&mut self.git_identity_gate) else {
            return;
        };
        match result {
            Ok(identity) if !identity.needs_setup() => {
                self.clear_notice();
                self.settle_git_identity(resume);
            }
            Ok(identity) => {
                if matches!(self.mode, Mode::Dashboard) {
                    // The dialog settles the gate; until then, the next new
                    // session checks again.
                    self.git_identity_gate = GitIdentityGate::Unchecked;
                    self.clear_notice();
                    self.mode = Mode::GitIdentity(GitIdentityDialog::new(
                        resume,
                        identity.name,
                        identity.email,
                    ));
                } else {
                    // The user moved on while Git was read; the dialog does
                    // not open over their work, and the request still runs.
                    self.git_identity_gate = GitIdentityGate::Unchecked;
                    self.set_notice(UNSET_NOTICE);
                    self.resume_after_git_identity(resume);
                }
            }
            Err(error) => {
                self.set_failure_notice(format!(
                    "Could not read the Git identity on this host: {error}"
                ));
                self.settle_git_identity(resume);
            }
        }
    }

    /// Takes the result of saving the identity. A failure keeps the dialog
    /// and what was typed, with the reason under the fields.
    pub fn apply_git_identity_save(&mut self, result: Result<GitIdentity, String>) {
        let saving = matches!(&self.mode, Mode::GitIdentity(dialog) if dialog.saving);
        match result {
            Ok(identity) => {
                self.set_notice(format!(
                    "Git identity saved: {} <{}>",
                    identity.name.unwrap_or_default(),
                    identity.email.unwrap_or_default()
                ));
                if saving {
                    let Mode::GitIdentity(dialog) =
                        std::mem::replace(&mut self.mode, Mode::Dashboard)
                    else {
                        unreachable!("checked above");
                    };
                    self.settle_git_identity(dialog.resume);
                } else {
                    self.git_identity_gate = GitIdentityGate::Settled;
                }
            }
            Err(error) => match &mut self.mode {
                Mode::GitIdentity(dialog) if saving => {
                    dialog.saving = false;
                    dialog.error = Some(error);
                }
                _ => self.set_failure_notice(format!("Could not save the Git identity: {error}")),
            },
        }
    }

    /// The new-session action a settled identity check let through, for the
    /// binary to run as though the user had just asked for it.
    pub fn take_git_identity_followup(&mut self) -> Option<DashboardAction> {
        self.git_identity_followup.take()
    }

    fn settle_git_identity(&mut self, resume: GitIdentityResume) {
        self.git_identity_gate = GitIdentityGate::Settled;
        self.resume_after_git_identity(resume);
    }

    fn resume_after_git_identity(&mut self, resume: GitIdentityResume) {
        let action = match resume {
            // The new-session flow opens over the dashboard only; a user who
            // opened something else meanwhile is told how to continue.
            GitIdentityResume::BeginNew if matches!(self.mode, Mode::Dashboard) => self.begin_new(),
            GitIdentityResume::BeginNew => {
                if self.git_identity_gate == GitIdentityGate::Settled {
                    self.set_notice("Git identity checked; start the new session again.");
                }
                return;
            }
            GitIdentityResume::Launch(launch) => *launch,
        };
        if action != DashboardAction::None {
            self.git_identity_followup = Some(action);
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
                self.mode = Mode::Dashboard;
                self.set_notice(SKIPPED_NOTICE);
                self.settle_git_identity(dialog.resume);
                DashboardAction::None
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
