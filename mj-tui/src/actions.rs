//! The one place that knows what the combined surface's keys do.
//!
//! Every pane command has exactly one [`CommandSpec`] here, carrying its keys,
//! the word the footer uses for it, and a closure that says whether it applies
//! right now. Key handling ([`DashboardState::handle_dashboard_key`]), the
//! footer ([`crate::render::combined_footer_text`]), and the help overlay
//! ([`crate::help`]) all read this table, so a binding and its advertisement
//! cannot drift apart.
//!
//! The registry lives in `mj-tui` rather than a crate of its own because
//! availability is a question about [`DashboardState`].

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::dialogs::{ConfirmDialog, Confirmation};
use crate::{DashboardAction, DashboardState, Focus, dashboard_accelerator};

/// One thing the surface can be asked to do.
///
/// The command palette (`F2`, a later milestone) will add its own id; until it
/// exists there is deliberately no `Palette` entry, because the help overlay
/// lists every entry in this enum and must not advertise a command that does
/// nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommandId {
    OpenSession,
    NewSession,
    ResumeDialog,
    SessionCommands,
    RenameSession,
    ContainerSettings,
    StopSession,
    MarkAllRead,
    CancelOperation,
    ToggleProject,
    RefreshCapacity,
    TargetActions,
    RefreshQuotas,
    EditProfile,
    OpenConfig,
    CycleFocus,
    CyclePaneLayout,
    Workspaces,
    WebViewer,
    QuitDetach,
    Help,
}

/// Where a command belongs: which pane has to own the keyboard for it to
/// apply, or `Global`/`Pane` for the ones that apply wherever the keyboard is.
///
/// `Sessions` is the pane itself (create, resume, mark read); `Session` is the
/// selected row (rename, container settings, stop). `Setup` is the first-run
/// path that only exists while the configuration is empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scope {
    Global,
    Pane,
    Sessions,
    Session,
    Targets,
    Quota,
    Setup,
}

impl Scope {
    /// The heading the help overlay prints above this group.
    pub(crate) const fn heading(self) -> &'static str {
        match self {
            Self::Global => "Anywhere",
            Self::Pane => "Panes",
            Self::Sessions => "Sessions pane",
            Self::Session => "Selected session",
            Self::Targets => "Targets pane",
            Self::Quota => "Quota pane",
            Self::Setup => "First-run setup",
        }
    }
}

/// The order the help overlay prints the groups in, and the order
/// [`available`] walks when it collects what applies at the current focus.
pub(crate) const SCOPE_ORDER: [Scope; 7] = [
    Scope::Sessions,
    Scope::Session,
    Scope::Targets,
    Scope::Quota,
    Scope::Setup,
    Scope::Pane,
    Scope::Global,
];

/// Whether a command can be run, and if not, why the user cannot see it.
///
/// `Hidden` means the command makes no sense here at all (container settings
/// for a session that is not on a container). `Blocked` means it would make
/// sense but something is in the way, and carries the sentence saying so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Availability {
    Ready,
    Hidden,
    Blocked(&'static str),
}

/// One key that runs a command, with the text used to name it on screen.
///
/// `modifiers` holds `KeyModifiers::CONTROL` to mean "the dashboard
/// accelerator", which is Command on macOS and Control everywhere else; see
/// [`dashboard_accelerator`]. `KeyModifiers::NONE` on a character means the
/// plain letter, which only reaches a pane because the composer is a separate
/// focus.
#[derive(Debug, Clone, Copy)]
pub(crate) struct KeyHint {
    pub(crate) code: KeyCode,
    pub(crate) modifiers: KeyModifiers,
    pub(crate) label: &'static str,
}

impl KeyHint {
    const fn plain(code: KeyCode, label: &'static str) -> Self {
        Self {
            code,
            modifiers: KeyModifiers::NONE,
            label,
        }
    }

    const fn accelerated(code: KeyCode, label: &'static str) -> Self {
        Self {
            code,
            modifiers: KeyModifiers::CONTROL,
            label,
        }
    }

    /// Whether a pressed key is this hint. `plain` is the caller's reading of
    /// "no accelerator and no Alt", computed once per key press.
    fn matches(self, key: KeyEvent, plain: bool) -> bool {
        if self.code != key.code {
            return false;
        }
        if self.modifiers.contains(KeyModifiers::CONTROL) {
            dashboard_accelerator(key.modifiers)
        } else if matches!(self.code, KeyCode::Char(_)) {
            // Plain letters are pane keys: a modifier means something else.
            plain
        } else {
            // Enter, Tab, and the function keys have always answered whatever
            // modifiers came with them.
            true
        }
    }
}

