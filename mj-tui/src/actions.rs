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
use mj_chat::components::EventResult;
use mj_core::config::KeyAction;
use mj_core::state::SessionTransitionKind;

use crate::dialogs::{ConfirmDialog, Confirmation};
use crate::tile_layout::NavDirection;
use crate::{DashboardAction, DashboardState, Focus};

/// One thing the surface can be asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandId {
    OpenSession,
    PinSession,
    UnpinSession,
    OpenSessionSplitRight,
    OpenSessionSplitBelow,
    ClosePane,
    FocusPaneLeft,
    FocusPaneDown,
    FocusPaneUp,
    FocusPaneRight,
    FocusLastPane,
    ZoomPane,
    ResizeMode,
    SwapPaneLeft,
    SwapPaneDown,
    SwapPaneUp,
    SwapPaneRight,
    SuspendSession,
    RenameWorkspace,
    CloseWorkspace,
    ResizePaneLeft,
    ResizePaneDown,
    ResizePaneUp,
    ResizePaneRight,
    NewSessionWizard,
    ChangeGoSetup,
    RestartSession,
    ResumeDialog,
    RenameSession,
    ChangedFiles,
    ContainerSettings,
    MoveSession,
    DestroySession,
    MarkAllRead,
    FilterSessions,
    NextAttention,
    PreviousAttention,
    CancelOperation,
    ToggleProject,
    TargetActions,
    EditProfile,
    Refresh,
    OpenConfig,
    ManageProfiles,
    ManageMachines,
    ManageTargets,
    CycleFocus,
    CycleFocusReverse,
    CycleFocusedPaneSize,
    TogglePanePreset,
    Workspaces,
    FocusWorkspaces,
    SelectWorkspacePrevious,
    SelectWorkspaceNext,
    SwitchWorkspace,
    WebViewer,
    RestartDaemon,
    NoticeLog,
    QuitDetach,
    Palette,
    CycleSpinner,
    ToggleTranscriptRendering,
    ToggleDictation,
    OpenSubagents,
    Help,
}

/// Where a command belongs: which pane has to own the keyboard for it to
/// apply, or `Global`/`Pane`/`Settings` for commands available from any pane.
///
/// `Sessions` is the pane itself (create, mark read); `Session` is the
/// selected row (rename, container settings, stop). `Setup` is the first-run
/// path that only exists while the configuration is empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scope {
    Global,
    Settings,
    Pane,
    Sessions,
    Session,
    Targets,
    Quota,
    Setup,
}

impl Scope {
    /// The heading the command palette prints above this scope.
    pub(crate) const fn heading(self) -> &'static str {
        match self {
            Self::Global => "Anywhere",
            Self::Settings => "Settings",
            Self::Pane => "Panes",
            Self::Sessions => "Sessions pane",
            Self::Session => "Selected session",
            Self::Targets => "Targets pane",
            Self::Quota => "Quota pane",
            Self::Setup => "First-run settings",
        }
    }
}

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

/// One plain key a focused pane answers, with the text used to name it.
///
/// Pane keys are never configurable: they are the bare letters, Enter, Tab,
/// Space, and `?` that only reach a pane, because the composer is a separate
/// focus and reads those characters as text. Everything a user can rebind is
/// a [`KeyAction`] on the spec instead.
#[derive(Debug, Clone, Copy)]
pub(crate) struct KeyHint {
    pub(crate) code: KeyCode,
    pub(crate) label: &'static str,
}

impl KeyHint {
    const fn plain(code: KeyCode, label: &'static str) -> Self {
        Self { code, label }
    }

    /// Whether a pressed key is this hint. `plain` is the caller's reading of
    /// "no accelerator and no Alt", computed once per key press.
    fn matches(self, key: KeyEvent, plain: bool) -> bool {
        if self.code != key.code {
            return false;
        }
        if matches!(self.code, KeyCode::Char(_)) {
            // Plain letters are pane keys: a modifier means something else.
            plain
        } else {
            // Enter and Tab have always answered whatever modifiers came
            // with them.
            true
        }
    }
}

/// Which of the footer's two groups a hint prints in.
///
/// The row reads `pane commands │ prefix chords`, so the reader always finds a
/// key in the same place: what applies here, then what applies everywhere
/// after the prefix. Group membership is stated here rather than inferred from
/// [`Scope`], because `Tab` and the pane preset share a scope and belong in
/// different groups.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum FooterGroup {
    Pane,
    Chord,
}

