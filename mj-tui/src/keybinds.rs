//! The prefix-key router: what a key press means once the configured
//! `[keys]` bindings are in force.
//!
//! mj follows tmux and herdr. One prefix key — `ctrl+b` unless the
//! configuration says otherwise — arms a pending state, and the next key runs
//! the command bound after the prefix. Pressing the prefix twice sends the
//! literal key on to whatever has focus, so `Ctrl-B` still moves the composer's
//! cursor back one character.
//!
//! The router runs in exactly one place per key press: the terminal event loop
//! in `mj-cli`. Running it a second time further down would re-arm the prefix
//! on the literal key the loop is forwarding.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use mj_core::config::{
    Binding, KeyAction, KeyCombo, KeyName, Keybinds, Modifiers, Trigger, format_key_combo,
    normalize_key_combo,
};

use crate::{CommandId, DashboardState};

/// What the router decided about one key press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyRoute {
    /// Nothing here claims the key; hand it to the panes or the composer.
    Forward,
    /// The router answered the key itself and there is nothing more to do.
    Consumed,
    /// The key runs a command. `index` carries the slot of a numbered key.
    Command { id: CommandId, index: Option<usize> },
}

/// The configuration's spelling of one crossterm key press, or `None` for a
/// key the `[keys]` grammar cannot describe.
///
/// `BackTab` becomes `Tab` carrying `shift`, which is how the configuration
/// writes `shift+tab`. The Super/Command modifier is kept as itself: the
/// accelerator remapping the dashboard does elsewhere is about menu
/// accelerators, and a binding must mean the same key on every platform.
pub(crate) fn key_event_combo(key: &KeyEvent) -> Option<KeyCombo> {
    let (name, back_tab) = match key.code {
        KeyCode::Char(character) => (KeyName::Char(character), false),
        KeyCode::Enter => (KeyName::Enter, false),
        KeyCode::Esc => (KeyName::Esc, false),
        KeyCode::Tab => (KeyName::Tab, false),
        KeyCode::BackTab => (KeyName::Tab, true),
        KeyCode::Backspace => (KeyName::Backspace, false),
        KeyCode::Delete => (KeyName::Delete, false),
        KeyCode::Insert => (KeyName::Insert, false),
        KeyCode::Home => (KeyName::Home, false),
        KeyCode::End => (KeyName::End, false),
        KeyCode::PageUp => (KeyName::PageUp, false),
        KeyCode::PageDown => (KeyName::PageDown, false),
        KeyCode::Left => (KeyName::Left, false),
        KeyCode::Right => (KeyName::Right, false),
        KeyCode::Up => (KeyName::Up, false),
        KeyCode::Down => (KeyName::Down, false),
        KeyCode::F(number) => (KeyName::F(number), false),
        _ => return None,
    };
    let modifiers = Modifiers {
        ctrl: key.modifiers.contains(KeyModifiers::CONTROL),
        alt: key.modifiers.contains(KeyModifiers::ALT),
        shift: back_tab || key.modifiers.contains(KeyModifiers::SHIFT),
        super_: key.modifiers.contains(KeyModifiers::SUPER),
    };
    Some(normalize_key_combo(KeyCombo::new(name, modifiers)))
}

/// The crossterm key press a combination stands for, for tests and for the
/// documentation screenshots.
#[cfg(test)]
pub(crate) fn combo_key_event(combo: KeyCombo) -> KeyEvent {
    let code = match combo.name {
        KeyName::Char(character) => KeyCode::Char(character),
        KeyName::Enter => KeyCode::Enter,
        KeyName::Esc => KeyCode::Esc,
        KeyName::Tab => KeyCode::Tab,
        KeyName::Backspace => KeyCode::Backspace,
        KeyName::Delete => KeyCode::Delete,
        KeyName::Insert => KeyCode::Insert,
        KeyName::Home => KeyCode::Home,
        KeyName::End => KeyCode::End,
        KeyName::PageUp => KeyCode::PageUp,
        KeyName::PageDown => KeyCode::PageDown,
        KeyName::Left => KeyCode::Left,
        KeyName::Right => KeyCode::Right,
        KeyName::Up => KeyCode::Up,
        KeyName::Down => KeyCode::Down,
        KeyName::F(number) => KeyCode::F(number),
    };
    let mut modifiers = KeyModifiers::NONE;
    if combo.modifiers.ctrl {
        modifiers |= KeyModifiers::CONTROL;
    }
    if combo.modifiers.alt {
        modifiers |= KeyModifiers::ALT;
    }
    if combo.modifiers.shift {
        modifiers |= KeyModifiers::SHIFT;
    }
    if combo.modifiers.super_ {
        modifiers |= KeyModifiers::SUPER;
    }
    KeyEvent::new(code, modifiers)
}

