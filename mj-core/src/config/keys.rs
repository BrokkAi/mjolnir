//! Configurable key bindings: a tmux-style prefix key plus one field per action.
//!
//! The key types here are terminal-agnostic on purpose. `mj-core` does not
//! depend on crossterm, so the terminal crates translate their own key events
//! into a [`KeyCombo`] and ask [`Keybinds`] what that combination means.
//!
//! The string grammar matches herdr's, so a line can be copied between the two
//! configuration files: tokens joined by `+`, an optional `prefix+` marker for
//! a binding that only fires after the prefix key, and `1..9` for the numbered
//! workspace range.

use std::collections::BTreeMap;
use std::fmt;
use std::ops::BitOr;

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};

/// The default prefix key, spelled as it is written in `config.toml`.
pub const DEFAULT_PREFIX: &str = "ctrl+b";

/// A key, without any modifier state.
///
/// `shift+tab` is [`KeyName::Tab`] carrying `shift`; there is no separate
/// back-tab name, so the terminal bridge maps crossterm's `BackTab` onto it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum KeyName {
    Char(char),
    Enter,
    Esc,
    Tab,
    Backspace,
    Delete,
    Insert,
    Home,
    End,
    PageUp,
    PageDown,
    Left,
    Right,
    Up,
    Down,
    F(u8),
}

/// The modifier keys held down with a [`KeyName`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Modifiers {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub super_: bool,
}

impl Modifiers {
    pub const NONE: Self = Self {
        ctrl: false,
        alt: false,
        shift: false,
        super_: false,
    };
    pub const CTRL: Self = Self {
        ctrl: true,
        alt: false,
        shift: false,
        super_: false,
    };
    pub const SHIFT: Self = Self {
        ctrl: false,
        alt: false,
        shift: true,
        super_: false,
    };

    pub const fn is_empty(self) -> bool {
        !self.ctrl && !self.alt && !self.shift && !self.super_
    }

    /// True when nothing but `shift` is held.
    const fn shift_only(self) -> bool {
        !self.ctrl && !self.alt && !self.super_
    }
}

impl BitOr for Modifiers {
    type Output = Self;

    fn bitor(self, other: Self) -> Self {
        Self {
            ctrl: self.ctrl || other.ctrl,
            alt: self.alt || other.alt,
            shift: self.shift || other.shift,
            super_: self.super_ || other.super_,
        }
    }
}

/// One key together with the modifiers held with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KeyCombo {
    pub name: KeyName,
    pub modifiers: Modifiers,
}

impl KeyCombo {
    pub const fn new(name: KeyName, modifiers: Modifiers) -> Self {
        Self { name, modifiers }
    }

    pub const fn plain(name: KeyName) -> Self {
        Self::new(name, Modifiers::NONE)
    }
}

impl fmt::Display for KeyCombo {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&format_key_combo(*self))
    }
}

fn parse_modifier_token(token: &str) -> Option<Modifiers> {
    let mut modifiers = Modifiers::NONE;
    match token.to_ascii_lowercase().as_str() {
        "ctrl" | "control" => modifiers.ctrl = true,
        "alt" | "option" | "meta" => modifiers.alt = true,
        "shift" => modifiers.shift = true,
        "cmd" | "command" | "super" => modifiers.super_ = true,
        _ => return None,
    }
    Some(modifiers)
}

fn parse_key_name(token: &str) -> Option<KeyName> {
    let lower = token.to_ascii_lowercase();
    let name = match lower.as_str() {
        "space" => KeyName::Char(' '),
        "enter" | "return" => KeyName::Enter,
        "esc" | "escape" => KeyName::Esc,
        "tab" => KeyName::Tab,
        "backspace" | "bs" => KeyName::Backspace,
        "delete" => KeyName::Delete,
        "insert" => KeyName::Insert,
        "home" => KeyName::Home,
        "end" => KeyName::End,
        "pageup" => KeyName::PageUp,
        "pagedown" => KeyName::PageDown,
        "left" => KeyName::Left,
        "right" => KeyName::Right,
        "up" => KeyName::Up,
        "down" => KeyName::Down,
        "minus" => KeyName::Char('-'),
        "plus" => KeyName::Char('+'),
        "comma" => KeyName::Char(','),
        "period" => KeyName::Char('.'),
        "slash" => KeyName::Char('/'),
        "backslash" => KeyName::Char('\\'),
        "quote" => KeyName::Char('\''),
        "semicolon" => KeyName::Char(';'),
        "colon" => KeyName::Char(':'),
        "percent" => KeyName::Char('%'),
        "ampersand" => KeyName::Char('&'),
        "backtick" => KeyName::Char('`'),
        _ => {
            // A single character is itself; otherwise only `f1`..`f12` remain.
            let mut characters = token.chars();
            let first = characters.next()?;
            if characters.next().is_none() {
                return Some(KeyName::Char(first));
            }
            let number = lower.strip_prefix('f')?.parse::<u8>().ok()?;
            return (1..=12).contains(&number).then_some(KeyName::F(number));
        }
    };
    Some(name)
}

