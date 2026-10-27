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
        KeyAction::Help => CommandId::Palette,
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
        KeyAction::SplitVertical => CommandId::OpenSessionSplitRight,
        KeyAction::SplitHorizontal => CommandId::OpenSessionSplitBelow,
        KeyAction::ClosePane => CommandId::ClosePane,
        KeyAction::FocusPaneLeft => CommandId::FocusPaneLeft,
        KeyAction::FocusPaneDown => CommandId::FocusPaneDown,
        KeyAction::FocusPaneUp => CommandId::FocusPaneUp,
        KeyAction::FocusPaneRight => CommandId::FocusPaneRight,
        KeyAction::Zoom => CommandId::ZoomPane,
        KeyAction::LastPane => CommandId::FocusLastPane,
        KeyAction::ResizeMode => CommandId::ResizeMode,
        KeyAction::SwapPaneLeft => CommandId::SwapPaneLeft,
        KeyAction::SwapPaneDown => CommandId::SwapPaneDown,
        KeyAction::SwapPaneUp => CommandId::SwapPaneUp,
        KeyAction::SwapPaneRight => CommandId::SwapPaneRight,
        KeyAction::SuspendSession => CommandId::SuspendSession,
        KeyAction::RenameWorkspace => CommandId::RenameWorkspace,
        KeyAction::CloseWorkspace => CommandId::CloseWorkspace,
        KeyAction::ResizePaneLeft => CommandId::ResizePaneLeft,
        KeyAction::ResizePaneDown => CommandId::ResizePaneDown,
        KeyAction::ResizePaneUp => CommandId::ResizePaneUp,
        KeyAction::ResizePaneRight => CommandId::ResizePaneRight,
        KeyAction::OpenSubagents => CommandId::OpenSubagents,
        KeyAction::SessionActions => CommandId::SessionActions,
        KeyAction::InterruptTurn => CommandId::InterruptTurn,
        KeyAction::RestartSession => CommandId::RestartSession,
        KeyAction::MoveSession => CommandId::MoveSession,
        KeyAction::DestroySession => CommandId::DestroySession,
        KeyAction::ContainerSettings => CommandId::ContainerSettings,
        KeyAction::ManageProfiles => CommandId::ManageProfiles,
        KeyAction::ManageTargets => CommandId::ManageTargets,
        KeyAction::ManageMachines => CommandId::ManageMachines,
        KeyAction::RestartDaemon => CommandId::RestartDaemon,
        KeyAction::NoticeLog => CommandId::NoticeLog,
        KeyAction::ChangeGoSetup => CommandId::ChangeGoSetup,
        KeyAction::CycleSpinner => CommandId::CycleSpinner,
    }
}