/// One command: what it is called, what runs it, and when it applies.
pub(crate) struct CommandSpec {
    pub(crate) id: CommandId,
    pub(crate) label: &'static str,
    pub(crate) description: &'static str,
    pub(crate) scope: Scope,
    /// Plain keys a focused pane answers. Not configurable.
    pub(crate) pane_keys: &'static [KeyHint],
    /// The configurable action that also runs this command, if any.
    pub(crate) action: Option<KeyAction>,
    /// The word the footer prints after the key, or `None` for commands the
    /// footer never has room to name. Dynamic because
    /// [`CommandId::CancelOperation`] names the operation it would cancel.
    pub(crate) footer: fn(&DashboardState) -> Option<String>,
    /// Which footer group the hint prints in, and where inside it. Equal
    /// ranks keep registry order, so only the groups whose order matters to
    /// the reader — the chords and the function keys — carry distinct ranks.
    pub(crate) footer_group: FooterGroup,
    pub(crate) footer_rank: u8,
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

fn spinner_available(dashboard: &DashboardState) -> Availability {
    if dashboard.spinner_save_pending {
        Availability::Blocked("The spinner preference is being saved")
    } else {
        Availability::Ready
    }
}

fn active_workspace_ready(dashboard: &DashboardState) -> Availability {
    if dashboard.active_workspace_id().is_some() {
        Availability::Ready
    } else {
        Availability::Blocked("No active workspace")
    }
}

/// The gate the conversation-pane commands share. They act on the pane the
/// conversation is in, so they apply where a conversation is what the
/// keyboard is near: the composer itself, or the Sessions list beside it.
fn conversation_pane_ready(dashboard: &DashboardState) -> Availability {
    if matches!(dashboard.focus, Focus::Prompt | Focus::Sessions) {
        Availability::Ready
    } else {
        Availability::Hidden
    }
}

fn selected_session_ready(dashboard: &DashboardState) -> Availability {
    if dashboard.command_session().is_some() {
        Availability::Ready
    } else {
        Availability::Hidden
    }
}

fn pin_session_available(dashboard: &DashboardState) -> Availability {
    let Some(session) = dashboard.command_session() else {
        return Availability::Hidden;
    };
    if dashboard.pin_id(&session.id).is_some() {
        Availability::Blocked("this session is already pinned")
    } else {
        Availability::Ready
    }
}

fn unpin_session_available(dashboard: &DashboardState) -> Availability {
    let Some(session) = dashboard.command_session() else {
        return Availability::Hidden;
    };
    if dashboard.pin_id(&session.id).is_some() {
        Availability::Ready
    } else {
        Availability::Blocked("this session is not pinned")
    }
}

/// The gate the session commands share: there must be a selected session, and
/// it must not be in the middle of a launch or a stop.
fn session_idle(dashboard: &DashboardState) -> Availability {
    let Some(session) = dashboard.command_session() else {
        return Availability::Hidden;
    };
    if dashboard.move_queue_admission_incomplete(&session.id) {
        return Availability::Blocked("Move queue admission is incomplete; retry Move first");
    }
    if dashboard.transition_kind(&session.id).is_some() {
        return Availability::Blocked("a session transition is in progress");
    }
    if dashboard.transition_failure_kind(&session.id).is_some() {
        return Availability::Blocked("recover the failed transition first");
    }
    match dashboard.session_operation_kind(&session.id) {
        Some(_) => Availability::Blocked("an operation is in progress"),
        None => Availability::Ready,
    }
}

fn suspend_session_available(dashboard: &DashboardState) -> Availability {
    let Some(session) = dashboard.command_session() else {
        return Availability::Hidden;
    };
    if !session.state.is_active() {
        return Availability::Hidden;
    }
    // A close or destroy that failed part-way is exactly what Stop should be
    // able to retry: the daemon re-runs the interrupted close when it gets a
    // Close for a Closing or Destroying record. `transition_failure_kind` is
    // already None while an operation is in flight, so the only other gate
    // that still applies here is move queue admission.
    if matches!(
        dashboard.transition_failure_kind(&session.id),
        Some(SessionTransitionKind::Suspending | SessionTransitionKind::Destroying)
    ) {
        return if dashboard.move_queue_admission_incomplete(&session.id) {
            Availability::Blocked("Move queue admission is incomplete; retry Move first")
        } else {
            Availability::Ready
        };
    }
    session_idle(dashboard)
}

fn restart_session_available(dashboard: &DashboardState) -> Availability {
    let Some(session) = dashboard.command_session() else {
        return Availability::Hidden;
    };
    if !session.state.is_active() && session.checkpoint.is_none() {
        return Availability::Blocked("this session has no recovery copy to restart");
    }
    session_idle(dashboard)
}

fn move_session_available(dashboard: &DashboardState) -> Availability {
    let Some(session) = dashboard.command_session() else {
        return Availability::Hidden;
    };
    if !session.state.is_active() {
        return Availability::Hidden;
    }
    session_idle(dashboard)
}

/// A selected session whose target is running, which is what reading its
/// checkout needs.
fn live_session(dashboard: &DashboardState) -> Availability {
    let Some(session) = dashboard.command_session() else {
        return Availability::Hidden;
    };
    if session.state.is_active() && session.target.is_some() {
        Availability::Ready
    } else {
        Availability::Blocked("the session's target is not running")
    }
}

fn container_session(dashboard: &DashboardState) -> Availability {
    let Some(session) = dashboard.selected_container_session() else {
        return Availability::Hidden;
    };
    if dashboard.move_queue_admission_incomplete(&session.id) {
        return Availability::Blocked("Move queue admission is incomplete; retry Move first");
    }
    Availability::Ready
}

fn config_present(dashboard: &DashboardState) -> Availability {
    if dashboard.config_is_empty() {
        Availability::Blocked("configure at least one profile and target first")
    } else {
        Availability::Ready
    }
}

fn profiles_present(dashboard: &DashboardState) -> Availability {
    if dashboard.pane_size(crate::SupportPane::Quota) == crate::PaneSize::Minimized {
        return Availability::Hidden;
    }
    if dashboard.config.enabled_profiles().next().is_none() {
        Availability::Hidden
    } else {
        Availability::Ready
    }
}

fn targets_visible(dashboard: &DashboardState) -> Availability {
    if dashboard.pane_size(crate::SupportPane::Targets) == crate::PaneSize::Minimized {
        Availability::Hidden
    } else {
        Availability::Ready
    }
}

fn support_pane_focused(dashboard: &DashboardState) -> Availability {
    if dashboard.focus().support_pane().is_some() {
        Availability::Ready
    } else {
        Availability::Blocked("select Sessions, Targets, or Quota first")
    }
}

fn cancel_footer(dashboard: &DashboardState) -> Option<String> {
    let session = dashboard.command_session()?;
    let operation = dashboard.session_operations.get(&session.id)?;
    if !operation.cancellable {
        return None;
    }
    let kind = operation.kind;
    Some(format!("cancel {}", kind.label().to_lowercase()))
}

/// The footer names the next-attention key only while something is actually
/// waiting, and carries the same badge as the tabs, so the hint is a signal
/// as well as a reminder of the key.
fn attention_footer(dashboard: &DashboardState) -> Option<String> {
    let (level, count) = dashboard.attention_badge_summary()?;
    Some(format!(
        "next ({}{count})",
        crate::render::sessions::attention_glyph(level)
    ))
}

/// The selected session (or, from the composer, the conversation's) must
/// have sub-agents to show. With none, the command stays listed and says why,
/// so a search for "sub-agent" always finds it.
fn subagents_available(dashboard: &DashboardState) -> Availability {
    let Some(session) = dashboard.command_session_id() else {
        return Availability::Hidden;
    };
    if dashboard.subagent_count_for(session) > 0 {
        Availability::Ready
    } else {
        Availability::Blocked("this session has no sub-agents")
    }
}

/// Named in the footer only while there are sub-agents to open.
fn subagents_footer(dashboard: &DashboardState) -> Option<String> {
    (subagents_available(dashboard) == Availability::Ready).then(|| "sub-agents".to_owned())
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
        id: CommandId::ChangeGoSetup,
        label: "Change fast-start setup",
        description: "Choose and remember a new account, target, or isolation for this project.",
        scope: Scope::Settings,
        pane_keys: &[],
        action: Some(KeyAction::ChangeGoSetup),
        footer: no_footer,
        footer_group: FooterGroup::Pane,
        footer_rank: 0,
        available: |dashboard| {
            if dashboard.go.is_some() {
                Availability::Ready
            } else {
                Availability::Hidden
            }
        },
    },
    CommandSpec {
        id: CommandId::OpenSession,
        label: "Open session",
        description: "Show the selected session's conversation and type in it.",
        scope: Scope::Sessions,
        pane_keys: &[KeyHint::plain(KeyCode::Enter, "Enter")],
        action: None,
        footer: footer_word!("open"),
        footer_group: FooterGroup::Pane,
        footer_rank: 0,
        available: selected_session_ready,
    },
    CommandSpec {
        id: CommandId::PinSession,
        label: "Pin session…",
        description: "Keep a session visible while browsing others.",
        scope: Scope::Session,
        pane_keys: &[],
        action: None,
        footer: no_footer,
        footer_group: FooterGroup::Pane,
        footer_rank: 0,
        available: pin_session_available,
    },
    CommandSpec {
        id: CommandId::UnpinSession,
        label: "Unpin session",
        description: "Leave this pane empty without stopping its session.",
        scope: Scope::Session,
        pane_keys: &[],
        action: None,
        footer: no_footer,
        footer_group: FooterGroup::Pane,
        footer_rank: 0,
        available: unpin_session_available,
    },
    CommandSpec {
        id: CommandId::OpenSessionSplitRight,
        label: "Split right",
        description: "Split this pane right; keep the old Browse session pinned and browse in the new pane.",
        scope: Scope::Pane,
        pane_keys: &[],
        action: Some(KeyAction::SplitVertical),
        footer: no_footer,
        footer_group: FooterGroup::Pane,
        footer_rank: 0,
        available: conversation_pane_ready,
    },
    CommandSpec {
        id: CommandId::OpenSessionSplitBelow,
        label: "Split down",
        description: "Split this pane down; keep the old Browse session pinned and browse in the new pane.",
        scope: Scope::Pane,
        pane_keys: &[],
        action: Some(KeyAction::SplitHorizontal),
        footer: no_footer,
        footer_group: FooterGroup::Pane,
        footer_rank: 0,
        available: conversation_pane_ready,
    },
    CommandSpec {
        id: CommandId::ClosePane,
        label: "Close pane",
        description: "Remove the conversation pane you are in; the last one is emptied instead.",
        scope: Scope::Pane,
        pane_keys: &[],
        action: Some(KeyAction::ClosePane),
        footer: no_footer,
        footer_group: FooterGroup::Pane,
        footer_rank: 0,
        available: conversation_pane_ready,
    },
    CommandSpec {
        id: CommandId::FocusPaneLeft,
        label: "Focus pane left",
        description: "Move the keyboard to the conversation pane left this one.",
        scope: Scope::Pane,
        pane_keys: &[],
        action: Some(KeyAction::FocusPaneLeft),
        footer: no_footer,
        footer_group: FooterGroup::Pane,
        footer_rank: 0,
        available: conversation_pane_ready,
    },
    CommandSpec {
        id: CommandId::FocusPaneDown,
        label: "Focus pane down",
        description: "Move the keyboard to the conversation pane below this one.",
        scope: Scope::Pane,
        pane_keys: &[],
        action: Some(KeyAction::FocusPaneDown),
        footer: no_footer,
        footer_group: FooterGroup::Pane,
        footer_rank: 0,
        available: conversation_pane_ready,
    },
    CommandSpec {
        id: CommandId::FocusPaneUp,
        label: "Focus pane up",
        description: "Move the keyboard to the conversation pane above this one.",
        scope: Scope::Pane,
        pane_keys: &[],
        action: Some(KeyAction::FocusPaneUp),
        footer: no_footer,
        footer_group: FooterGroup::Pane,
        footer_rank: 0,
        available: conversation_pane_ready,
    },
    CommandSpec {
        id: CommandId::FocusPaneRight,
        label: "Focus pane right",
        description: "Move the keyboard to the conversation pane right this one.",
        scope: Scope::Pane,
        pane_keys: &[],
        action: Some(KeyAction::FocusPaneRight),
        footer: no_footer,
        footer_group: FooterGroup::Pane,
        footer_rank: 0,
        available: conversation_pane_ready,
    },
    CommandSpec {
        id: CommandId::FocusLastPane,
        label: "Focus last pane",
        description: "Move the keyboard back to the conversation pane it was in before.",
        scope: Scope::Pane,
        pane_keys: &[],
        action: Some(KeyAction::LastPane),
        footer: no_footer,
        footer_group: FooterGroup::Pane,
        footer_rank: 0,
        available: conversation_pane_ready,
    },
    CommandSpec {
        id: CommandId::ZoomPane,
        label: "Zoom pane",
        description: "Fill the conversation area with the pane you are in, or put the others back.",
        scope: Scope::Pane,
        pane_keys: &[],
        action: Some(KeyAction::Zoom),
        footer: no_footer,
        footer_group: FooterGroup::Pane,
        footer_rank: 0,
        available: conversation_pane_ready,
    },
    CommandSpec {
        id: CommandId::ResizeMode,
        label: "Resize panes",
        description: "Resize conversation panes with h/j/k/l or arrows until Escape.",
        scope: Scope::Pane,
        pane_keys: &[],
        action: Some(KeyAction::ResizeMode),
        footer: no_footer,
        footer_group: FooterGroup::Chord,
        footer_rank: 0,
        available: conversation_pane_ready,
    },
    CommandSpec {
        id: CommandId::SwapPaneLeft,
        label: "Swap pane left",
        description: "Move the focused conversation pane left, exchanging places with its neighbor.",
        scope: Scope::Pane,
        pane_keys: &[],
        action: Some(KeyAction::SwapPaneLeft),
        footer: no_footer,
        footer_group: FooterGroup::Chord,
        footer_rank: 0,
        available: conversation_pane_ready,
    },
    CommandSpec {
        id: CommandId::SwapPaneDown,
        label: "Swap pane down",
        description: "Move the focused conversation pane down, exchanging places with its neighbor.",
        scope: Scope::Pane,
        pane_keys: &[],
        action: Some(KeyAction::SwapPaneDown),
        footer: no_footer,
        footer_group: FooterGroup::Chord,
        footer_rank: 0,
        available: conversation_pane_ready,
    },
    CommandSpec {
        id: CommandId::SwapPaneUp,
        label: "Swap pane up",
        description: "Move the focused conversation pane up, exchanging places with its neighbor.",
        scope: Scope::Pane,
        pane_keys: &[],
        action: Some(KeyAction::SwapPaneUp),
        footer: no_footer,
        footer_group: FooterGroup::Chord,
        footer_rank: 0,
        available: conversation_pane_ready,
    },
    CommandSpec {
        id: CommandId::SwapPaneRight,
        label: "Swap pane right",
        description: "Move the focused conversation pane right, exchanging places with its neighbor.",
        scope: Scope::Pane,
        pane_keys: &[],
        action: Some(KeyAction::SwapPaneRight),
        footer: no_footer,
        footer_group: FooterGroup::Chord,
        footer_rank: 0,
        available: conversation_pane_ready,
    },
    CommandSpec {
        id: CommandId::SuspendSession,
        label: "Suspend session…",
        description: "Save a recovery copy and release the environment. Resume this session later.",
        scope: Scope::Session,
        pane_keys: &[],
        action: Some(KeyAction::SuspendSession),
        footer: no_footer,
        footer_group: FooterGroup::Chord,
        footer_rank: 0,
        available: suspend_session_available,
    },
    CommandSpec {
        id: CommandId::RenameWorkspace,
        label: "Rename workspace…",
        description: "Rename the active workspace.",
        scope: Scope::Global,
        pane_keys: &[],
        action: Some(KeyAction::RenameWorkspace),
        footer: no_footer,
        footer_group: FooterGroup::Chord,
        footer_rank: 0,
        available: active_workspace_ready,
    },
    CommandSpec {
        id: CommandId::CloseWorkspace,
        label: "Close workspace…",
        description: "Confirm stopping workspace sessions, preserving history, and discarding drafts.",
        scope: Scope::Global,
        pane_keys: &[],
        action: Some(KeyAction::CloseWorkspace),
        footer: no_footer,
        footer_group: FooterGroup::Chord,
        footer_rank: 0,
        available: active_workspace_ready,
    },
    CommandSpec {
        id: CommandId::ResizePaneLeft,
        label: "Resize pane left",
        description: "Move the border of the conversation pane you are in left one step.",
        scope: Scope::Pane,
        pane_keys: &[],
        action: Some(KeyAction::ResizePaneLeft),
        footer: no_footer,
        footer_group: FooterGroup::Pane,
        footer_rank: 0,
        available: conversation_pane_ready,
    },
    CommandSpec {
        id: CommandId::ResizePaneDown,
        label: "Resize pane down",
        description: "Move the border of the conversation pane you are in down one step.",
        scope: Scope::Pane,
        pane_keys: &[],
        action: Some(KeyAction::ResizePaneDown),
        footer: no_footer,
        footer_group: FooterGroup::Pane,
        footer_rank: 0,
        available: conversation_pane_ready,
    },
    CommandSpec {
        id: CommandId::ResizePaneUp,
        label: "Resize pane up",
        description: "Move the border of the conversation pane you are in up one step.",
        scope: Scope::Pane,
        pane_keys: &[],
        action: Some(KeyAction::ResizePaneUp),
        footer: no_footer,
        footer_group: FooterGroup::Pane,
        footer_rank: 0,
        available: conversation_pane_ready,
    },
    CommandSpec {
        id: CommandId::ResizePaneRight,
        label: "Resize pane right",
        description: "Move the border of the conversation pane you are in right one step.",
        scope: Scope::Pane,
        pane_keys: &[],
        action: Some(KeyAction::ResizePaneRight),
        footer: no_footer,
        footer_group: FooterGroup::Pane,
        footer_rank: 0,
        available: conversation_pane_ready,
    },
    CommandSpec {
        id: CommandId::NewSessionWizard,
        label: "Create session",
        description: "Choose the profile, project, target, and mounts in the full wizard.",
        scope: Scope::Sessions,
        pane_keys: &[
            KeyHint::plain(KeyCode::Char('n'), "n"),
            KeyHint::plain(KeyCode::Char('N'), "N"),
        ],
        action: Some(KeyAction::NewSession),
        footer: footer_word!("create"),
        footer_group: FooterGroup::Chord,
        footer_rank: 0,
        available: config_present,
    },
    CommandSpec {
        id: CommandId::RestartSession,
        label: "Restart session",
        description: "Restart with the same profile, target, and mounts, without confirmation.",
        scope: Scope::Session,
        // No pane key: a mis-hit must not restart a live session. Reachable
        // from the palette, the row menu, and an explicit `[keys]` binding.
        pane_keys: &[],
        action: Some(KeyAction::RestartSession),
        footer: no_footer,
        footer_group: FooterGroup::Pane,
        footer_rank: 2,
        available: restart_session_available,
    },
    CommandSpec {
        id: CommandId::ResumeDialog,
        label: "Sessions",
        description: "Open every running session, in every workspace, on one list. \
                      The tabs to its right list the sessions that can be resumed, \
                      imported, or restored from the search index.",
        scope: Scope::Global,
        pane_keys: &[],
        action: Some(KeyAction::Resume),
        footer: footer_word!("sessions"),
        footer_group: FooterGroup::Chord,
        footer_rank: 1,
        available: always_ready,
    },
    CommandSpec {
        id: CommandId::MarkAllRead,
        label: "Mark all read",
        description: "Clear the unread marker on every session in this workspace at once; questions, failures, and unreachable sessions stay flagged.",
        scope: Scope::Sessions,
        pane_keys: &[],
        action: Some(KeyAction::MarkAllRead),
        footer: footer_word!("read"),
        footer_group: FooterGroup::Chord,
        footer_rank: 2,
        available: always_ready,
    },
    CommandSpec {
        id: CommandId::FilterSessions,
        label: "Search sessions",
        description: "Type to keep only the sessions whose name, project, profile, or target matches. The letters b, w, i, d, and a keep only blocked, working, idle, or done sessions, or all of them.",
        scope: Scope::Sessions,
        pane_keys: &[KeyHint::plain(KeyCode::Char('/'), "/")],
        action: None,
        footer: footer_word!("search (filter a/b/w/i/d)"),
        footer_group: FooterGroup::Pane,
        footer_rank: 0,
        available: always_ready,
    },
    CommandSpec {
        id: CommandId::NextAttention,
        label: "Next session needing you",
        description: "Jump to the session that most needs a person: a question first, then a failure, then unread activity, in any workspace.",
        scope: Scope::Global,
        pane_keys: &[],
        action: Some(KeyAction::NextAttention),
        footer: attention_footer,
        footer_group: FooterGroup::Chord,
        footer_rank: 3,
        available: always_ready,
    },
    CommandSpec {
        id: CommandId::PreviousAttention,
        label: "Previous session needing you",
        description: "Walk the sessions that need a person in the other direction.",
        scope: Scope::Global,
        pane_keys: &[],
        action: Some(KeyAction::PreviousAttention),
        footer: no_footer,
        footer_group: FooterGroup::Chord,
        footer_rank: 3,
        available: always_ready,
    },
    CommandSpec {
        id: CommandId::CancelOperation,
        label: "Cancel operation",
        description: "Stop the launch, resume, or stop the selected session is in the middle of.",
        scope: Scope::Global,
        pane_keys: &[],
        action: Some(KeyAction::CancelOperation),
        footer: cancel_footer,
        footer_group: FooterGroup::Chord,
        footer_rank: 5,
        available: operation_in_flight,
    },
    CommandSpec {
        id: CommandId::ToggleProject,
        label: "Fold project",
        description: "Space folds the selected session's project; 1 to 9 fold by number.",
        scope: Scope::Sessions,
        pane_keys: &[KeyHint::plain(KeyCode::Char(' '), "Space")],
        action: None,
        footer: no_footer,
        footer_group: FooterGroup::Pane,
        footer_rank: 0,
        available: selected_session_ready,
    },
    CommandSpec {
        id: CommandId::RenameSession,
        label: "Rename session",
        description: "Give the selected session your own title.",
        scope: Scope::Session,
        pane_keys: &[],
        action: Some(KeyAction::RenameSession),
        footer: no_footer,
        footer_group: FooterGroup::Pane,
        footer_rank: 0,
        available: session_idle,
    },
    CommandSpec {
        id: CommandId::ChangedFiles,
        label: "Changed files",
        description: "List the files the selected session's checkout has changed, with the branch and its distance from upstream.",
        scope: Scope::Session,
        pane_keys: &[],
        action: Some(KeyAction::ChangedFiles),
        footer: no_footer,
        footer_group: FooterGroup::Chord,
        footer_rank: 0,
        available: live_session,
    },
    CommandSpec {
        id: CommandId::OpenSubagents,
        label: "Sub-agents",
        description: "Open the sub-agents of this session in their own list. Esc or the X on the workspace strip returns to the parent.",
        scope: Scope::Session,
        pane_keys: &[],
        action: Some(KeyAction::OpenSubagents),
        footer: subagents_footer,
        footer_group: FooterGroup::Chord,
        footer_rank: 2,
        available: subagents_available,
    },
    CommandSpec {
        id: CommandId::ContainerSettings,
        label: "Container settings",
        description: "Edit CPU, memory, and mounts for the next time the container is created.",
        scope: Scope::Session,
        pane_keys: &[],
        action: Some(KeyAction::ContainerSettings),
        footer: no_footer,
        footer_group: FooterGroup::Pane,
        footer_rank: 0,
        available: container_session,
    },
    CommandSpec {
        id: CommandId::MoveSession,
        label: "Move session…",
        description: "Restore the selected session on another profile and/or target.",
        scope: Scope::Session,
        pane_keys: &[],
        action: Some(KeyAction::MoveSession),
        footer: no_footer,
        footer_group: FooterGroup::Pane,
        footer_rank: 0,
        available: move_session_available,
    },
    CommandSpec {
        id: CommandId::DestroySession,
        label: "Destroy session…",
        description: "Permanently remove the selected session, its target, and its recovery archive.",
        scope: Scope::Session,
        // No pane key: a mis-hit must not begin deleting a session.
        pane_keys: &[],
        action: Some(KeyAction::DestroySession),
        footer: no_footer,
        footer_group: FooterGroup::Pane,
        footer_rank: 0,
        // Deliberately available while an operation runs: preempting a wedged
        // one is what force destruction is for.
        available: selected_session_ready,
    },
    CommandSpec {
        id: CommandId::TargetActions,
        label: "Target actions",
        description: "Test or rename the selected target.",
        scope: Scope::Targets,
        pane_keys: &[
            KeyHint::plain(KeyCode::Enter, "Enter"),
            KeyHint::plain(KeyCode::Char('e'), "e"),
        ],
        action: None,
        footer: footer_word!("actions"),
        footer_group: FooterGroup::Pane,
        footer_rank: 0,
        available: targets_visible,
    },
    CommandSpec {
        id: CommandId::EditProfile,
        label: "Rename profile",
        description: "Rename the selected profile's configuration id.",
        scope: Scope::Quota,
        pane_keys: &[
            KeyHint::plain(KeyCode::Enter, "Enter"),
            KeyHint::plain(KeyCode::Char('e'), "e"),
        ],
        action: None,
        footer: footer_word!("edit profile"),
        footer_group: FooterGroup::Pane,
        footer_rank: 0,
        available: profiles_present,
    },
    CommandSpec {
        id: CommandId::ManageProfiles,
        label: "Manage agent profiles",
        description: "Add or edit agent accounts, harnesses, and environment settings.",
        scope: Scope::Settings,
        pane_keys: &[],
        action: Some(KeyAction::ManageProfiles),
        footer: no_footer,
        footer_group: FooterGroup::Pane,
        footer_rank: 0,
        available: always_ready,
    },
    CommandSpec {
        id: CommandId::ManageMachines,
        label: "Manage machines",
        description: "Add SSH hosts or EC2 launch templates and edit their shared settings.",
        scope: Scope::Settings,
        pane_keys: &[],
        // Unbound by default like its siblings; `[keys] manage_machines`
        // binds it.
        action: Some(KeyAction::ManageMachines),
        footer: no_footer,
        footer_group: FooterGroup::Pane,
        footer_rank: 0,
        available: always_ready,
    },
    CommandSpec {
        id: CommandId::ManageTargets,
        label: "Manage runtimes",
        description: "Add a runtime (the engine or directory a target runs in) and choose the machine it runs on.",
        scope: Scope::Settings,
        pane_keys: &[],
        action: Some(KeyAction::ManageTargets),
        footer: no_footer,
        footer_group: FooterGroup::Pane,
        footer_rank: 0,
        available: always_ready,
    },
    CommandSpec {
        id: CommandId::OpenConfig,
        label: "Open settings",
        description: "Edit all configuration in the Settings modal.",
        scope: Scope::Settings,
        pane_keys: &[],
        action: Some(KeyAction::OpenSettings),
        footer: footer_word!("settings"),
        footer_group: FooterGroup::Chord,
        footer_rank: 9,
        available: always_ready,
    },
    CommandSpec {
        id: CommandId::CycleFocus,
        label: "Next pane",
        description: "Move the keyboard down the layout; Shift-Tab reverses it.",
        scope: Scope::Pane,
        pane_keys: &[KeyHint::plain(KeyCode::Tab, "Tab")],
        action: Some(KeyAction::NextPane),
        footer: footer_word!("pane"),
        footer_group: FooterGroup::Pane,
        footer_rank: 1,
        available: always_ready,
    },
    CommandSpec {
        id: CommandId::CycleFocusReverse,
        label: "Previous pane",
        description: "Move the keyboard up the layout.",
        scope: Scope::Pane,
        pane_keys: &[],
        action: Some(KeyAction::PreviousPane),
        footer: no_footer,
        footer_group: FooterGroup::Pane,
        footer_rank: 1,
        available: always_ready,
    },
    CommandSpec {
        id: CommandId::CycleFocusedPaneSize,
        label: "Pane size",
        description: "Cycle the focused pane through minimized, standard, and maximized.",
        scope: Scope::Pane,
        pane_keys: &[],
        action: Some(KeyAction::PaneSize),
        // The pane title carries clickable size controls in the place they
        // apply, so the footer spends its width on keys with no such affordance.
        footer: no_footer,
        footer_group: FooterGroup::Chord,
        footer_rank: 3,
        available: support_pane_focused,
    },
    CommandSpec {
        id: CommandId::TogglePanePreset,
        label: "Pane preset",
        description: "Restore standard panes, or minimize all three for the conversation.",
        scope: Scope::Pane,
        pane_keys: &[],
        action: Some(KeyAction::PanePreset),
        footer: footer_word!("panes"),
        footer_group: FooterGroup::Chord,
        footer_rank: 4,
        available: always_ready,
    },
    CommandSpec {
        id: CommandId::Workspaces,
        label: "Workspaces",
        description: "Open the workspace manager to add, rename, or delete a workspace.",
        scope: Scope::Global,
        // Workspace management is opened by the pinned hamburger, its key, or
        // the command palette. It intentionally has no footer hint.
        pane_keys: &[],
        action: Some(KeyAction::WorkspaceManager),
        footer: no_footer,
        footer_group: FooterGroup::Pane,
        footer_rank: 0,
        available: always_ready,
    },
    CommandSpec {
        id: CommandId::FocusWorkspaces,
        label: "Focus the workspace strip",
        description: "Put the keyboard on the workspace tabs.",
        scope: Scope::Global,
        pane_keys: &[],
        action: Some(KeyAction::FocusWorkspaces),
        footer: no_footer,
        footer_group: FooterGroup::Pane,
        footer_rank: 0,
        available: always_ready,
    },
    CommandSpec {
        id: CommandId::SelectWorkspacePrevious,
        label: "Previous workspace",
        description: "Select the previous workspace tab.",
        scope: Scope::Global,
        pane_keys: &[],
        action: Some(KeyAction::PreviousWorkspace),
        footer: no_footer,
        footer_group: FooterGroup::Pane,
        footer_rank: 0,
        available: always_ready,
    },
    CommandSpec {
        id: CommandId::SelectWorkspaceNext,
        label: "Next workspace",
        description: "Select the next workspace tab.",
        scope: Scope::Global,
        pane_keys: &[],
        action: Some(KeyAction::NextWorkspace),
        footer: no_footer,
        footer_group: FooterGroup::Pane,
        footer_rank: 0,
        available: always_ready,
    },
    CommandSpec {
        id: CommandId::SwitchWorkspace,
        label: "Switch to workspace by number",
        description: "Select the first to ninth workspace tab directly.",
        scope: Scope::Global,
        pane_keys: &[],
        action: Some(KeyAction::SwitchWorkspace),
        footer: no_footer,
        footer_group: FooterGroup::Pane,
        footer_rank: 0,
        available: always_ready,
    },
    CommandSpec {
        id: CommandId::WebViewer,
        label: "Web viewer",
        description: "Show the address and code for the browser and phone viewer.",
        scope: Scope::Global,
        pane_keys: &[],
        action: Some(KeyAction::WebViewer),
        footer: footer_word!("web"),
        footer_group: FooterGroup::Chord,
        footer_rank: 7,
        available: always_ready,
    },
    CommandSpec {
        id: CommandId::RestartDaemon,
        label: "Restart the Mjolnir daemon",
        description: "Stop the background daemon and start one from this build, then report which build came up.",
        scope: Scope::Global,
        // Restarting the daemon is rare and disruptive, so it has no default
        // key; `[keys] restart_daemon` binds one. It is always offered: the
        // daemon being gone is exactly when it is needed, and that is also
        // when nothing can be asked.
        pane_keys: &[],
        action: Some(KeyAction::RestartDaemon),
        footer: no_footer,
        footer_group: FooterGroup::Chord,
        footer_rank: 0,
        available: always_ready,
    },
    CommandSpec {
        id: CommandId::Refresh,
        label: "Refresh targets and quotas",
        description: "Re-probe every target's capacity and ask every profile for its quota again.",
        scope: Scope::Global,
        pane_keys: &[],
        action: Some(KeyAction::Refresh),
        footer: footer_word!("refresh"),
        footer_group: FooterGroup::Chord,
        footer_rank: 8,
        available: always_ready,
    },
    CommandSpec {
        id: CommandId::NoticeLog,
        label: "Recent messages",
        description: "Show the last notices the footer reported, newest first, including failures that replaced each other.",
        scope: Scope::Global,
        pane_keys: &[],
        action: Some(KeyAction::NoticeLog),
        footer: no_footer,
        footer_group: FooterGroup::Chord,
        footer_rank: 0,
        available: always_ready,
    },
    CommandSpec {
        id: CommandId::QuitDetach,
        label: "Detach from this terminal",
        description: "Leave this terminal client; the daemon and its sessions keep running.",
        scope: Scope::Global,
        pane_keys: &[],
        action: Some(KeyAction::Detach),
        footer: footer_word!("detach"),
        footer_group: FooterGroup::Chord,
        footer_rank: 6,
        available: always_ready,
    },
    CommandSpec {
        id: CommandId::ToggleTranscriptRendering,
        label: "Toggle transcript rendering",
        description: "Switch the conversation between rendered Markdown and raw text.",
        scope: Scope::Global,
        pane_keys: &[],
        action: Some(KeyAction::ToggleTranscriptRendering),
        footer: footer_word!("rendering"),
        footer_group: FooterGroup::Chord,
        footer_rank: 10,
        available: always_ready,
    },
    CommandSpec {
        id: CommandId::ToggleDictation,
        label: "Dictation",
        description: "Start or stop dictating into the composer.",
        scope: Scope::Global,
        pane_keys: &[],
        action: Some(KeyAction::ToggleDictation),
        footer: no_footer,
        footer_group: FooterGroup::Chord,
        footer_rank: 13,
        available: always_ready,
    },
    CommandSpec {
        id: CommandId::Palette,
        label: "Command palette",
        description: "Search every command that applies right now and run one.",
        scope: Scope::Global,
        pane_keys: &[],
        action: Some(KeyAction::Palette),
        footer: footer_word!("palette"),
        footer_group: FooterGroup::Chord,
        footer_rank: 11,
        available: always_ready,
    },
    CommandSpec {
        id: CommandId::CycleSpinner,
        label: "Next spinner style",
        description: "Cycle activity animations: scan, pulse, wave, bars, shimmer, globe.",
        scope: Scope::Settings,
        pane_keys: &[],
        action: Some(KeyAction::CycleSpinner),
        footer: no_footer,
        footer_group: FooterGroup::Pane,
        footer_rank: 0,
        available: spinner_available,
    },
    CommandSpec {
        id: CommandId::Help,
        label: "Help",
        description: "List every key this surface answers.",
        scope: Scope::Global,
        pane_keys: &[KeyHint::plain(KeyCode::Char('?'), "?")],
        action: Some(KeyAction::Help),
        footer: footer_word!("keys"),
        footer_group: FooterGroup::Chord,
        footer_rank: 12,
        available: always_ready,
    },
];