/// Parse one key combination, such as `ctrl+b`, `shift+tab`, `f5` or `?`.
pub fn parse_key_combo(text: &str) -> Option<KeyCombo> {
    let mut modifiers = Modifiers::NONE;
    let mut key: Option<&str> = None;
    for part in text.split('+') {
        let token = part.trim();
        if token.is_empty() {
            return None;
        }
        if let Some(modifier) = parse_modifier_token(token) {
            modifiers = modifiers | modifier;
        } else if key.is_some() {
            return None;
        } else {
            key = Some(token);
        }
    }
    let name = parse_key_name(key?)?;
    Some(normalize_key_combo(KeyCombo { name, modifiers }))
}

/// Fold the two spellings a terminal may deliver into one.
///
/// An uppercase letter becomes the lowercase letter plus `shift`, and `shift`
/// on any other character is dropped because the terminal already delivered
/// the shifted symbol itself.
pub fn normalize_key_combo(mut combo: KeyCombo) -> KeyCombo {
    match combo.name {
        KeyName::Char(character) if character.is_alphabetic() => {
            if character.is_uppercase() {
                if let Some(lower) = character.to_lowercase().next() {
                    combo.name = KeyName::Char(lower);
                }
                combo.modifiers.shift = true;
            }
        }
        KeyName::Char(_) => combo.modifiers.shift = false,
        _ => {}
    }
    combo
}

fn super_label() -> &'static str {
    if cfg!(target_os = "macos") {
        "cmd"
    } else {
        "super"
    }
}

fn key_name_label(name: KeyName) -> String {
    match name {
        KeyName::Char(' ') => "space".to_owned(),
        KeyName::Char(character) => character.to_lowercase().collect(),
        KeyName::Enter => "enter".to_owned(),
        KeyName::Esc => "esc".to_owned(),
        KeyName::Tab => "tab".to_owned(),
        KeyName::Backspace => "backspace".to_owned(),
        KeyName::Delete => "delete".to_owned(),
        KeyName::Insert => "insert".to_owned(),
        KeyName::Home => "home".to_owned(),
        KeyName::End => "end".to_owned(),
        KeyName::PageUp => "pageup".to_owned(),
        KeyName::PageDown => "pagedown".to_owned(),
        KeyName::Left => "left".to_owned(),
        KeyName::Right => "right".to_owned(),
        KeyName::Up => "up".to_owned(),
        KeyName::Down => "down".to_owned(),
        KeyName::F(number) => format!("f{number}"),
    }
}

/// Spell a combination the way `config.toml` does, so that
/// `format_key_combo(parse_key_combo(s))` returns `s` for canonical strings.
pub fn format_key_combo(combo: KeyCombo) -> String {
    let mut label = String::new();
    if combo.modifiers.ctrl {
        label.push_str("ctrl+");
    }
    if combo.modifiers.alt {
        label.push_str("alt+");
    }
    if combo.modifiers.shift {
        label.push_str("shift+");
    }
    if combo.modifiers.super_ {
        label.push_str(super_label());
        label.push('+');
    }
    label.push_str(&key_name_label(combo.name));
    label
}

/// A printable character with no modifier but `shift`: plain typing.
fn is_unmodified_printable(combo: KeyCombo) -> bool {
    matches!(combo.name, KeyName::Char(character) if !character.is_control())
        && combo.modifiers.shift_only()
}

/// One binding value: a single key string, or several alternatives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum BindingConfig {
    One(String),
    Many(Vec<String>),
}