/// The command one bindable action runs. Exhaustive on purpose: a new action
/// in `mj-core` must be given a command here before it compiles.
pub(crate) fn command_for_action(action: KeyAction) -> CommandId {
    match action {
        KeyAction::Help => CommandId::Help,
        KeyAction::OpenSettings => CommandId::OpenConfig,
        KeyAction::Detach => CommandId::QuitDetach,
        KeyAction::NewSession => CommandId::NewSessionWizard,
        KeyAction::Resume => CommandId::ResumeDialog,
        KeyAction::WorkspaceManager => CommandId::Workspaces,
        KeyAction::FocusWorkspaces => CommandId::FocusWorkspaces,
        KeyAction::NextWorkspace => CommandId::SelectWorkspaceNext,
        KeyAction::PreviousWorkspace => CommandId::SelectWorkspacePrevious,
        KeyAction::SwitchWorkspace => CommandId::SwitchWorkspace,
        KeyAction::NextPane => CommandId::CycleFocus,
        KeyAction::PreviousPane => CommandId::CycleFocusReverse,
        KeyAction::PaneSize => CommandId::CycleFocusedPaneSize,
        KeyAction::PanePreset => CommandId::TogglePanePreset,
        KeyAction::Refresh => CommandId::Refresh,
        KeyAction::Palette => CommandId::Palette,
        KeyAction::CancelOperation => CommandId::CancelOperation,
        KeyAction::MarkAllRead => CommandId::MarkAllRead,
        KeyAction::NextAttention => CommandId::NextAttention,
        KeyAction::PreviousAttention => CommandId::PreviousAttention,
        KeyAction::WebViewer => CommandId::WebViewer,
        KeyAction::RenameSession => CommandId::RenameSession,
        KeyAction::ToggleTranscriptRendering => CommandId::ToggleTranscriptRendering,
        KeyAction::ToggleDictation => CommandId::ToggleDictation,
        KeyAction::ChangedFiles => CommandId::ChangedFiles,
        KeyAction::StopSession => CommandId::StopSession,
        KeyAction::RestartSession => CommandId::RestartSession,
        KeyAction::MoveSession => CommandId::MoveSession,
        KeyAction::DeleteSession => CommandId::ForceDestroySession,
        KeyAction::ContainerSettings => CommandId::ContainerSettings,
        KeyAction::ManageProfiles => CommandId::ManageProfiles,
        KeyAction::ManageTargets => CommandId::ManageTargets,
        KeyAction::ManageMachines => CommandId::ManageMachines,
        KeyAction::RestartDaemon => CommandId::RestartDaemon,
        KeyAction::ChangeGoSetup => CommandId::ChangeGoSetup,
        KeyAction::CycleSpinner => CommandId::CycleSpinner,
    }
}

/// How many bindings at the head of `bindings` form the nine numbered
/// workspace keys, so the interface can print them as one row.
fn digit_range_len(bindings: &[Binding]) -> usize {
    if bindings.len() < 9 {
        return 0;
    }
    let first = bindings[0];
    let matching = bindings.iter().take(9).enumerate().all(|(slot, binding)| {
        binding.trigger == first.trigger
            && binding.index == Some(slot)
            && binding.combo.modifiers == first.combo.modifiers
            && binding.combo.name
                == KeyName::Char(char::from_digit(slot as u32 + 1, 10).unwrap_or('0'))
    });
    if matching { 9 } else { 0 }
}