/// One command: what it is called, what runs it, and when it applies.
pub(crate) struct CommandSpec {
    pub(crate) id: CommandId,
    pub(crate) label: &'static str,
    pub(crate) description: &'static str,
    pub(crate) scope: Scope,
    pub(crate) keys: &'static [KeyHint],
    /// The word the footer prints after the key, or `None` for commands the
    /// footer never has room to name. Dynamic because
    /// [`CommandId::CancelOperation`] names the operation it would cancel.
    pub(crate) footer: fn(&DashboardState) -> Option<String>,
    pub(crate) available: fn(&DashboardState) -> Availability,
}

fn no_footer(_: &DashboardState) -> Option<String> {
    None
}

/// Builds a `footer` function that always prints the same word.
macro_rules! footer_word {
    ($word:literal) => {{
        fn word(_: &DashboardState) -> Option<String> {
            Some($word.to_owned())
        }
        word as fn(&DashboardState) -> Option<String>
    }};
}

fn always_ready(_: &DashboardState) -> Availability {
    Availability::Ready
}

fn selected_session_ready(dashboard: &DashboardState) -> Availability {
    if dashboard.selected_session().is_some() {
        Availability::Ready
    } else {
        Availability::Hidden
    }
}

/// The gate the session commands share: there must be a selected session, and
/// it must not be in the middle of a launch or a stop.
fn session_idle(dashboard: &DashboardState) -> Availability {
    let Some(session) = dashboard.selected_session() else {
        return Availability::Hidden;
    };
    match dashboard.session_operation_kind(&session.id) {
        Some(_) => Availability::Blocked("an operation is in progress"),
        None => Availability::Ready,
    }
}

fn container_session(dashboard: &DashboardState) -> Availability {
    if dashboard.selected_container_session().is_some() {
        Availability::Ready
    } else {
        Availability::Hidden
    }
}

fn config_present(dashboard: &DashboardState) -> Availability {
    if dashboard.config_is_empty() {
        Availability::Blocked("configure at least one profile and target first")
    } else {
        Availability::Ready
    }
}

fn config_absent(dashboard: &DashboardState) -> Availability {
    if dashboard.config_is_empty() {
        Availability::Ready
    } else {
        Availability::Hidden
    }
}

fn profiles_present(dashboard: &DashboardState) -> Availability {
    if dashboard.config.profiles.is_empty() {
        Availability::Hidden
    } else {
        Availability::Ready
    }
}

fn cancel_footer(dashboard: &DashboardState) -> Option<String> {
    let session = dashboard.selected_session()?;
    let kind = dashboard.session_operation_kind(&session.id)?;
    Some(format!("cancel {}", kind.label().to_lowercase()))
}

fn operation_in_flight(dashboard: &DashboardState) -> Availability {
    match cancel_footer(dashboard) {
        Some(_) => Availability::Ready,
        None => Availability::Hidden,
    }
}