impl Default for BindingConfig {
    fn default() -> Self {
        Self::One(String::new())
    }
}

impl BindingConfig {
    /// The key strings that carry a binding; an empty string is "unbound".
    pub fn entries(&self) -> Vec<&str> {
        let raw: &[String] = match self {
            Self::One(one) => std::slice::from_ref(one),
            Self::Many(many) => many,
        };
        raw.iter()
            .map(|entry| entry.trim())
            .filter(|entry| !entry.is_empty())
            .collect()
    }
}

impl From<&str> for BindingConfig {
    fn from(value: &str) -> Self {
        Self::One(value.to_owned())
    }
}

impl<const N: usize> From<[&str; N]> for BindingConfig {
    fn from(value: [&str; N]) -> Self {
        Self::Many(value.iter().map(|entry| (*entry).to_owned()).collect())
    }
}

/// Declare every bindable action once, and expand it into the action enum, the
/// `[keys]` configuration struct, and the default bindings.
macro_rules! key_actions {
    ($($variant:ident / $field:ident = $default:expr),* $(,)?) => {
        /// One named thing a key can be bound to.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum KeyAction {
            $($variant),*
        }

        impl KeyAction {
            /// Every action, in declaration order.
            pub const ALL: &'static [Self] = &[$(Self::$variant),*];

            /// The `[keys]` field that binds this action.
            pub fn field_name(self) -> &'static str {
                match self {
                    $(Self::$variant => stringify!($field)),*
                }
            }
        }

        /// The `[keys]` section of `config.toml`.
        #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(default, deny_unknown_fields)]
        pub struct KeysConfig {
            pub prefix: String,
            $(pub $field: BindingConfig),*
        }

        impl Default for KeysConfig {
            fn default() -> Self {
                Self {
                    prefix: DEFAULT_PREFIX.to_owned(),
                    $($field: BindingConfig::from($default)),*
                }
            }
        }

        impl KeysConfig {
            pub fn is_default(&self) -> bool {
                self == &Self::default()
            }

            /// The configured value for one action.
            pub fn field(&self, action: KeyAction) -> &BindingConfig {
                match action {
                    $(KeyAction::$variant => &self.$field),*
                }
            }
        }
    };
}

key_actions! {
    Help / help = "prefix+?",
    OpenSettings / settings = "prefix+s",
    Detach / detach = "prefix+q",
    NewSession / new_session = "prefix+c",
    Resume / resume = "prefix+g",
    WorkspaceManager / workspace_manager = "prefix+shift+n",
    FocusWorkspaces / focus_workspaces = "prefix+w",
    NextWorkspace / next_workspace = "prefix+n",
    PreviousWorkspace / previous_workspace = "prefix+p",
    SwitchWorkspace / switch_workspace = "prefix+1..9",
    NextPane / next_pane = "prefix+tab",
    PreviousPane / previous_pane = "prefix+shift+tab",
    PaneSize / pane_size = "prefix+z",
    PanePreset / pane_preset = "prefix+b",
    Refresh / refresh = "prefix+shift+r",
    Palette / palette = "prefix+:",
    CancelOperation / cancel_operation = "prefix+shift+c",
    MarkAllRead / mark_all_read = "prefix+a",
    WebViewer / web_viewer = "prefix+u",
    RenameSession / rename_session = "prefix+shift+t",
    ToggleTranscriptRendering / toggle_transcript_rendering = "prefix+t",
    ToggleDictation / toggle_dictation = "prefix+m",
    StopSession / stop_session = "",
    RestartSession / restart_session = "",
    MoveSession / move_session = "",
    DeleteSession / delete_session = "",
    ContainerSettings / container_settings = "",
    ManageProfiles / manage_profiles = "",
    ManageTargets / manage_targets = "",
    ChangeGoSetup / change_go_setup = "",
    CycleSpinner / cycle_spinner = "",
}

/// Whether a binding fires on its own or only after the prefix key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Trigger {
    Direct,
    Prefix,
}

/// One resolved binding. `index` carries the 0-based slot of a `1..9` range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Binding {
    pub trigger: Trigger,
    pub combo: KeyCombo,
    pub index: Option<usize>,
}

/// What a pressed key means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyMatch {
    pub action: KeyAction,
    pub index: Option<usize>,
}