/// One binding's label. `rhs_only` drops the prefix from a prefix binding, so
/// the footer can print `c` under a `ctrl+b then:` heading.
fn binding_label(keybinds: &Keybinds, binding: &Binding, rhs_only: bool) -> String {
    let key = format_key_combo(binding.combo);
    match binding.trigger {
        Trigger::Prefix if !rhs_only => format!("{} {key}", keybinds.prefix_label()),
        _ => key,
    }
}

/// Every label for one action, with the nine numbered workspace keys collapsed
/// into a single `1-9` row.
fn action_labels(keybinds: &Keybinds, action: KeyAction, rhs_only: bool) -> Vec<String> {
    let bindings = keybinds.bindings(action);
    let mut labels = Vec::new();
    let mut position = 0;
    while let Some(binding) = bindings.get(position) {
        let run = digit_range_len(&bindings[position..]);
        let label = binding_label(keybinds, binding, rhs_only);
        if run == 9 {
            // Every label in the run ends with its own digit; `1-9` replaces it.
            let stem = label.strip_suffix('1').unwrap_or(label.as_str());
            labels.push(format!("{stem}1-9"));
            position += run;
        } else {
            labels.push(label);
            position += 1;
        }
    }
    labels
}

impl DashboardState {
    /// The bindings currently in force.
    pub fn keybinds(&self) -> &Keybinds {
        &self.keybinds
    }

    /// Whether the prefix key has been pressed and the surface is waiting for
    /// the key that completes the chord.
    pub fn prefix_pending(&self) -> bool {
        self.prefix_pending
    }

    /// Disarms a pending prefix, for a pointer event or a lost focus.
    pub fn cancel_prefix(&mut self) {
        self.prefix_pending = false;
    }