/// Every command the surface has. The order here is the order the footer
/// prints its hints and the order the help overlay prints each group.
pub(crate) static COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        id: CommandId::OpenSession,
        label: "Open session",
        description: "Show the selected session's conversation and type in it.",
        scope: Scope::Sessions,
        keys: &[KeyHint::plain(KeyCode::Enter, "Enter")],
        footer: footer_word!("open"),
        available: selected_session_ready,
    },
    CommandSpec {
        id: CommandId::NewSession,
        label: "New session",
        description: "Start the wizard that picks a profile, a bundle, and a target.",
        scope: Scope::Sessions,
        keys: &[KeyHint::plain(KeyCode::Char('n'), "n")],
        footer: footer_word!("new"),
        available: config_present,
    },
    CommandSpec {
        id: CommandId::ResumeDialog,
        label: "Resume a session",
        description: "Open the picker for every session that is not live.",
        scope: Scope::Sessions,
        keys: &[KeyHint::plain(KeyCode::Char('s'), "s")],
        footer: footer_word!("resume"),
        available: always_ready,
    },
    CommandSpec {
        id: CommandId::MarkAllRead,
        label: "Mark all read",
        description: "Clear the unread marker on every session at once.",
        scope: Scope::Sessions,
        keys: &[KeyHint::plain(KeyCode::Char('a'), "a")],
        footer: footer_word!("mark read"),
        available: always_ready,
    },
    CommandSpec {
        id: CommandId::CancelOperation,
        label: "Cancel operation",
        description: "Stop the launch, resume, or stop the selected session is in the middle of.",
        scope: Scope::Sessions,
        keys: &[KeyHint::plain(KeyCode::Char('x'), "x")],
        footer: cancel_footer,
        available: operation_in_flight,
    },
    CommandSpec {
        id: CommandId::ToggleProject,
        label: "Fold project",
        description: "Space folds the selected session's project; 1 to 9 fold by number.",
        scope: Scope::Sessions,
        keys: &[KeyHint::plain(KeyCode::Char(' '), "Space")],
        footer: no_footer,
        available: selected_session_ready,
    },
    CommandSpec {
        id: CommandId::SessionCommands,
        label: "Session commands",
        description: "Open the selected session's rename, container, and stop actions.",
        scope: Scope::Session,
        keys: &[KeyHint::plain(KeyCode::Char('e'), "e")],
        footer: footer_word!("edit"),
        available: session_idle,
    },
    CommandSpec {
        id: CommandId::RenameSession,
        label: "Rename session",
        description: "Give the selected session your own title.",
        scope: Scope::Session,
        keys: &[],
        footer: no_footer,
        available: session_idle,
    },
    CommandSpec {
        id: CommandId::ContainerSettings,
        label: "Container settings",
        description: "Edit CPU, memory, and mounts for the next time the container is created.",
        scope: Scope::Session,
        keys: &[],
        footer: no_footer,
        available: container_session,
    },
    CommandSpec {
        id: CommandId::StopSession,
        label: "Stop session",
        description: "Shut the selected session down, after a confirmation.",
        scope: Scope::Session,
        keys: &[],
        footer: no_footer,
        available: selected_session_ready,
    },
    CommandSpec {
        id: CommandId::RefreshCapacity,
        label: "Refresh capacity",
        description: "Re-probe the targets and redraw their load.",
        scope: Scope::Targets,
        keys: &[KeyHint::plain(KeyCode::Char('r'), "r")],
        footer: footer_word!("refresh"),
        available: always_ready,
    },
    CommandSpec {
        id: CommandId::TargetActions,
        label: "Target actions",
        description: "Test or rename the selected target.",
        scope: Scope::Targets,
        keys: &[
            KeyHint::plain(KeyCode::Enter, "Enter"),
            KeyHint::plain(KeyCode::Char('e'), "e"),
        ],
        footer: footer_word!("actions"),
        available: always_ready,
    },
    CommandSpec {
        id: CommandId::RefreshQuotas,
        label: "Refresh quotas",
        description: "Ask every profile for its quota again.",
        scope: Scope::Quota,
        keys: &[KeyHint::plain(KeyCode::Char('r'), "r")],
        footer: footer_word!("refresh"),
        available: always_ready,
    },
    CommandSpec {
        id: CommandId::EditProfile,
        label: "Edit profile",
        description: "Rename the selected profile's configuration id.",
        scope: Scope::Quota,
        keys: &[
            KeyHint::plain(KeyCode::Enter, "Enter"),
            KeyHint::plain(KeyCode::Char('e'), "e"),
        ],
        footer: footer_word!("edit profile"),
        available: profiles_present,
    },
    CommandSpec {
        id: CommandId::OpenConfig,
        label: "Open setup",
        description: "Run first-run setup, which writes a working configuration.",
        scope: Scope::Setup,
        keys: &[KeyHint::plain(KeyCode::Char('e'), "e")],
        footer: footer_word!("setup"),
        available: config_absent,
    },
    CommandSpec {
        id: CommandId::CycleFocus,
        label: "Next pane",
        description: "Move the keyboard down the layout; Shift-Tab reverses it.",
        scope: Scope::Pane,
        keys: &[KeyHint::plain(KeyCode::Tab, "Tab")],
        footer: footer_word!("pane"),
        available: always_ready,
    },
    CommandSpec {
        id: CommandId::CyclePaneLayout,
        label: "Pane layout",
        description: "Turn the two-position dial: panes open, or collapsed for the conversation.",
        scope: Scope::Pane,
        keys: &[KeyHint::accelerated(KeyCode::Char('g'), "Ctrl-G")],
        footer: footer_word!("panes"),
        available: always_ready,
    },
    CommandSpec {
        id: CommandId::Workspaces,
        label: "Workspaces",
        description: "Switch to another workspace.",
        scope: Scope::Global,
        keys: &[KeyHint::plain(KeyCode::F(2), "F2")],
        footer: footer_word!("workspaces"),
        available: always_ready,
    },
    CommandSpec {
        id: CommandId::WebViewer,
        label: "Web viewer",
        description: "Show the address and code for the browser and phone viewer.",
        scope: Scope::Global,
        keys: &[KeyHint::plain(KeyCode::F(3), "F3")],
        footer: footer_word!("web"),
        available: always_ready,
    },
    CommandSpec {
        id: CommandId::QuitDetach,
        label: "Detach",
        description: "Leave the terminal surface; the sessions keep running.",
        scope: Scope::Global,
        keys: &[KeyHint::accelerated(KeyCode::Char('q'), "Ctrl-Q")],
        footer: footer_word!("quit"),
        available: always_ready,
    },
    CommandSpec {
        id: CommandId::Help,
        label: "Help",
        description: "List every key this surface answers.",
        scope: Scope::Global,
        keys: &[
            KeyHint::plain(KeyCode::F(1), "F1"),
            KeyHint::plain(KeyCode::Char('?'), "?"),
        ],
        footer: footer_word!("help"),
        available: always_ready,
    },
];