/// The commands the palette omits because a visible control already runs them.
///
/// Each of these is one click away on the dashboard itself, so listing them
/// again in the palette only lengthens the search. The keyboard chords, footer
/// hints, and onboarding buttons that run them are unaffected.
const PALETTE_HIDDEN: &[CommandId] = &[
    CommandId::Palette,         // already open when the list is drawn
    CommandId::SwitchWorkspace, // the numbered keys act on the visible tab strip
];

/// How many commands the palette remembers under its Recent heading.
pub(crate) const RECENT_COMMANDS: usize = 5;

/// Whether [`palette_entries`](crate::palette::palette_entries) skips `id`.
pub(crate) fn hidden_from_palette(id: CommandId) -> bool {
    PALETTE_HIDDEN.contains(&id)
}

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
/// `Setup` is deliberately excluded from key matching (see
/// [`pane_command_for_key`]);
/// it is listed here so the footer can offer it while the configuration is
/// still empty.
fn scope_applies(scope: Scope, focus: Focus) -> bool {
    match scope {
        Scope::Global | Scope::Settings | Scope::Pane | Scope::Setup => true,
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
pub(crate) fn pane_command_for_key(key: KeyEvent, focus: Focus) -> Option<CommandId> {
    let plain = !key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER);
    COMMANDS
        .iter()
        .find(|spec| {
            spec.scope != Scope::Setup
                && scope_applies(spec.scope, focus)
                && spec.pane_keys.iter().any(|hint| {
                    // At the composer a bare letter is text, so no pane
                    // character key reaches the panes from there.
                    if focus == Focus::Prompt && matches!(hint.code, KeyCode::Char(_)) {
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
        .filter(|spec| {
            !(dashboard
                .command_session_id()
                .is_some_and(|id| dashboard.is_native_agent(id))
                && matches!(
                    spec.id,
                    CommandId::RestartSession
                        | CommandId::RenameSession
                        | CommandId::ContainerSettings
                        | CommandId::ChangedFiles
                        | CommandId::MoveSession
                        | CommandId::DestroySession
                ))
        })
        .filter(|spec| (spec.available)(dashboard) == Availability::Ready)
        .map(|spec| spec.id)
        .collect()
}

impl DashboardState {
    /// Whether a bound command still answers with the current dialog open.
    ///
    /// Help and detach have always answered from every surface, and the pane
    /// preset only changes the layout underneath, so all three survive a modal.
    /// The rest would act on a surface the user cannot see, so they wait for
    /// the dialog to close — except over the help overlay, which is a
    /// reference rather than a decision.
    pub fn command_allowed_now(&self, id: CommandId) -> bool {
        match id {
            // Refreshing is harmless over a modal: it asks the daemon for
            // fresh capacity and quota figures and changes nothing on screen
            // the dialog owns.
            CommandId::Help
            | CommandId::QuitDetach
            | CommandId::TogglePanePreset
            | CommandId::Refresh => true,
            // Cancel is allowed through exactly one modal: the target-actions
            // dialog, where it cancels the test that dialog is running.
            CommandId::CancelOperation => self.target_test_running() || !self.modal_open(),
            CommandId::CycleFocusedPaneSize => !self.modal_open(),
            // Moving the keyboard or the workspace out from under an open
            // dialog would act on a surface the user cannot see.
            CommandId::CycleFocus
            | CommandId::CycleFocusReverse
            | CommandId::FocusWorkspaces
            | CommandId::SelectWorkspacePrevious
            | CommandId::SelectWorkspaceNext
            | CommandId::SwitchWorkspace => !self.modal_open(),
            _ => !self.modal_open() || matches!(self.mode, crate::Mode::Help(_)),
        }
    }

    /// Runs one registry command. Every arm calls the same entry point the
    /// key handler used to call directly, so the footer, the help overlay, and
    /// the keyboard cannot disagree about what a command does.
    pub fn dispatch_command(&mut self, id: CommandId) -> DashboardAction {
        let saved = self.command_session_override.clone();
        if spec(id).scope == Scope::Session && saved.is_none() && self.focus == Focus::Prompt {
            let Some(session) = self.current_session_id().map(str::to_owned) else {
                return DashboardAction::None;
            };
            self.command_session_override = Some(session);
        }
        let action = self.dispatch_command_inner(id);
        self.command_session_override = saved;
        action
    }

    /// Puts a command at the head of the palette's Recent group.
    pub(crate) fn remember_command(&mut self, id: CommandId) {
        self.recent_commands.retain(|recent| *recent != id);
        self.recent_commands.push_front(id);
        self.recent_commands.truncate(RECENT_COMMANDS);
    }

    fn dispatch_command_inner(&mut self, id: CommandId) -> DashboardAction {
        if !matches!(
            id,
            CommandId::ResizePaneLeft
                | CommandId::ResizePaneDown
                | CommandId::ResizePaneUp
                | CommandId::ResizePaneRight
        ) {
            self.resize_mode = false;
        }
        if self
            .command_session_id()
            .is_some_and(|selected| self.is_native_agent(selected))
            && matches!(
                id,
                CommandId::RestartSession
                    | CommandId::RenameSession
                    | CommandId::ContainerSettings
                    | CommandId::ChangedFiles
                    | CommandId::MoveSession
                    | CommandId::SuspendSession
                    | CommandId::DestroySession
            )
        {
            self.set_notice("Native agents are owned by their parent session");
            return DashboardAction::None;
        }
        // Commands a person reaches for by name are worth remembering; the
        // pane keys and the palette itself are not. The palette records
        // whatever it runs itself, pane keys included.
        if spec(id).pane_keys.is_empty() && !matches!(id, CommandId::Palette | CommandId::Help) {
            self.remember_command(id);
        }
        if matches!(id, CommandId::SuspendSession | CommandId::RestartSession) {
            match (spec(id).available)(self) {
                Availability::Hidden => return DashboardAction::None,
                Availability::Blocked(reason) => {
                    self.set_notice(reason);
                    return DashboardAction::None;
                }
                Availability::Ready => {}
            }
        }
        match id {
            CommandId::PinSession => {
                if let Some(session) = self.command_session_id().map(str::to_owned) {
                    self.begin_pin_menu(session);
                }
                DashboardAction::None
            }
            CommandId::UnpinSession => self
                .command_session_id()
                .map(str::to_owned)
                .map_or(DashboardAction::None, |session_id| {
                    DashboardAction::UnpinSession { session_id }
                }),
            CommandId::OpenSession => self.open_selected_session(),
            CommandId::OpenSessionSplitRight => {
                self.split_command(ratatui::layout::Direction::Horizontal)
            }
            CommandId::OpenSessionSplitBelow => {
                self.split_command(ratatui::layout::Direction::Vertical)
            }
            CommandId::ClosePane => DashboardAction::ClosePane {
                pane: self.focused_pane(),
            },
            CommandId::FocusPaneLeft => self.focus_pane_command(NavDirection::Left),
            CommandId::FocusPaneDown => self.focus_pane_command(NavDirection::Down),
            CommandId::FocusPaneUp => self.focus_pane_command(NavDirection::Up),
            CommandId::FocusPaneRight => self.focus_pane_command(NavDirection::Right),
            CommandId::FocusLastPane => self.focus_last_pane_command(),
            CommandId::ZoomPane => self.zoom_pane_command(),
            CommandId::ResizeMode => self.begin_resize_mode(),
            CommandId::SwapPaneLeft => self.swap_pane_command(NavDirection::Left),
            CommandId::SwapPaneDown => self.swap_pane_command(NavDirection::Down),
            CommandId::SwapPaneUp => self.swap_pane_command(NavDirection::Up),
            CommandId::SwapPaneRight => self.swap_pane_command(NavDirection::Right),
            CommandId::RenameWorkspace => self.begin_workspace_command(false),
            CommandId::CloseWorkspace => self.begin_workspace_command(true),
            CommandId::SuspendSession => {
                if let Some(session) = self.selected_session() {
                    let active_children = self
                        .state
                        .subagents
                        .values()
                        .filter(|r| r.parent_session_id == session.id)
                        .filter(|r| {
                            self.state
                                .sessions
                                .get(&r.child_session_id)
                                .is_some_and(|s| s.state.is_active())
                        })
                        .count();
                    let interrupting =
                        self.attention_level(&session.id) == crate::AttentionLevel::Working;
                    if !interrupting && active_children == 0 {
                        return DashboardAction::Suspend {
                            session_id: session.id.clone(),
                        };
                    }
                    self.mode = crate::Mode::Confirm(
                        ConfirmDialog::new(Confirmation::SuspendSession {
                            session_id: session.id.clone(),
                            active_children,
                            interrupting,
                        })
                        .naming_session(session.display_title()),
                    );
                }
                DashboardAction::None
            }
            CommandId::ResizePaneLeft => self.resize_pane_command(NavDirection::Left),
            CommandId::ResizePaneDown => self.resize_pane_command(NavDirection::Down),
            CommandId::ResizePaneUp => self.resize_pane_command(NavDirection::Up),
            CommandId::ResizePaneRight => self.resize_pane_command(NavDirection::Right),
            CommandId::NewSessionWizard => self.begin_new(),
            CommandId::ChangeGoSetup => self.change_go_setup(),
            CommandId::RestartSession => {
                let Some((session_id, name)) = self
                    .selected_session()
                    .map(|s| (s.id.clone(), s.display_title().to_owned()))
                else {
                    return DashboardAction::None;
                };
                // Mid-turn work is lost by a restart, so that case asks first;
                // an idle session restarts at once.
                if self.attention_level(&session_id) == crate::AttentionLevel::Working {
                    self.mode = crate::Mode::Confirm(
                        ConfirmDialog::new(Confirmation::InterruptWork {
                            session_id,
                            restart: true,
                        })
                        .naming_session(&name),
                    );
                    return DashboardAction::None;
                }
                DashboardAction::RestartSession { session_id }
            }
            CommandId::ResumeDialog => DashboardAction::OpenResumeDialog,
            CommandId::Palette => {
                self.begin_palette();
                DashboardAction::None
            }
            CommandId::CycleSpinner => {
                if self.spinner_save_pending {
                    return DashboardAction::None;
                }
                let style = self.config.spinner.next();
                self.spinner_save_pending = true;
                self.set_notice(format!("Saving {style} spinner…"));
                DashboardAction::SaveSpinnerStyle { style }
            }
            CommandId::RenameSession => {
                self.begin_rename();
                DashboardAction::None
            }
            CommandId::ContainerSettings => {
                self.begin_container_edit();
                DashboardAction::None
            }
            CommandId::ChangedFiles => self.begin_changed_files(),
            CommandId::OpenSubagents => match subagents_available(self) {
                Availability::Ready => self
                    .command_session_id()
                    .map(str::to_owned)
                    .map_or(DashboardAction::None, |parent_id| {
                        DashboardAction::OpenSubagents { parent_id }
                    }),
                Availability::Blocked(reason) => {
                    self.set_notice(crate::help::sentence(reason));
                    DashboardAction::None
                }
                Availability::Hidden => DashboardAction::None,
            },
            CommandId::MoveSession => self.begin_move(),
            CommandId::DestroySession => {
                let Some((session_id, name)) = self
                    .selected_session()
                    .map(|session| (session.id.clone(), session.display_title().to_owned()))
                else {
                    return DashboardAction::None;
                };
                self.mode = crate::Mode::Confirm(
                    ConfirmDialog::new(Confirmation::ForceDestroy { session_id })
                        .naming_session(&name),
                );
                DashboardAction::None
            }
            CommandId::MarkAllRead => self.mark_all_read(),
            CommandId::FilterSessions => {
                self.begin_sessions_filter();
                DashboardAction::None
            }
            CommandId::NextAttention => self.step_attention(1),
            CommandId::PreviousAttention => self.step_attention(-1),
            CommandId::CancelOperation => {
                // The target-actions dialog's running test is the one thing
                // cancel reaches through a modal.
                if let Some(action) = self.cancel_target_test() {
                    return action;
                }
                let operation = self.selected_session().and_then(|session| {
                    self.session_operation_kind(&session.id)
                        .map(|kind| (session.id.clone(), kind))
                });
                match operation {
                    Some((session_id, kind)) => {
                        DashboardAction::CancelOperation { session_id, kind }
                    }
                    None => {
                        // A key that does nothing silently reads as broken.
                        self.set_notice("Nothing to cancel");
                        DashboardAction::None
                    }
                }
            }
            CommandId::ToggleProject => {
                self.toggle_selected_project();
                DashboardAction::None
            }
            CommandId::TargetActions => {
                if self.pane_size(crate::SupportPane::Targets) == crate::PaneSize::Minimized {
                    return DashboardAction::None;
                }
                self.begin_target_actions();
                DashboardAction::None
            }
            CommandId::EditProfile => {
                if self.pane_size(crate::SupportPane::Quota) == crate::PaneSize::Minimized {
                    return DashboardAction::None;
                }
                self.begin_profile_rename();
                DashboardAction::None
            }
            CommandId::Refresh => {
                // Refreshing re-probes target readiness as well. It is allowed
                // through an open modal (see `command_allowed_now`), so a
                // wizard sitting on its target step sees fresh readiness. A
                // cleared entry is re-probed on the next render.
                self.target_readiness.clear();
                DashboardAction::RefreshAll
            }
            CommandId::ManageProfiles => {
                self.begin_settings_section("profiles", None);
                DashboardAction::None
            }
            CommandId::ManageMachines => {
                self.begin_settings_section("machines", None);
                DashboardAction::None
            }
            CommandId::ManageTargets => {
                self.begin_settings_section("targets", None);
                DashboardAction::None
            }
            CommandId::OpenConfig => {
                self.begin_setup();
                DashboardAction::None
            }
            CommandId::CycleFocus => {
                self.cycle_focus(false);
                DashboardAction::None
            }
            CommandId::CycleFocusReverse => {
                self.cycle_focus(true);
                DashboardAction::None
            }
            CommandId::CycleFocusedPaneSize => {
                self.cycle_focused_pane_size();
                DashboardAction::None
            }
            CommandId::TogglePanePreset => {
                self.toggle_pane_preset();
                DashboardAction::None
            }
            CommandId::Workspaces => self.begin_workspace_manager(),
            CommandId::FocusWorkspaces => {
                self.focus = Focus::Workspaces;
                self.workspace_control_focus = crate::workspaces::WorkspaceControlFocus::Tabs;
                self.set_session_action_focus(None);
                DashboardAction::None
            }
            // The numbered workspace keys carry an index, and a registry
            // command carries no argument, so the router calls
            // `select_workspace_index` directly.
            CommandId::SwitchWorkspace => DashboardAction::None,
            CommandId::ToggleTranscriptRendering => DashboardAction::ToggleTranscriptRendering,
            CommandId::ToggleDictation => DashboardAction::ToggleDictation,
            CommandId::SelectWorkspacePrevious => self.select_adjacent_workspace(-1),
            CommandId::SelectWorkspaceNext => self.select_adjacent_workspace(1),
            CommandId::WebViewer => self.open_web_dialog(),
            CommandId::RestartDaemon => DashboardAction::RestartDaemon,
            CommandId::NoticeLog => {
                self.begin_notice_log();
                DashboardAction::None
            }
            CommandId::QuitDetach => DashboardAction::QuitDetach,
            // Help toggles: the same key that opens the reference closes it
            // again, which is what the overlay's own Esc/F1/? arm does when
            // the key reaches it rather than the chord pre-filter.
            CommandId::Help => {
                if matches!(self.mode, crate::Mode::Help(_)) {
                    self.close_help();
                } else {
                    self.begin_help();
                }
                DashboardAction::None
            }
        }
    }

    /// Runs one command. A command always consumes its chord, whether or not
    /// it also hands the caller an action to execute.
    pub fn dispatch_command_result(&mut self, id: CommandId) -> EventResult<DashboardAction> {
        let action = self.dispatch_command(id);
        EventResult {
            consumed: true,
            action: (!matches!(&action, DashboardAction::None)).then_some(action),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionOperationKind;
    use crate::keybinds::command_for_action;
    use crate::test_support::{dashboard_with_session, key, operation, running_session};
    use mj_core::config::Keybinds;

    /// A mis-hit key once stopped a live session, so no command that starts a
    /// session transition may claim one, either as a pane key or as a default
    /// binding. They stay reachable from the palette, the row's ⋯ menu, and an
    /// explicit `[keys]` entry.
    #[test]
    fn session_transition_commands_bind_no_key() {
        let defaults = Keybinds::default();
        for id in [
            CommandId::RestartSession,
            CommandId::MoveSession,
            CommandId::DestroySession,
        ] {
            let spec = spec(id);
            assert!(spec.pane_keys.is_empty(), "{id:?} must not bind a pane key");
            let action = spec.action.expect("a session command is bindable");
            assert!(
                defaults.bindings(action).is_empty(),
                "{id:?} must not carry a default binding"
            );
        }
    }

    /// A close that failed part-way leaves a durable Closing/Destroying
    /// record. Stop is how the person retries it, so it stays available even
    /// though every other session command is blocked on the failure.
    #[test]
    fn stop_retries_a_close_that_failed_part_way() {
        let mut session = running_session();
        session.state = mj_core::state::SessionState::Destroying;
        session.last_error = Some("archive unavailable".into());
        let mut dashboard = dashboard_with_session(session);
        dashboard.focus_sessions();
        assert_eq!(
            (spec(CommandId::SuspendSession).available)(&dashboard),
            Availability::Ready
        );

        // An ordinary running session with an operation in flight is not a
        // failed close, and Stop stays blocked behind that operation.
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        dashboard.session_operations.insert(
            "session-1".into(),
            operation(SessionOperationKind::Launching, None),
        );
        assert!(matches!(
            (spec(CommandId::SuspendSession).available)(&dashboard),
            Availability::Blocked(_)
        ));
    }

    /// A-17: the Sub-agents pane opened only by a mouse click on the prompt's
    /// lower border. It is a registry command now, so the help overlay, the
    /// palette, a default chord, and the footer all reach it.
    #[test]
    fn sub_agents_open_from_a_chord_and_are_listed_for_help_and_the_palette() {
        let (mut dashboard, parent) = crate::test_support::dashboard_with_one_subagent();
        dashboard.focus_sessions();
        assert_eq!(
            dashboard.key_labels(CommandId::OpenSubagents),
            vec!["ctrl+b shift+a".to_owned()]
        );
        assert_eq!(
            (spec(CommandId::OpenSubagents).available)(&dashboard),
            Availability::Ready
        );
        assert!(!hidden_from_palette(CommandId::OpenSubagents));
        assert!(spec(CommandId::OpenSubagents).label.contains("Sub-agents"));
        let footer = crate::render::combined_footer_text(&dashboard, 400);
        assert!(footer.contains("shift+a sub-agents"), "{footer}");
        assert_eq!(
            dashboard.dispatch_command(CommandId::OpenSubagents),
            DashboardAction::OpenSubagents {
                parent_id: parent.clone()
            }
        );

        // Esc in the sub-agents' Sessions list goes back to the parent, as
        // the X on the workspace strip does.
        dashboard.open_subagent_workspace(parent);
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Esc)),
            DashboardAction::ExitSubagentWorkspace
        );

        // A session without sub-agents keeps the command listed, greyed with
        // the reason, so a search for it still finds it.
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        assert!(matches!(
            (spec(CommandId::OpenSubagents).available)(&dashboard),
            Availability::Blocked(_)
        ));
        let footer = crate::render::combined_footer_text(&dashboard, 400);
        assert!(!footer.contains("sub-agents"), "{footer}");
    }

    /// A-16 and A-18: the help text reads as sentences, and Close pane says
    /// the one pane it refuses to close.
    #[test]
    fn pane_help_text_reads_as_sentences_and_names_the_browse_exception() {
        assert!(
            spec(CommandId::FocusPaneLeft)
                .description
                .contains("pane left of this one")
        );
        assert!(
            spec(CommandId::FocusPaneRight)
                .description
                .contains("pane right of this one")
        );
        let close = spec(CommandId::ClosePane).description;
        assert!(
            close.contains("The Browse pane cannot be closed"),
            "{close}"
        );
        assert!(close.contains("Swap pane"), "{close}");
    }

    #[test]
    fn workspace_manager_has_a_prefix_key_and_no_palette_row() {
        let dashboard = dashboard_with_session(running_session());
        assert!(spec(CommandId::Workspaces).pane_keys.is_empty());
        assert_eq!(
            dashboard.key_labels(CommandId::Workspaces),
            vec!["ctrl+b shift+n".to_owned()]
        );
        // Still dispatchable: the pinned hamburger runs it through
        // `run_available_command`, which needs the command to stay available.
        assert!(available(&dashboard, None).contains(&CommandId::Workspaces));
        // Listed in the palette too: the button is one way in, but a person
        // who types "work" expects to find it.
        assert!(!hidden_from_palette(CommandId::Workspaces));
    }

    /// The footer, the help overlay, and the palette all read one command per
    /// action, so a new `[keys]` field cannot advertise a key that runs
    /// nothing, and two actions cannot quietly share a command.
    #[test]
    fn every_key_action_maps_to_exactly_one_command() {
        for action in KeyAction::ALL.iter().copied() {
            let id = command_for_action(action);
            assert_eq!(
                spec(id).action,
                Some(action),
                "{action:?} maps to {id:?}, which claims a different action"
            );
        }
        for entry in COMMANDS {
            if let Some(action) = entry.action {
                assert_eq!(command_for_action(action), entry.id);
            }
        }
    }

    /// The conversation-pane commands answer herdr's letters, so a herdr user
    /// splits, closes, and moves between panes without learning anything new.
    /// The resize commands are bindable but unbound, like the other commands
    /// a mis-hit should not run.
    #[test]
    fn the_pane_commands_carry_herdrs_letters() {
        let dashboard = dashboard_with_session(running_session());
        for (id, label) in [
            (CommandId::OpenSessionSplitRight, "ctrl+b v"),
            (CommandId::OpenSessionSplitBelow, "ctrl+b -"),
            (CommandId::ClosePane, "ctrl+b x"),
            (CommandId::FocusPaneLeft, "ctrl+b h"),
            (CommandId::FocusPaneDown, "ctrl+b j"),
            (CommandId::FocusPaneUp, "ctrl+b k"),
            (CommandId::FocusPaneRight, "ctrl+b l"),
            (CommandId::ZoomPane, "ctrl+b z"),
            (CommandId::FocusLastPane, "ctrl+b ;"),
            // Zoom took herdr's `prefix+z`, so the support panes' size key is
            // its shifted form.
            (CommandId::CycleFocusedPaneSize, "ctrl+b shift+z"),
        ] {
            assert_eq!(dashboard.key_labels(id), vec![label.to_owned()], "{id:?}");
        }
        for id in [
            CommandId::ResizePaneLeft,
            CommandId::ResizePaneDown,
            CommandId::ResizePaneUp,
            CommandId::ResizePaneRight,
        ] {
            assert!(spec(id).action.is_some(), "{id:?} must be bindable");
            assert!(
                dashboard.key_labels(id).is_empty(),
                "{id:?} must be unbound"
            );
        }
    }

    #[test]
    fn the_palette_omits_only_itself_and_the_numbered_workspace_keys() {
        for id in [CommandId::Palette, CommandId::SwitchWorkspace] {
            assert!(hidden_from_palette(id), "{id:?}");
        }
        for id in [
            CommandId::Workspaces,
            CommandId::NewSessionWizard,
            CommandId::ResumeDialog,
            CommandId::RestartSession,
            CommandId::OpenConfig,
            CommandId::WebViewer,
            CommandId::Help,
        ] {
            assert!(!hidden_from_palette(id), "{id:?}");
        }
    }

    #[test]
    fn spinner_selection_waits_for_the_current_save_before_accepting_another() {
        let mut dashboard = dashboard_with_session(running_session());
        let first = dashboard.dispatch_command(CommandId::CycleSpinner);
        assert!(matches!(
            first,
            DashboardAction::SaveSpinnerStyle {
                style: mj_core::config::SpinnerStyle::Pulse
            }
        ));
        assert!(matches!(
            spinner_available(&dashboard),
            Availability::Blocked(_)
        ));
        assert!(matches!(
            dashboard.dispatch_command(CommandId::CycleSpinner),
            DashboardAction::None
        ));

        dashboard.config.spinner = mj_core::config::SpinnerStyle::Pulse;
        dashboard.finish_spinner_style_save();
        assert_eq!(spinner_available(&dashboard), Availability::Ready);
        assert!(matches!(
            dashboard.dispatch_command(CommandId::CycleSpinner),
            DashboardAction::SaveSpinnerStyle {
                style: mj_core::config::SpinnerStyle::Wave
            }
        ));
    }

    #[test]
    fn session_activity_arms_redraws_until_the_work_settles() {
        let mut dashboard = dashboard_with_session(running_session());
        assert!(!dashboard.needs_fast_tick());
        dashboard
            .session_details
            .get_mut("session-1")
            .unwrap()
            .activity
            .foreground_tool_started_at_ms = Some(1);
        assert!(dashboard.needs_fast_tick());
        dashboard
            .session_details
            .get_mut("session-1")
            .unwrap()
            .activity = mj_client::usage_format::SessionActivity::default();
        assert!(!dashboard.needs_fast_tick());
        dashboard.session_operations.insert(
            "session-1".into(),
            operation(SessionOperationKind::Launching, None),
        );
        assert!(dashboard.needs_fast_tick());
        dashboard.session_operations.clear();
        assert!(!dashboard.needs_fast_tick());
    }

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
            let mut seen: Vec<KeyCode> = Vec::new();
            for entry in COMMANDS {
                if entry.scope == Scope::Setup || !scope_applies(entry.scope, focus) {
                    continue;
                }
                for hint in entry.pane_keys {
                    assert!(
                        !seen.contains(&hint.code),
                        "{:?} is claimed twice at {focus:?}",
                        hint.code
                    );
                    seen.push(hint.code);
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

    #[test]
    fn force_destroy_needs_a_selected_session_and_survives_in_flight_operations() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        assert!(available(&dashboard, None).contains(&CommandId::DestroySession));

        dashboard.session_operations.insert(
            "session-1".into(),
            operation(SessionOperationKind::Launching, None),
        );
        assert!(
            available(&dashboard, None).contains(&CommandId::DestroySession),
            "force destruction exists to preempt a wedged operation"
        );
    }

    #[test]
    fn pin_and_unpin_availability_follow_the_current_layout() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        assert_eq!(
            (spec(CommandId::PinSession).available)(&dashboard),
            Availability::Ready
        );
        assert_eq!(
            (spec(CommandId::UnpinSession).available)(&dashboard),
            Availability::Blocked("this session is not pinned")
        );

        dashboard.pin_ids.insert("session-1".into(), 1);
        assert_eq!(
            (spec(CommandId::PinSession).available)(&dashboard),
            Availability::Blocked("this session is already pinned")
        );
        assert_eq!(
            (spec(CommandId::UnpinSession).available)(&dashboard),
            Availability::Ready
        );
    }

    #[test]
    fn move_command_opens_the_fixed_workspace_resume_controls() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        assert!(available(&dashboard, None).contains(&CommandId::MoveSession));
        assert_eq!(
            dashboard.dispatch_command(CommandId::MoveSession),
            DashboardAction::None
        );
        let crate::Mode::Resume(wizard) = &dashboard.mode else {
            panic!("move opens the shared resume wizard");
        };
        assert!(wizard.moving);
        assert_eq!(wizard.session_id, "session-1");
        assert!(
            wizard.discard_queue,
            "move defaults to discarding queued work"
        );
    }

    #[test]
    fn prepared_move_is_retained_for_the_explicit_confirmation_submit() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        let _ = dashboard.dispatch_command(CommandId::MoveSession);
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::None
        );
        if let Some(DashboardAction::CheckTargetReadiness {
            generation,
            target_ids,
        }) = dashboard.take_prerequisite_check()
        {
            for id in target_ids {
                dashboard.apply_target_readiness(generation, id, Ok(()));
            }
        }
        let preparation_request_id = match dashboard.handle_key(key(KeyCode::Enter)) {
            DashboardAction::MoveSession {
                preparation_request_id: Some(request_id),
                ..
            } => request_id,
            action => panic!("entering move review should request preparation: {action:?}"),
        };
        let preparation = mj_core::state::MovePreparation {
            in_place: false,
            source_unavailable: false,
            conversion: None,
            selection: mj_core::state::MoveSelection {
                session_id: "session-1".into(),
                profile_id: Some("codex-1".into()),
                target_template_id: Some("podman".into()),
                additional_mounts: Some(Vec::new()),
                resource_allocation: None,
                clear_resource_allocation: false,
            },
            source_profile_id: "codex-1".into(),
            source_target_template_id: "podman".into(),
            cross_harness: false,
            active: true,
            queued_commands: Vec::new(),
            fingerprint: "fingerprint".into(),
            operation_id: "move-1".into(),
        };
        assert!(dashboard.apply_move_preparation(preparation_request_id, preparation.clone()));
        let crate::Mode::Resume(wizard) = &dashboard.mode else {
            panic!("move remains in its confirmation wizard");
        };
        assert_eq!(wizard.preparation.as_ref(), Some(&preparation));
        assert!(!wizard.preparing);
        assert!(matches!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::MoveSession {
                preparation_request_id: None,
                ..
            }
        ));
    }
}