    /// Reads one key press against the configured bindings.
    ///
    /// With no prefix pending a direct binding runs its command, the prefix
    /// itself arms the pending state, and anything else is forwarded. While
    /// pending, the prefix again forwards the literal key (tmux's rule, which
    /// is how `Ctrl-B` still reaches the composer), `Esc` cancels, a bound key
    /// runs its command, and anything else is swallowed with a notice saying
    /// so.
    pub fn route_bound_key(&mut self, key: &KeyEvent) -> KeyRoute {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return KeyRoute::Forward;
        }
        if key.kind == KeyEventKind::Press {
            // A key press ends the pointer bookkeeping a modal-closing click
            // left behind, exactly as `handle_key_at` does for the keys that
            // reach it. Without this a command run from here would leave the
            // next click suppressed.
            self.modal_click_transition = None;
            self.suppress_modal_release = false;
        }
        let Some(combo) = key_event_combo(key) else {
            return KeyRoute::Forward;
        };
        if !self.prefix_pending {
            if combo == self.keybinds.prefix {
                self.prefix_pending = true;
                return KeyRoute::Consumed;
            }
            return match self.keybinds.resolve_direct(combo) {
                Some(matched) => KeyRoute::Command {
                    id: command_for_action(matched.action),
                    index: matched.index,
                },
                None => KeyRoute::Forward,
            };
        }
        self.prefix_pending = false;
        if combo == self.keybinds.prefix {
            // tmux's rule: the doubled prefix is the literal key.
            return KeyRoute::Forward;
        }
        if combo == KeyCombo::plain(KeyName::Esc) {
            return KeyRoute::Consumed;
        }
        if let Some(matched) = self.keybinds.resolve_prefix(combo) {
            return KeyRoute::Command {
                id: command_for_action(matched.action),
                index: matched.index,
            };
        }
        let prefix = self.keybinds.prefix_label();
        let pressed = format_key_combo(combo);
        let notice = match action_labels(&self.keybinds, KeyAction::Help, true).first() {
            Some(help) => format!("{prefix} {pressed} is not bound; {prefix} {help} lists keys"),
            None => format!("{prefix} {pressed} is not bound"),
        };
        self.set_notice(notice);
        KeyRoute::Consumed
    }

    /// [`DashboardState::route_bound_key`] for a whole terminal event.
    pub fn route_bound_key_event(&mut self, event: &crossterm::event::Event) -> KeyRoute {
        match event {
            crossterm::event::Event::Key(key) => self.route_bound_key(key),
            _ => KeyRoute::Forward,
        }
    }

    /// Every key that runs a command: the pane keys first, then the live
    /// bindings, as `["Enter", "ctrl+b c"]`.
    pub(crate) fn key_labels(&self, id: CommandId) -> Vec<String> {
        let spec = crate::actions::spec(id);
        let mut labels = spec
            .pane_keys
            .iter()
            .map(|hint| hint.label.to_owned())
            .collect::<Vec<_>>();
        if let Some(action) = spec.action {
            labels.extend(action_labels(&self.keybinds, action, false));
        }
        labels
    }

    /// The first key that runs a command, for a sentence that tells the reader
    /// what to press. `None` when nothing is bound to it.
    ///
    /// A configured binding comes before a pane key here, the other way round
    /// from [`DashboardState::key_labels`]: prose like "press … to create a
    /// session" is read from the conversation as often as from a pane, and a
    /// bare pane letter is text everywhere else.
    pub fn first_key_label(&self, id: CommandId) -> Option<String> {
        let spec = crate::actions::spec(id);
        spec.action
            .and_then(|action| {
                action_labels(&self.keybinds, action, false)
                    .into_iter()
                    .next()
            })
            .or_else(|| spec.pane_keys.first().map(|hint| hint.label.to_owned()))
    }

    /// The key the footer prints for a command: the key after the prefix for a
    /// chord command, otherwise its first pane key.
    pub(crate) fn footer_key(&self, id: CommandId) -> Option<String> {
        let spec = crate::actions::spec(id);
        let after_prefix = || {
            spec.action.and_then(|action| {
                action_labels(&self.keybinds, action, true)
                    .into_iter()
                    .next()
            })
        };
        if spec.footer_group == crate::actions::FooterGroup::Chord {
            return after_prefix();
        }
        spec.pane_keys
            .first()
            .map(|hint| hint.label.to_owned())
            .or_else(after_prefix)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        config, dashboard_with_session, key, prefix_key, route, running_session,
    };
    use crate::{DashboardAction, Focus, Mode};
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    fn plain(character: char) -> KeyEvent {
        key(KeyCode::Char(character))
    }

    #[test]
    fn the_prefix_arms_pending_and_a_bound_key_runs_its_command() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        assert_eq!(dashboard.route_bound_key(&prefix_key()), KeyRoute::Consumed);
        assert!(dashboard.prefix_pending());
        assert_eq!(
            dashboard.route_bound_key(&plain('?')),
            KeyRoute::Command {
                id: CommandId::Help,
                index: None
            }
        );
        assert!(!dashboard.prefix_pending());
    }

    #[test]
    fn pressing_the_prefix_twice_forwards_the_literal_key_and_disarms() {
        let mut dashboard = dashboard_with_session(running_session());
        assert_eq!(dashboard.route_bound_key(&prefix_key()), KeyRoute::Consumed);
        assert_eq!(dashboard.route_bound_key(&prefix_key()), KeyRoute::Forward);
        assert!(!dashboard.prefix_pending());
        assert_eq!(dashboard.notices.current(), None);
    }

    #[test]
    fn esc_cancels_a_pending_prefix_without_forwarding() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.route_bound_key(&prefix_key());
        assert_eq!(
            dashboard.route_bound_key(&key(KeyCode::Esc)),
            KeyRoute::Consumed
        );
        assert!(!dashboard.prefix_pending());
        assert_eq!(dashboard.notices.current(), None);
    }

    #[test]
    fn an_unbound_key_after_the_prefix_is_swallowed_with_a_notice() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.route_bound_key(&prefix_key());
        assert_eq!(dashboard.route_bound_key(&plain('y')), KeyRoute::Consumed);
        assert_eq!(
            dashboard.notices.current().as_deref(),
            Some("ctrl+b y is not bound; ctrl+b ? lists keys")
        );
    }

    #[test]
    fn a_direct_user_binding_runs_without_the_prefix() {
        let mut dashboard = dashboard_with_session(running_session());
        let mut config = config();
        config.keys.refresh = "f5".into();
        dashboard.set_config(config);
        assert_eq!(
            dashboard.route_bound_key(&key(KeyCode::F(5))),
            KeyRoute::Command {
                id: CommandId::Refresh,
                index: None
            }
        );
        assert!(!dashboard.prefix_pending());
        // The default prefix chord is gone, because the user's value replaced it.
        dashboard.route_bound_key(&prefix_key());
        assert_eq!(
            dashboard.route_bound_key(&key(KeyCode::Char('R'))),
            KeyRoute::Consumed
        );
    }

    #[test]
    fn set_config_replaces_the_live_bindings() {
        let mut dashboard = dashboard_with_session(running_session());
        assert_eq!(dashboard.keybinds().prefix_label(), "ctrl+b");
        let mut config = config();
        config.keys.prefix = "ctrl+a".to_owned();
        dashboard.set_config(config);
        assert_eq!(dashboard.keybinds().prefix_label(), "ctrl+a");
        assert_eq!(dashboard.route_bound_key(&prefix_key()), KeyRoute::Forward);
        assert_eq!(
            dashboard.route_bound_key(&KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            KeyRoute::Consumed
        );
        assert!(dashboard.prefix_pending());
    }

    #[test]
    fn shift_and_uppercase_letters_match_the_same_binding() {
        for pressed in [
            KeyEvent::new(KeyCode::Char('N'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('N'), KeyModifiers::SHIFT),
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::SHIFT),
        ] {
            let mut dashboard = dashboard_with_session(running_session());
            dashboard.route_bound_key(&prefix_key());
            assert_eq!(
                dashboard.route_bound_key(&pressed),
                KeyRoute::Command {
                    id: CommandId::Workspaces,
                    index: None
                },
                "{pressed:?}"
            );
        }
    }

    #[test]
    fn colon_and_question_mark_match_with_or_without_the_shift_flag() {
        for (pressed, expected) in [
            (
                KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE),
                CommandId::Palette,
            ),
            (
                KeyEvent::new(KeyCode::Char(':'), KeyModifiers::SHIFT),
                CommandId::Palette,
            ),
            (
                KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE),
                CommandId::Help,
            ),
            (
                KeyEvent::new(KeyCode::Char('?'), KeyModifiers::SHIFT),
                CommandId::Help,
            ),
        ] {
            let mut dashboard = dashboard_with_session(running_session());
            dashboard.route_bound_key(&prefix_key());
            assert_eq!(
                dashboard.route_bound_key(&pressed),
                KeyRoute::Command {
                    id: expected,
                    index: None
                },
                "{pressed:?}"
            );
        }
    }

    #[test]
    fn switch_workspace_digits_carry_their_index() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.route_bound_key(&prefix_key());
        assert_eq!(
            dashboard.route_bound_key(&plain('3')),
            KeyRoute::Command {
                id: CommandId::SwitchWorkspace,
                index: Some(2)
            }
        );
        // The nine keys read as one row wherever they are advertised.
        assert_eq!(
            dashboard.key_labels(CommandId::SwitchWorkspace),
            vec!["ctrl+b 1-9".to_owned()]
        );
        assert_eq!(
            dashboard.footer_key(CommandId::SwitchWorkspace),
            Some("1-9".to_owned())
        );
    }

    #[test]
    fn a_mouse_press_cancels_a_pending_prefix() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.route_bound_key(&prefix_key());
        assert!(dashboard.prefix_pending());
        dashboard.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        assert!(!dashboard.prefix_pending());
    }

    /// The router is the only place the numbered workspace keys carry an
    /// argument, so the dispatch path has to read the index rather than the
    /// command alone.
    #[test]
    fn routing_a_numbered_workspace_key_selects_that_workspace() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.set_workspace_names(
            [
                ("default".to_owned(), "Default".to_owned()),
                ("second".to_owned(), "Second".to_owned()),
            ]
            .into_iter()
            .collect(),
        );
        assert_eq!(
            route(&mut dashboard, &[prefix_key(), plain('2')]),
            DashboardAction::SelectWorkspace {
                workspace_id: "second".to_owned()
            }
        );
    }

    #[test]
    fn the_prefix_help_key_opens_the_overlay_from_the_composer() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_prompt();
        assert_eq!(dashboard.focus, Focus::Prompt);
        route(&mut dashboard, &[prefix_key(), plain('?')]);
        assert!(matches!(dashboard.mode, Mode::Help(_)));
    }
}