/// The bindings in force: the prefix key and every action's resolved keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keybinds {
    pub prefix: KeyCombo,
    actions: BTreeMap<KeyAction, Vec<Binding>>,
}

impl Keybinds {
    /// The keys bound to one action, in the order they were written.
    pub fn bindings(&self, action: KeyAction) -> &[Binding] {
        self.actions
            .get(&action)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// The action a key runs on its own, with no prefix pending.
    pub fn resolve_direct(&self, combo: KeyCombo) -> Option<KeyMatch> {
        self.lookup(Trigger::Direct, combo)
    }

    /// The action a key runs while the prefix is pending.
    pub fn resolve_prefix(&self, combo: KeyCombo) -> Option<KeyMatch> {
        self.lookup(Trigger::Prefix, combo)
    }

    fn lookup(&self, trigger: Trigger, combo: KeyCombo) -> Option<KeyMatch> {
        let combo = normalize_key_combo(combo);
        self.actions.iter().find_map(|(action, bindings)| {
            bindings
                .iter()
                .find(|binding| binding.trigger == trigger && binding.combo == combo)
                .map(|binding| KeyMatch {
                    action: *action,
                    index: binding.index,
                })
        })
    }

    /// Labels for the user interface: `"ctrl+b c"` for a prefix binding and the
    /// plain combination for a direct one.
    pub fn labels(&self, action: KeyAction) -> Vec<String> {
        let prefix = self.prefix_label();
        self.bindings(action)
            .iter()
            .map(|binding| match binding.trigger {
                Trigger::Prefix => format!("{prefix} {}", format_key_combo(binding.combo)),
                Trigger::Direct => format_key_combo(binding.combo),
            })
            .collect()
    }

    /// The keys that follow the prefix, without the prefix itself: `"c"`.
    pub fn prefix_rhs_labels(&self, action: KeyAction) -> Vec<String> {
        self.bindings(action)
            .iter()
            .filter(|binding| binding.trigger == Trigger::Prefix)
            .map(|binding| format_key_combo(binding.combo))
            .collect()
    }

    /// The prefix key as it is written and displayed: `"ctrl+b"`.
    pub fn prefix_label(&self) -> String {
        format_key_combo(self.prefix)
    }
}

impl Default for Keybinds {
    fn default() -> Self {
        KeysConfig::default()
            .resolve()
            .expect("the default key bindings resolve without conflicts")
    }
}

fn key_error(field: &str, raw: &str, reason: impl fmt::Display) -> anyhow::Error {
    anyhow!("keys.{field} = \"{raw}\": {reason}")
}

/// Modifiers of a `1..9` range form, such as `1..9` itself or `ctrl+1..9`.
fn parse_range_modifiers(text: &str) -> Option<Modifiers> {
    let mut modifiers = Modifiers::NONE;
    let mut saw_range = false;
    for part in text.split('+') {
        let token = part.trim();
        if token == "1..9" {
            if saw_range {
                return None;
            }
            saw_range = true;
        } else {
            modifiers = modifiers | parse_modifier_token(token)?;
        }
    }
    saw_range.then_some(modifiers)
}

impl KeysConfig {
    /// Turn the configured strings into the bindings the interface uses.
    ///
    /// Every failure is a configuration error, except that a binding the user
    /// wrote silently displaces a default binding on the same key.
    pub fn resolve(&self) -> Result<Keybinds> {
        let prefix = parse_key_combo(&self.prefix)
            .ok_or_else(|| key_error("prefix", &self.prefix, "not a valid key combination"))?;
        let modified = prefix.modifiers.ctrl || prefix.modifiers.alt || prefix.modifiers.super_;
        if !modified && !matches!(prefix.name, KeyName::F(_)) {
            return Err(key_error(
                "prefix",
                &self.prefix,
                "the prefix must use ctrl, alt or super, or be a function key",
            ));
        }

        let defaults = Self::default();
        let mut actions: BTreeMap<KeyAction, Vec<Binding>> = BTreeMap::new();
        // (trigger, combo) -> the action that claimed it, and whether the user wrote it.
        let mut claimed: BTreeMap<(Trigger, KeyCombo), (KeyAction, bool)> = BTreeMap::new();

        // User fields first, so a default that collides with them can be dropped.
        for user_pass in [true, false] {
            for action in KeyAction::ALL.iter().copied() {
                let value = self.field(action);
                let is_user = value != defaults.field(action);
                if is_user != user_pass {
                    continue;
                }
                let field = action.field_name();
                for raw in value.entries() {
                    let (trigger, body) = match raw.strip_prefix("prefix+") {
                        Some(rest) => (Trigger::Prefix, rest),
                        None => (Trigger::Direct, raw),
                    };
                    for (combo, index) in expand_entry(action, field, raw, body)? {
                        check_combo(field, raw, trigger, combo, prefix)?;
                        match claimed.get(&(trigger, combo)) {
                            // A default yields to whatever the user bound.
                            Some((_, true)) if !is_user => continue,
                            Some((other, _)) => {
                                return Err(key_error(
                                    field,
                                    raw,
                                    format!("already bound by keys.{}", other.field_name()),
                                ));
                            }
                            None => {}
                        }
                        claimed.insert((trigger, combo), (action, is_user));
                        actions.entry(action).or_default().push(Binding {
                            trigger,
                            combo,
                            index,
                        });
                    }
                }
            }
        }

        Ok(Keybinds { prefix, actions })
    }
}

/// The combinations one entry stands for: one, or the nine of a `1..9` range.
fn expand_entry(
    action: KeyAction,
    field: &str,
    raw: &str,
    body: &str,
) -> Result<Vec<(KeyCombo, Option<usize>)>> {
    if let Some(modifiers) = parse_range_modifiers(body) {
        if action != KeyAction::SwitchWorkspace {
            return Err(key_error(
                field,
                raw,
                "the 1..9 range form is only valid for keys.switch_workspace",
            ));
        }
        return Ok((1..=9u32)
            .map(|digit| {
                let character = char::from_digit(digit, 10).expect("1..9 is a decimal digit");
                let combo = normalize_key_combo(KeyCombo::new(KeyName::Char(character), modifiers));
                (combo, Some(digit as usize - 1))
            })
            .collect());
    }
    let combo = parse_key_combo(body)
        .ok_or_else(|| key_error(field, raw, "not a valid key combination"))?;
    Ok(vec![(combo, None)])
}

fn check_combo(
    field: &str,
    raw: &str,
    trigger: Trigger,
    combo: KeyCombo,
    prefix: KeyCombo,
) -> Result<()> {
    let label = format_key_combo(combo);
    if trigger == Trigger::Direct {
        if combo == prefix {
            return Err(key_error(
                field,
                raw,
                "the prefix key cannot also be a direct binding",
            ));
        }
        if is_unmodified_printable(combo) {
            return Err(key_error(
                field,
                raw,
                format!(
                    "a direct binding needs a modifier, because the composer reads {label} as text; \
                     write \"prefix+{label}\" to bind it after the prefix"
                ),
            ));
        }
        if combo.modifiers == Modifiers::CTRL
            && matches!(combo.name, KeyName::Char('c') | KeyName::Char('v'))
        {
            return Err(key_error(
                field,
                raw,
                format!(
                    "{label} is reserved for cancel and paste; \
                     write \"prefix+{label}\" to bind it after the prefix"
                ),
            ));
        }
    }
    if trigger == Trigger::Prefix && combo == prefix {
        return Err(key_error(
            field,
            raw,
            format!("{label} after the prefix sends the literal prefix key and cannot be bound"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys_with(mutate: impl FnOnce(&mut KeysConfig)) -> KeysConfig {
        let mut keys = KeysConfig::default();
        mutate(&mut keys);
        keys
    }

    #[test]
    fn parse_key_combo_accepts_herdr_syntax_and_named_punctuation() {
        assert_eq!(
            parse_key_combo("ctrl+b"),
            Some(KeyCombo::new(KeyName::Char('b'), Modifiers::CTRL))
        );
        assert_eq!(
            parse_key_combo("shift+tab"),
            Some(KeyCombo::new(KeyName::Tab, Modifiers::SHIFT))
        );
        assert_eq!(parse_key_combo("f5"), Some(KeyCombo::plain(KeyName::F(5))));
        assert_eq!(
            parse_key_combo("slash"),
            Some(KeyCombo::plain(KeyName::Char('/')))
        );
        assert_eq!(
            parse_key_combo("space"),
            Some(KeyCombo::plain(KeyName::Char(' ')))
        );
        assert_eq!(
            parse_key_combo("return"),
            Some(KeyCombo::plain(KeyName::Enter))
        );
        assert_eq!(
            parse_key_combo("option+pagedown"),
            Some(KeyCombo::new(
                KeyName::PageDown,
                Modifiers {
                    alt: true,
                    ..Modifiers::NONE
                }
            ))
        );

        // An uppercase letter is the lowercase letter plus shift, and shift on
        // a symbol is dropped because the terminal sent the shifted character.
        assert_eq!(parse_key_combo("N"), parse_key_combo("shift+n"));
        assert_eq!(parse_key_combo("shift+?"), parse_key_combo("?"));

        for invalid in [
            "",
            "ctrl+",
            "ctrl",
            "ctrl+a+b",
            "f13",
            "nosuchkey",
            "ctrl++",
        ] {
            assert!(parse_key_combo(invalid).is_none(), "{invalid}");
        }
    }

    #[test]
    fn format_key_combo_round_trips_every_default_binding() {
        let defaults = KeysConfig::default();
        assert_eq!(
            format_key_combo(parse_key_combo(&defaults.prefix).expect("prefix parses")),
            defaults.prefix
        );
        for action in KeyAction::ALL.iter().copied() {
            for raw in defaults.field(action).entries() {
                let body = raw.strip_prefix("prefix+").unwrap_or(raw);
                if parse_range_modifiers(body).is_some() {
                    continue;
                }
                let combo = parse_key_combo(body).unwrap_or_else(|| panic!("{raw} parses"));
                assert_eq!(format_key_combo(combo), body, "{raw}");
            }
        }
    }

    #[test]
    fn default_keybinds_resolve_without_conflicts() {
        let keybinds = Keybinds::default();
        assert_eq!(keybinds.prefix_label(), DEFAULT_PREFIX);
        assert_eq!(
            keybinds.labels(KeyAction::NewSession),
            vec!["ctrl+b c".to_owned()]
        );
        assert_eq!(
            keybinds.prefix_rhs_labels(KeyAction::NextPane),
            vec!["tab".to_owned()]
        );
        assert!(keybinds.bindings(KeyAction::StopSession).is_empty());

        let create = parse_key_combo("c").expect("c parses");
        assert_eq!(
            keybinds.resolve_prefix(create),
            Some(KeyMatch {
                action: KeyAction::NewSession,
                index: None
            })
        );
        assert_eq!(keybinds.resolve_direct(create), None);
    }

    #[test]
    fn a_user_binding_displaces_the_default_it_collides_with_silently() {
        let keys = keys_with(|keys| keys.mark_all_read = "prefix+g".into());
        let keybinds = keys.resolve().expect("the default gives way");

        let goto = parse_key_combo("g").expect("g parses");
        assert_eq!(
            keybinds.resolve_prefix(goto),
            Some(KeyMatch {
                action: KeyAction::MarkAllRead,
                index: None
            })
        );
        assert!(keybinds.bindings(KeyAction::Resume).is_empty());
        // The displaced action keeps its other keys, if it had any.
        assert_eq!(keybinds.labels(KeyAction::MarkAllRead), vec!["ctrl+b g"]);
    }

    #[test]
    fn two_user_bindings_on_one_trigger_fail_validation() {
        let keys = keys_with(|keys| {
            keys.help = "prefix+space".into();
            keys.pane_preset = "prefix+space".into();
        });
        let error = keys
            .resolve()
            .expect_err("one key, two actions")
            .to_string();
        assert!(
            error.contains("keys.pane_preset = \"prefix+space\""),
            "{error}"
        );
        assert!(error.contains("keys.help"), "{error}");
    }

    #[test]
    fn an_unmodified_printable_direct_binding_fails_validation() {
        let keys = keys_with(|keys| keys.refresh = "r".into());
        let error = keys
            .resolve()
            .expect_err("plain letters are text")
            .to_string();
        assert!(error.starts_with("keys.refresh = \"r\": "), "{error}");
        assert!(error.contains("prefix+r"), "{error}");

        // The same key after the prefix is fine.
        keys_with(|keys| keys.refresh = "prefix+r".into())
            .resolve()
            .expect("a prefix binding on a letter is allowed");
    }

    #[test]
    fn ctrl_c_and_ctrl_v_cannot_be_direct_bindings() {
        for raw in ["ctrl+c", "ctrl+v"] {
            let keys = keys_with(|keys| keys.cancel_operation = raw.into());
            let error = keys.resolve().expect_err("reserved key").to_string();
            assert!(
                error.contains(&format!("keys.cancel_operation = \"{raw}\"")),
                "{error}"
            );
            assert!(error.contains("reserved"), "{error}");

            keys_with(|keys| keys.cancel_operation = format!("prefix+{raw}").as_str().into())
                .resolve()
                .expect("the prefix form is allowed");
        }
    }

    #[test]
    fn a_prefix_that_is_not_a_modified_chord_fails_validation() {
        let error = keys_with(|keys| keys.prefix = "b".to_owned())
            .resolve()
            .expect_err("an unmodified prefix would swallow typing")
            .to_string();
        assert!(error.starts_with("keys.prefix = \"b\": "), "{error}");

        let error = keys_with(|keys| keys.prefix = "ctrl+nope".to_owned())
            .resolve()
            .expect_err("unparseable prefix")
            .to_string();
        assert!(error.contains("not a valid key combination"), "{error}");

        for valid in ["ctrl+space", "alt+x", "f12"] {
            keys_with(|keys| keys.prefix = valid.to_owned())
                .resolve()
                .unwrap_or_else(|error| panic!("{valid} is a usable prefix: {error}"));
        }
    }

    #[test]
    fn binding_the_prefix_itself_after_the_prefix_fails_validation() {
        let keys = keys_with(|keys| keys.help = "prefix+ctrl+b".into());
        let error = keys
            .resolve()
            .expect_err("the doubled prefix sends the literal key")
            .to_string();
        assert!(error.contains("keys.help = \"prefix+ctrl+b\""), "{error}");
        assert!(error.contains("literal prefix"), "{error}");
    }

    #[test]
    fn the_prefix_key_cannot_be_a_direct_binding() {
        let keys = keys_with(|keys| keys.refresh = "ctrl+b".into());
        let error = keys
            .resolve()
            .expect_err("the prefix key is not a direct binding")
            .to_string();
        assert!(error.contains("keys.refresh = \"ctrl+b\""), "{error}");
        assert!(
            error.contains("the prefix key cannot also be a direct binding"),
            "{error}"
        );

        // Moving the prefix frees the old key for a direct binding.
        keys_with(|keys| {
            keys.prefix = "ctrl+space".to_owned();
            keys.refresh = "ctrl+b".into();
        })
        .resolve()
        .expect("ctrl+b is bindable once it is no longer the prefix");
    }

    #[test]
    fn an_empty_string_unbinds_a_default() {
        let keys = keys_with(|keys| keys.help = "".into());
        let keybinds = keys.resolve().expect("an empty value is legal");
        assert!(keybinds.bindings(KeyAction::Help).is_empty());
        assert!(keybinds.labels(KeyAction::Help).is_empty());
        assert_eq!(
            keybinds.resolve_prefix(parse_key_combo("?").expect("? parses")),
            None
        );
    }

    #[test]
    fn a_range_binding_is_only_valid_for_switch_workspace() {
        let error = keys_with(|keys| keys.next_workspace = "prefix+1..9".into())
            .resolve()
            .expect_err("only workspace selection is indexed")
            .to_string();
        assert!(
            error.contains("keys.next_workspace = \"prefix+1..9\""),
            "{error}"
        );
        assert!(error.contains("switch_workspace"), "{error}");
    }

    #[test]
    fn switch_workspace_range_yields_nine_indexed_bindings() {
        let keybinds = Keybinds::default();
        let bindings = keybinds.bindings(KeyAction::SwitchWorkspace);
        assert_eq!(bindings.len(), 9);
        for (slot, binding) in bindings.iter().enumerate() {
            assert_eq!(binding.trigger, Trigger::Prefix);
            assert_eq!(binding.index, Some(slot));
        }
        assert_eq!(
            keybinds.resolve_prefix(parse_key_combo("3").expect("3 parses")),
            Some(KeyMatch {
                action: KeyAction::SwitchWorkspace,
                index: Some(2)
            })
        );
        assert_eq!(
            keybinds
                .prefix_rhs_labels(KeyAction::SwitchWorkspace)
                .first(),
            Some(&"1".to_owned())
        );
    }
}