/// The specification for one command. Panics only if `COMMANDS` has lost an
/// entry, which a unit test in this module rules out.
pub(crate) fn spec(id: CommandId) -> &'static CommandSpec {
    COMMANDS
        .iter()
        .find(|spec| spec.id == id)
        .expect("every CommandId has one entry in COMMANDS")
}

/// Whether a command in `scope` can be run while `focus` owns the keyboard.
///
/// `Setup` is deliberately excluded from key matching (see [`spec_for_key`]);
/// it is listed here so the footer can offer it while the configuration is
/// still empty.
fn scope_applies(scope: Scope, focus: Focus) -> bool {
    match scope {
        Scope::Global | Scope::Pane | Scope::Setup => true,
        Scope::Sessions | Scope::Session => focus == Focus::Sessions,
        Scope::Targets => focus == Focus::Targets,
        Scope::Quota => focus == Focus::Quota,
    }
}

/// The command a pane key press runs, or `None` if the panes do not answer it.
///
/// The "plain rule": a bare character only counts when no accelerator and no
/// Alt is held, because the panes are the one place where a letter cannot be
/// mistaken for text. Characters are ignored entirely while the composer has
/// focus, which keeps the old behaviour where the panes answered nothing there.
///
/// `Scope::Setup` never matches here. Its key is `e`, which the Sessions,
/// Targets, and Quota panes also use; the caller resolves that ambiguity by
/// checking for an empty configuration first, exactly as the surface always
/// has.
pub(crate) fn spec_for_key(key: KeyEvent, focus: Focus) -> Option<CommandId> {
    let plain = !key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER);
    COMMANDS
        .iter()
        .find(|spec| {
            spec.scope != Scope::Setup
                && scope_applies(spec.scope, focus)
                && spec.keys.iter().any(|hint| {
                    if focus == Focus::Prompt
                        && matches!(hint.code, KeyCode::Char(_))
                        && !hint.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        return false;
                    }
                    hint.matches(key, plain)
                })
        })
        .map(|spec| spec.id)
}

/// The commands that are ready to run right now, in registry order.
///
/// `scope_filter` of `Some(scope)` asks for one group; `None` asks for
/// everything that applies where the keyboard currently is, which is what the
/// footer wants.
pub(crate) fn available(dashboard: &DashboardState, scope_filter: Option<Scope>) -> Vec<CommandId> {
    COMMANDS
        .iter()
        .filter(|spec| match scope_filter {
            Some(scope) => spec.scope == scope,
            None => scope_applies(spec.scope, dashboard.focus),
        })
        .filter(|spec| (spec.available)(dashboard) == Availability::Ready)
        .map(|spec| spec.id)
        .collect()
}