/// How many bindings at the head of `bindings` form the nine numbered
/// workspace keys, so the interface can print them as one row.
fn digit_range_len(bindings: &[&Binding]) -> usize {
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
fn binding_labels(keybinds: &Keybinds, bindings: &[&Binding], rhs_only: bool) -> Vec<String> {
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
        if self.modal_open() {
            self.resize_mode = false;
        }
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
        if self.simple_mode {
            // Simple mode has no prefix state. Every Mjolnir command is one
            // direct chord, and unbound keys continue into the focused
            // surface unchanged (including a bare Ctrl+B for tmux).
            self.prefix_pending = false;
            if self.keybinds.resolve_direct(combo).is_none() && self.resize_mode_active() {
                let id = if key.modifiers.is_empty() {
                    match key.code {
                        KeyCode::Char('h') | KeyCode::Left => Some(CommandId::ResizePaneLeft),
                        KeyCode::Char('j') | KeyCode::Down => Some(CommandId::ResizePaneDown),
                        KeyCode::Char('k') | KeyCode::Up => Some(CommandId::ResizePaneUp),
                        KeyCode::Char('l') | KeyCode::Right => Some(CommandId::ResizePaneRight),
                        KeyCode::Esc => {
                            self.resize_mode = false;
                            None
                        }
                        _ => None,
                    }
                } else {
                    None
                };
                return id.map_or(KeyRoute::Consumed, |id| KeyRoute::Command {
                    id,
                    index: None,
                });
            }
            self.resize_mode = false;
            return match self.keybinds.resolve_direct(combo) {
                Some(matched) => KeyRoute::Command {
                    id: command_for_action(matched.action),
                    index: matched.index,
                },
                None => KeyRoute::Forward,
            };
        }
        if !self.prefix_pending {
            if combo == self.keybinds.prefix {
                self.prefix_pending = true;
                return KeyRoute::Consumed;
            }
            if self.keybinds.resolve_direct(combo).is_none() && self.resize_mode_active() {
                let id = if key.modifiers.is_empty() {
                    match key.code {
                        KeyCode::Char('h') | KeyCode::Left => Some(CommandId::ResizePaneLeft),
                        KeyCode::Char('j') | KeyCode::Down => Some(CommandId::ResizePaneDown),
                        KeyCode::Char('k') | KeyCode::Up => Some(CommandId::ResizePaneUp),
                        KeyCode::Char('l') | KeyCode::Right => Some(CommandId::ResizePaneRight),
                        KeyCode::Esc => {
                            self.resize_mode = false;
                            None
                        }
                        _ => None,
                    }
                } else {
                    None
                };
                return id.map_or(KeyRoute::Consumed, |id| KeyRoute::Command {
                    id,
                    index: None,
                });
            }
            self.resize_mode = false;
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
            if self.resize_mode_active() {
                return KeyRoute::Consumed;
            }
            // tmux's rule: the doubled prefix is the literal key.
            return KeyRoute::Forward;
        }
        if combo == KeyCombo::plain(KeyName::Esc) {
            self.resize_mode = false;
            return KeyRoute::Consumed;
        }
        if let Some(matched) = self.keybinds.resolve_prefix(combo) {
            self.resize_mode = false;
            return KeyRoute::Command {
                id: command_for_action(matched.action),
                index: matched.index,
            };
        }
        let prefix = self.keybinds.prefix_label();
        let pressed = format_key_combo(combo);
        let palette_labels = command_action_labels(
            &self.keybinds,
            CommandId::Palette,
            KeyAction::Palette,
            false,
        );
        let notice = match palette_labels.first() {
            Some(palette) => {
                format!("{prefix} {pressed} is not bound; {palette} opens the command palette")
            }
            None => format!("{prefix} {pressed} is not bound"),
        };
        self.set_notice(notice);
        KeyRoute::Consumed
    }

    /// [`DashboardState::route_bound_key`] for a whole terminal event.
    pub fn route_bound_key_event(&mut self, event: &crossterm::event::Event) -> KeyRoute {
        match event {
            crossterm::event::Event::Key(key) => self.route_bound_key(key),
            crossterm::event::Event::Paste(_) if self.resize_mode_active() => KeyRoute::Consumed,
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
            labels.extend(command_action_labels(&self.keybinds, id, action, false));
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
                command_action_labels(&self.keybinds, id, action, false)
                    .into_iter()
                    .next()
            })
            .or_else(|| spec.pane_keys.first().map(|hint| hint.label.to_owned()))
    }

    /// The key the footer prints for a command: the key after the prefix for a
    /// chord command, otherwise its first pane key.
    pub(crate) fn footer_key(&self, id: CommandId) -> Option<String> {
        let spec = crate::actions::spec(id);
        if id == CommandId::Palette {
            return self
                .palette_binding_labels(Trigger::Prefix, true)
                .into_iter()
                .reduce(|first, next| format!("{first} / {next}"));
        }
        let after_prefix = || {
            spec.action.and_then(|action| {
                command_action_labels(&self.keybinds, id, action, true)
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

    pub(crate) fn palette_binding_labels(&self, trigger: Trigger, rhs_only: bool) -> Vec<String> {
        let bindings =
            command_action_bindings(&self.keybinds, CommandId::Palette, KeyAction::Palette)
                .into_iter()
                .filter(|binding| binding.trigger == trigger)
                .collect::<Vec<_>>();
        binding_labels(&self.keybinds, &bindings, rhs_only)
    }

    pub(crate) fn direct_palette_footer_key(&self) -> Option<String> {
        self.palette_binding_labels(Trigger::Direct, false)
            .into_iter()
            .reduce(|first, next| format!("{first} / {next}"))
    }
}

fn command_action_labels(
    keybinds: &Keybinds,
    id: CommandId,
    action: KeyAction,
    rhs_only: bool,
) -> Vec<String> {
    let bindings = command_action_bindings(keybinds, id, action);
    binding_labels(keybinds, &bindings, rhs_only)
}

fn command_action_bindings(keybinds: &Keybinds, id: CommandId, action: KeyAction) -> Vec<&Binding> {
    let mut actions = vec![action];
    actions.extend(
        KeyAction::ALL
            .iter()
            .copied()
            .filter(|candidate| *candidate != action && command_for_action(*candidate) == id),
    );
    actions
        .into_iter()
        .flat_map(|candidate| keybinds.bindings(candidate))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{config, dashboard_with_session, key, prefix_key, running_session};
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    fn plain(character: char) -> KeyEvent {
        key(KeyCode::Char(character))
    }

    #[test]
    fn simple_mode_has_no_prefix_and_leaves_ctrl_b_to_the_focused_surface() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.simple_mode = true;
        dashboard.keybinds = mj_core::config::simple_keybinds();

        let ctrl_b = KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL);
        assert_eq!(dashboard.route_bound_key(&ctrl_b), KeyRoute::Forward);
        assert!(!dashboard.prefix_pending());

        let ctrl_q = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL);
        assert_eq!(
            dashboard.route_bound_key(&ctrl_q),
            KeyRoute::Command {
                id: CommandId::QuitDetach,
                index: None,
            }
        );
        assert!(!dashboard.prefix_pending());

        let ctrl_f1 = KeyEvent::new(KeyCode::F(1), KeyModifiers::CONTROL);
        let ctrl_p = KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL);
        for key in [ctrl_f1, ctrl_p] {
            assert_eq!(
                dashboard.route_bound_key(&key),
                KeyRoute::Command {
                    id: CommandId::Palette,
                    index: None,
                }
            );
        }

        let dormant_prefix = KeyEvent::new(KeyCode::F(12), KeyModifiers::CONTROL);
        assert_eq!(
            dashboard.route_bound_key(&dormant_prefix),
            KeyRoute::Forward
        );
        assert!(!dashboard.prefix_pending());
    }

    #[test]
    fn a_config_reload_does_not_restore_prefix_bindings_in_simple_mode() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.simple_mode = true;
        dashboard.keybinds = mj_core::config::simple_keybinds();
        let mut reloaded = config();
        reloaded.keys.prefix = "ctrl+a".into();
        reloaded.keys.detach = "prefix+q".into();
        dashboard.set_config(reloaded);

        let ctrl_b = KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL);
        assert_eq!(dashboard.route_bound_key(&ctrl_b), KeyRoute::Forward);
        assert!(!dashboard.prefix_pending());
        let ctrl_q = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL);
        assert!(matches!(
            dashboard.route_bound_key(&ctrl_q),
            KeyRoute::Command {
                id: CommandId::QuitDetach,
                ..
            }
        ));
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
    fn control_punctuation_prefixes_accept_legacy_and_enhanced_terminal_events() {
        for (prefix, legacy, enhanced) in [
            (r"ctrl+\", '4', '\\'),
            ("ctrl+]", '5', ']'),
            ("ctrl+^", '6', '^'),
            ("ctrl+_", '7', '_'),
        ] {
            for character in [legacy, enhanced] {
                let mut dashboard = dashboard_with_session(running_session());
                let mut config = config();
                config.keys.prefix = prefix.into();
                dashboard.set_config(config);
                let event = KeyEvent::new(KeyCode::Char(character), KeyModifiers::CONTROL);
                assert_eq!(dashboard.route_bound_key(&event), KeyRoute::Consumed);
                assert!(dashboard.prefix_pending(), "{prefix}: {character}");
                assert_eq!(
                    dashboard.route_bound_key(&plain('?')),
                    KeyRoute::Command {
                        id: CommandId::Palette,
                        index: None,
                    }
                );
                assert!(!dashboard.prefix_pending());
                assert_eq!(dashboard.route_bound_key(&event), KeyRoute::Consumed);
                assert_eq!(dashboard.route_bound_key(&event), KeyRoute::Forward);
                assert!(!dashboard.prefix_pending());
            }
        }
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
                CommandId::Palette,
            ),
            (
                KeyEvent::new(KeyCode::Char('?'), KeyModifiers::SHIFT),
                CommandId::Palette,
            ),
        ] {
            let mut dashboard = dashboard_with_session(running_session());
            assert_eq!(dashboard.route_bound_key(&pressed), KeyRoute::Forward);
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
    fn decoded_legacy_help_bindings_are_palette_aliases() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let config_text = format!(
            "version = {}\n\n[keys]\nprefix = \"ctrl+a\"\nhelp = [\"ctrl+alt+p\", \"prefix+x\", \"prefix+f1\"]\npalette = \"\"\n",
            mj_core::config::CONFIG_VERSION
        );
        std::fs::write(&path, &config_text).unwrap();
        let config = mj_core::config::Config::load_from(&path).unwrap();
        assert_eq!(
            config.keys.help.entries(),
            ["ctrl+alt+p", "prefix+x", "prefix+f1"]
        );
        assert!(config.keys.palette.entries().is_empty());
        config.save_to(&path).unwrap();
        let config = mj_core::config::Config::load_from(&path).unwrap();
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.set_config(config.with_local_targets());

        assert_eq!(
            dashboard.key_labels(CommandId::Palette),
            ["?", "ctrl+alt+p", "ctrl+a x", "ctrl+a f1"]
        );
        assert_eq!(
            dashboard.footer_key(CommandId::Palette).as_deref(),
            Some("x / f1")
        );
        assert_eq!(
            dashboard.direct_palette_footer_key().as_deref(),
            Some("ctrl+alt+p")
        );
        let footer = crate::render::combined_footer_text(&dashboard, 200);
        assert!(footer.contains("ctrl+a then g sessions"), "{footer}");
        assert!(footer.contains("x / f1 palette"), "{footer}");
        assert!(footer.contains("ctrl+alt+p palette"), "{footer}");
        assert!(!footer.contains("ctrl+a then ctrl+alt+p"), "{footer}");
        let prefix = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL);
        for key in [
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE),
        ] {
            assert_eq!(dashboard.route_bound_key(&prefix), KeyRoute::Consumed);
            assert_eq!(
                dashboard.route_bound_key(&key),
                KeyRoute::Command {
                    id: CommandId::Palette,
                    index: None,
                }
            );
        }
        assert_eq!(
            dashboard.route_bound_key(&KeyEvent::new(
                KeyCode::Char('p'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            )),
            KeyRoute::Command {
                id: CommandId::Palette,
                index: None,
            }
        );

        dashboard.route_bound_key(&prefix);
        assert_eq!(
            dashboard.route_bound_key(&KeyEvent::new(KeyCode::F(12), KeyModifiers::NONE)),
            KeyRoute::Consumed
        );
        assert_eq!(
            dashboard.notices.current().as_deref(),
            Some("ctrl+a f12 is not bound; ctrl+alt+p opens the command palette")
        );

        let disabled = format!(
            "version = {}\n\n[keys]\nprefix = \"ctrl+a\"\nhelp = \"\"\npalette = \"\"\nworkspace_manager = \"prefix+?\"\n",
            mj_core::config::CONFIG_VERSION
        );
        std::fs::write(&path, &disabled).unwrap();
        let config = mj_core::config::Config::load_from(&path).unwrap();
        dashboard.set_config(config.with_local_targets());
        dashboard.route_bound_key(&prefix);
        let banner = crate::render::prefix_banner_line(&dashboard);
        let banner_text = banner
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(banner_text, " PREFIX  esc cancel · ctrl+a send");
        assert_eq!(
            dashboard.route_bound_key(&plain('?')),
            KeyRoute::Command {
                id: CommandId::Workspaces,
                index: None,
            }
        );
        dashboard.route_bound_key(&prefix);
        assert_eq!(
            dashboard.route_bound_key(&KeyEvent::new(KeyCode::F(12), KeyModifiers::NONE)),
            KeyRoute::Consumed
        );
        assert_eq!(
            dashboard.notices.current().as_deref(),
            Some("ctrl+a f12 is not bound")
        );

        let duplicate = format!(
            "version = {}\n\n[keys]\nprefix = \"ctrl+a\"\nhelp = \"prefix+x\"\npalette = \"prefix+x\"\n",
            mj_core::config::CONFIG_VERSION
        );
        std::fs::write(&path, &duplicate).unwrap();
        let config = mj_core::config::Config::load_from(&path).unwrap();
        dashboard.set_config(config.with_local_targets());
        assert_eq!(
            dashboard
                .key_labels(CommandId::Palette)
                .iter()
                .filter(|label| label.as_str() == "ctrl+a x")
                .count(),
            1
        );
        assert_eq!(dashboard.route_bound_key(&prefix), KeyRoute::Consumed);
        assert_eq!(
            dashboard.route_bound_key(&KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
            KeyRoute::Command {
                id: CommandId::Palette,
                index: None,
            }
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
}