impl DashboardState {
    /// Runs one registry command. Every arm calls the same entry point the
    /// key handler used to call directly, so the footer, the help overlay, and
    /// the keyboard cannot disagree about what a command does.
    pub(crate) fn dispatch_command(&mut self, id: CommandId) -> DashboardAction {
        match id {
            CommandId::OpenSession => self.open_selected_session(),
            CommandId::NewSession => self.begin_new(),
            CommandId::ResumeDialog => DashboardAction::OpenResumeDialog,
            CommandId::SessionCommands => {
                self.begin_session_edit();
                DashboardAction::None
            }
            CommandId::RenameSession => {
                self.begin_rename();
                DashboardAction::None
            }
            CommandId::ContainerSettings => {
                self.begin_container_edit();
                DashboardAction::None
            }
            CommandId::StopSession => {
                let Some(session_id) = self.selected_session().map(|session| session.id.clone())
                else {
                    return DashboardAction::None;
                };
                let reviewer_conversation = self.sessions_with_review.contains(&session_id);
                self.mode = crate::Mode::Confirm(ConfirmDialog::new(Confirmation::Close {
                    session_id,
                    reviewer_conversation,
                }));
                DashboardAction::None
            }
            CommandId::MarkAllRead => self.mark_all_read(),
            CommandId::CancelOperation => {
                let operation = self.selected_session().and_then(|session| {
                    self.session_operation_kind(&session.id)
                        .map(|kind| (session.id.clone(), kind))
                });
                operation.map_or(DashboardAction::None, |(session_id, kind)| {
                    DashboardAction::CancelOperation { session_id, kind }
                })
            }
            CommandId::ToggleProject => {
                self.toggle_selected_project();
                DashboardAction::None
            }
            CommandId::RefreshCapacity => DashboardAction::RefreshCapacity,
            CommandId::TargetActions => {
                self.begin_target_actions();
                DashboardAction::None
            }
            CommandId::RefreshQuotas => DashboardAction::RefreshQuotas,
            CommandId::EditProfile => {
                self.begin_profile_rename();
                DashboardAction::None
            }
            CommandId::OpenConfig => DashboardAction::OpenConfig,
            CommandId::CycleFocus => {
                self.cycle_focus(false);
                DashboardAction::None
            }
            CommandId::CyclePaneLayout => {
                self.cycle_pane_layout();
                DashboardAction::None
            }
            CommandId::Workspaces => DashboardAction::OpenWorkspacePicker,
            CommandId::WebViewer => self.open_web_dialog(),
            CommandId::QuitDetach => DashboardAction::QuitDetach,
            CommandId::Help => {
                self.begin_help();
                DashboardAction::None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionOperationKind;
    use crate::test_support::{dashboard_with_session, operation, running_session};

    /// `spec()` panics on a missing entry, so prove every id has one before
    /// any other test relies on it.
    #[test]
    fn every_command_id_has_exactly_one_spec() {
        for entry in COMMANDS {
            assert_eq!(spec(entry.id).id, entry.id);
            assert_eq!(
                COMMANDS
                    .iter()
                    .filter(|candidate| candidate.id == entry.id)
                    .count(),
                1,
                "{:?} appears more than once",
                entry.id
            );
        }
    }

    /// Two commands answering the same key in the same place would make the
    /// registry order, rather than the user's intent, decide what happens.
    #[test]
    fn no_two_commands_claim_the_same_key_in_one_pane() {
        for focus in [Focus::Sessions, Focus::Targets, Focus::Quota] {
            let mut seen: Vec<(KeyCode, KeyModifiers)> = Vec::new();
            for entry in COMMANDS {
                if entry.scope == Scope::Setup || !scope_applies(entry.scope, focus) {
                    continue;
                }
                for hint in entry.keys {
                    let key = (hint.code, hint.modifiers);
                    assert!(
                        !seen.contains(&key),
                        "{key:?} is claimed twice at {focus:?}"
                    );
                    seen.push(key);
                }
            }
        }
    }

    #[test]
    fn cancel_is_available_only_while_an_operation_runs() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        assert!(!available(&dashboard, None).contains(&CommandId::CancelOperation));

        dashboard.session_operations.insert(
            "session-1".into(),
            operation(SessionOperationKind::Launching, None),
        );
        assert!(available(&dashboard, None).contains(&CommandId::CancelOperation));
        assert_eq!(
            (spec(CommandId::CancelOperation).footer)(&dashboard).as_deref(),
            Some("cancel launch")
        );
    }
}
