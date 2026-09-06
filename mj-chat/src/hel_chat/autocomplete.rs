//! Slash commands: what they parse to, what the popup offers, and how a
//! chosen completion lands back in the composer.

use agent_client_protocol::schema::v1::{AvailableCommandInput, SessionConfigOption};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem};

use hel::hel_acp::{SessionConfigChoice, session_config_choices};
use hel::hel_transcript::{ChatEntry, ChatRole};

use super::ChatState;
use super::rendering::truncate_to_width;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LocalCommand {
    Help,
    Detach,
    Model,
    Effort,
    Fast,
    Plan,
    Implement,
    Review,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandSource {
    Hel,
    Agent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CommandChoice {
    name: String,
    description: String,
    input_hint: Option<String>,
    source: CommandSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutocompleteKind {
    Commands,
    ConfigValues { key: &'static str },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Autocomplete {
    kind: AutocompleteKind,
    selected: usize,
    matches: Vec<usize>,
}

impl ChatState {
    pub(super) fn move_autocomplete(&mut self, delta: isize) {
        let Some(autocomplete) = self.autocomplete.as_mut() else {
            return;
        };
        let len = autocomplete.matches.len();
        if len == 0 {
            return;
        }
        autocomplete.selected = if delta.is_negative() {
            autocomplete.selected.checked_sub(1).unwrap_or(len - 1)
        } else {
            (autocomplete.selected + 1) % len
        };
    }

    pub(super) fn accept_autocomplete(&mut self) -> bool {
        let Some(autocomplete) = self.autocomplete.clone() else {
            return false;
        };
        let Some(&index) = autocomplete.matches.get(autocomplete.selected) else {
            return false;
        };
        let value = match autocomplete.kind {
            AutocompleteKind::Commands => self
                .command_choices
                .get(index)
                .map(|command| format!("/{} ", command.name)),
            AutocompleteKind::ConfigValues { key: "model" } => self
                .model_values
                .get(index)
                .map(|choice| format!("/model {}", choice.value)),
            AutocompleteKind::ConfigValues { key: "effort" } => self
                .effort_values
                .get(index)
                .map(|choice| format!("/effort {}", choice.value)),
            AutocompleteKind::ConfigValues { .. } => None,
        };
        let Some(value) = value else {
            return false;
        };
        self.set_input(value);
        self.autocomplete = None;
        true
    }

    #[cfg(test)]
    pub(super) fn lists_command(&self, name: &str) -> bool {
        self.command_choices
            .iter()
            .any(|command| command.name == name)
    }

    pub(super) fn update_autocomplete(&mut self) {
        if !self.input_images.is_empty() {
            self.autocomplete = None;
            return;
        }
        if self.history_search.is_some() || self.input_cursor != self.input.len() {
            self.autocomplete = None;
            return;
        }
        for (prefix, key, values) in [
            ("/model ", "model", &self.model_values),
            ("/effort ", "effort", &self.effort_values),
        ] {
            if let Some(query) = self.input.strip_prefix(prefix) {
                // A bare command submits into the full value selector; the
                // inline popup only completes a partially typed value.
                if query.is_empty() {
                    self.autocomplete = None;
                    return;
                }
                // An advertised value is already a complete command. Leaving
                // its popup open makes Enter accept the text without
                // submitting it, and a concurrent session refresh can reopen
                // the popup immediately after acceptance.
                if values.iter().any(|choice| choice.value == query) {
                    self.autocomplete = None;
                    return;
                }
                let matches = matching_indices(values, query, |choice| {
                    (&choice.value, Some(choice.name.as_str()))
                });
                self.autocomplete = (!matches.is_empty()).then_some(Autocomplete {
                    kind: AutocompleteKind::ConfigValues { key },
                    selected: 0,
                    matches,
                });
                return;
            }
        }
        let Some(query) = self.input.strip_prefix('/') else {
            self.autocomplete = None;
            return;
        };
        if query.contains(char::is_whitespace) {
            self.autocomplete = None;
            return;
        }
        // A fully typed command is ready to submit. Leaving its popup open
        // would make Enter re-complete the text instead of running it, which
        // matters most for the bare /model and /effort selectors.
        if self
            .command_choices
            .iter()
            .any(|command| command.name == query)
        {
            self.autocomplete = None;
            return;
        }
        let matches = matching_indices(&self.command_choices, query, |command| {
            (&command.name, Some(command.description.as_str()))
        });
        self.autocomplete = (!matches.is_empty()).then_some(Autocomplete {
            kind: AutocompleteKind::Commands,
            selected: 0,
            matches,
        });
    }

    pub(super) fn rebuild_command_choices(&mut self) {
        let mut commands = builtin_command_choices();
        if self.supports_fast_mode() {
            commands.push(CommandChoice {
                name: "fast".to_owned(),
                description: "toggle Codex Fast mode".to_owned(),
                input_hint: None,
                source: CommandSource::Hel,
            });
        }
        if self.supports_plan_mode() {
            commands.push(CommandChoice {
                name: "plan".to_owned(),
                description: "toggle plan mode".to_owned(),
                input_hint: Some("message".to_owned()),
                source: CommandSource::Hel,
            });
            commands.push(CommandChoice {
                name: "implement".to_owned(),
                description: "leave plan mode and implement".to_owned(),
                input_hint: Some("instruction".to_owned()),
                source: CommandSource::Hel,
            });
        }
        for command in self.acp_surface.agent_commands() {
            let name = command.name.trim();
            if name.is_empty()
                || matches!(
                    name.to_ascii_lowercase().as_str(),
                    "fast" | "plan" | "implement"
                )
                || commands
                    .iter()
                    .any(|existing| existing.name.eq_ignore_ascii_case(name))
            {
                continue;
            }
            let input_hint = command.input.as_ref().and_then(|input| match input {
                AvailableCommandInput::Unstructured(input) => Some(input.hint.clone()),
                _ => None,
            });
            commands.push(CommandChoice {
                name: name.to_owned(),
                description: command.description.trim().to_owned(),
                input_hint,
                source: CommandSource::Agent,
            });
        }
        self.command_choices = commands;
        self.update_autocomplete();
    }

    pub(super) fn set_config_options(&mut self, options: &[SessionConfigOption]) {
        self.acp_surface.set_config_options(options);
        self.model_values = session_config_choices(options, "model");
        self.effort_values = session_config_choices(options, "effort");
        self.rebuild_command_choices();
    }

    pub(super) fn show_help(&mut self) {
        let commands = self
            .command_choices
            .iter()
            .map(|command| {
                let hint = command
                    .input_hint
                    .as_deref()
                    .map(|hint| format!(" <{hint}>"))
                    .unwrap_or_default();
                let source = match command.source {
                    CommandSource::Hel => "mj",
                    CommandSource::Agent => "agent",
                };
                format!(
                    "/{name}{hint} — {description} [{source}]",
                    name = command.name,
                    description = command.description
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        self.entries.push(ChatEntry::plain(
            self.latest_seq,
            ChatRole::System,
            format!("Clipboard: Ctrl-V paste text/image (Ctrl-Alt-V if intercepted by your terminal) · Backspace/Delete remove image markers · Ctrl-Alt-R restore a failed submission (empty composer)\n\nAvailable commands:\n!<command> — run a Bash command in this session [mj]\n{commands}"),
        ));
    }
}

pub(super) fn matching_indices<T>(
    values: &[T],
    query: &str,
    fields: impl Fn(&T) -> (&str, Option<&str>),
) -> Vec<usize> {
    let query = query.to_lowercase();
    let prefix = values
        .iter()
        .enumerate()
        .filter_map(|(index, value)| {
            fields(value)
                .0
                .to_lowercase()
                .starts_with(&query)
                .then_some(index)
        })
        .collect::<Vec<_>>();
    if !prefix.is_empty() {
        return prefix;
    }
    values
        .iter()
        .enumerate()
        .filter_map(|(index, value)| {
            let (primary, secondary) = fields(value);
            (primary.to_lowercase().contains(&query)
                || secondary.is_some_and(|secondary| secondary.to_lowercase().contains(&query)))
            .then_some(index)
        })
        .collect()
}

pub(super) fn builtin_command_choices() -> Vec<CommandChoice> {
    [
        ("help", "show available Mjolnir and agent commands", None),
        ("detach", "leave Mjolnir without stopping the worker", None),
        (
            "model",
            "change the active model, queued while the agent is busy",
            Some("value"),
        ),
        (
            "effort",
            "change the active reasoning effort, queued while the agent is busy",
            Some("value"),
        ),
        (
            "review",
            "review the finished turn now, or report how review is configured",
            Some("status"),
        ),
    ]
    .into_iter()
    .map(|(name, description, input_hint)| CommandChoice {
        name: name.to_owned(),
        description: description.to_owned(),
        input_hint: input_hint.map(str::to_owned),
        source: CommandSource::Hel,
    })
    .collect()
}

pub(super) fn parse_local_command(prompt: &str) -> Option<(LocalCommand, &str)> {
    let (name, args) = parse_slash_command(prompt)?;
    let command = match name {
        "help" => LocalCommand::Help,
        "detach" => LocalCommand::Detach,
        "model" => LocalCommand::Model,
        "effort" => LocalCommand::Effort,
        "fast" => LocalCommand::Fast,
        "plan" => LocalCommand::Plan,
        "implement" => LocalCommand::Implement,
        "review" => LocalCommand::Review,
        _ => return None,
    };
    Some((command, args))
}

pub(super) fn prompt_invokes_command(prompt: &str, expected: &str) -> bool {
    parse_slash_command(prompt).is_some_and(|(name, _)| name == expected)
}

fn parse_slash_command(prompt: &str) -> Option<(&str, &str)> {
    let command = prompt.strip_prefix('/')?;
    Some(
        command
            .split_once(char::is_whitespace)
            .map_or((command, ""), |(name, args)| (name, args.trim())),
    )
}

/// Draws the popup over the prompt and reports the rows it covers, so the
/// caller can register them as a selectable surface.
pub(super) fn render_autocomplete(
    frame: &mut Frame,
    prompt_area: Rect,
    chat: &ChatState,
) -> Option<Rect> {
    let autocomplete = chat.autocomplete.as_ref()?;
    let visible = autocomplete.matches.len().min(8);
    if visible == 0 {
        return None;
    }
    let height = (visible as u16).saturating_add(2);
    let area = Rect::new(
        prompt_area.x,
        prompt_area.y.saturating_sub(height),
        prompt_area.width,
        height,
    );
    frame.render_widget(Clear, area);
    let title = match autocomplete.kind {
        AutocompleteKind::Commands => " commands · ↑/↓ select · Tab/Enter accept ",
        AutocompleteKind::ConfigValues { .. } => " values · ↑/↓ select · Tab/Enter accept ",
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let start = autocomplete
        .selected
        .saturating_sub(visible.saturating_sub(1));
    let items = autocomplete.matches[start..]
        .iter()
        .take(visible)
        .enumerate()
        .filter_map(|(offset, index)| {
            let selected = start + offset == autocomplete.selected;
            autocomplete_row(chat, autocomplete.kind, *index).map(|row| {
                ListItem::new(truncate_to_width(&row, usize::from(inner.width))).style(
                    if selected {
                        Style::default()
                            .fg(Color::Black)
                            .bg(Color::White)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    },
                )
            })
        })
        .collect::<Vec<_>>();
    frame.render_widget(List::new(items), inner);
    Some(inner)
}

fn autocomplete_row(chat: &ChatState, kind: AutocompleteKind, index: usize) -> Option<String> {
    match kind {
        AutocompleteKind::Commands => {
            let command = chat.command_choices.get(index)?;
            let hint = command
                .input_hint
                .as_deref()
                .map(|hint| format!(" <{hint}>"))
                .unwrap_or_default();
            let source = match command.source {
                CommandSource::Hel => "mj",
                CommandSource::Agent => "agent",
            };
            Some(format!(
                "/{}{hint}  — {} [{source}]",
                command.name, command.description
            ))
        }
        AutocompleteKind::ConfigValues { key: "model" } => {
            config_value_row(chat.model_values.get(index)?)
        }
        AutocompleteKind::ConfigValues { key: "effort" } => {
            config_value_row(chat.effort_values.get(index)?)
        }
        AutocompleteKind::ConfigValues { .. } => None,
    }
}

pub(super) fn config_value_row(choice: &SessionConfigChoice) -> Option<String> {
    let description = choice
        .description
        .as_deref()
        .filter(|description| !description.trim().is_empty())
        .map(|description| format!(" — {description}"))
        .unwrap_or_default();
    Some(format!("{} ({}){description}", choice.name, choice.value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hel_chat::ChatAction;
    use crate::hel_chat::test_support::{
        advertise, fast_mode_option, grok_chat, key, mode_config_option, snapshot,
    };
    use crossterm::event::KeyCode;

    #[test]
    fn local_command_parser_requires_an_exact_command_boundary() {
        assert_eq!(parse_local_command("/checkpoint before refactor"), None);
        assert_eq!(parse_local_command("/checkpointing"), None);
        assert_eq!(parse_local_command("explain /checkpoint"), None);
        assert!(prompt_invokes_command("/goal finish it", "goal"));
        assert!(!prompt_invokes_command("/Goal finish it", "goal"));
        assert!(!prompt_invokes_command("/goalkeeper", "goal"));
    }

    /// Tab has two jobs now: finish a completion, and hand the keyboard to
    /// the next pane. An open popup wins, so a Tab meant for the completion
    /// can never move focus out from under it.
    #[test]
    fn tab_accepts_an_open_completion_before_it_cycles_focus() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.handle_key(key(KeyCode::Char('/')));
        chat.handle_key(key(KeyCode::Char('h')));
        assert!(chat.autocomplete.is_some(), "the popup is open");

        assert_eq!(chat.handle_key(key(KeyCode::Tab)), ChatAction::None);
        assert!(chat.autocomplete.is_none(), "the popup was accepted");
        assert_eq!(chat.input, "/help ");

        // With nothing to complete, the same key is the handle on the next
        // pane.
        assert_eq!(
            chat.handle_key(key(KeyCode::Tab)),
            ChatAction::CycleFocus { reverse: false }
        );
        assert_eq!(
            chat.handle_key(key(KeyCode::BackTab)),
            ChatAction::CycleFocus { reverse: true }
        );
    }

    /// The composer's word-kill and cursor keys used to be eaten before it
    /// saw them: Ctrl-W by the workspace picker, Ctrl-B by the web dialog.
    /// Both accelerators are the composer's again.
    #[test]
    fn the_composer_keeps_control_w_and_control_b() {
        use crossterm::event::{KeyEvent, KeyModifiers};

        let mut chat = ChatState::new(&snapshot(), &[]);
        for character in "one two".chars() {
            chat.handle_key(key(KeyCode::Char(character)));
        }

        let control =
            |character: char| KeyEvent::new(KeyCode::Char(character), KeyModifiers::CONTROL);
        assert_eq!(chat.handle_key(control('w')), ChatAction::None);
        assert_eq!(chat.input, "one ", "Ctrl-W kills the previous word");

        assert_eq!(chat.handle_key(control('b')), ChatAction::None);
        assert_eq!(
            chat.input_cursor, 3,
            "Ctrl-B steps the cursor back one character"
        );
    }

    #[test]
    fn autocomplete_merges_agent_commands_without_overriding_hel_commands() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.apply_session_update(
            1,
            &serde_json::json!({
                "sessionUpdate": "available_commands_update",
                "availableCommands": [
                    {"name": "compact", "description": "agent compact", "input": {"hint": "scope"}},
                    {"name": "review", "description": "agent review"},
                    {"name": "help", "description": "agent help"}
                ]
            }),
        );
        assert!(chat.command_choices.iter().any(|command| {
            command.name == "compact" && command.source == CommandSource::Agent
        }));
        // `/review` is Hel's: it opens the turn-review pane rather than
        // reaching the agent, so an agent command of the same name does not
        // replace it.
        assert_eq!(
            chat.command_choices
                .iter()
                .filter(|command| command.name == "review")
                .map(|command| command.source)
                .collect::<Vec<_>>(),
            vec![CommandSource::Hel]
        );
        assert_eq!(
            chat.command_choices
                .iter()
                .filter(|command| command.name == "help")
                .count(),
            1
        );

        chat.set_input("/rev".into());
        assert!(chat.accept_autocomplete());
        assert_eq!(chat.input, "/review ");
    }

    #[test]
    fn command_updates_replace_stale_adapter_capabilities() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        for available_commands in [
            serde_json::json!([
                {"name": "plan", "description": "toggle plan mode"},
                {"name": "goal", "description": "set a persistent goal"}
            ]),
            serde_json::json!([
                {"name": "plan", "description": "toggle plan mode"}
            ]),
        ] {
            chat.apply_session_update(
                1,
                &serde_json::json!({
                    "sessionUpdate": "available_commands_update",
                    "availableCommands": available_commands
                }),
            );
        }

        assert!(
            !chat
                .command_choices
                .iter()
                .any(|command| command.name == "plan")
        );
        assert!(
            !chat
                .command_choices
                .iter()
                .any(|command| command.name == "goal")
        );
    }

    #[test]
    fn advertised_plan_is_owned_by_hel_while_other_agent_commands_are_forwarded() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.apply_session_update(
            1,
            &serde_json::json!({
                "sessionUpdate": "available_commands_update",
                "availableCommands": [
                    {"name": "plan", "description": "toggle plan mode"},
                    {"name": "goal", "description": "set a persistent goal", "input": {"hint": "objective"}}
                ]
            }),
        );
        assert!(
            !chat
                .command_choices
                .iter()
                .any(|command| command.name == "plan")
        );
        assert!(
            chat.command_choices
                .iter()
                .any(|command| command.name == "goal")
        );

        chat.input = "/plan".into();
        assert_eq!(chat.handle_key(key(KeyCode::Enter)), ChatAction::None);
        assert_eq!(chat.input, "/plan");

        chat.input = "/goal ship the release".into();
        assert_eq!(
            chat.handle_key(key(KeyCode::Enter)),
            ChatAction::Prompt("/goal ship the release".into())
        );
    }

    #[test]
    fn config_value_autocomplete_uses_advertised_acp_choices() {
        use agent_client_protocol::schema::v1::{
            SessionConfigOptionCategory, SessionConfigSelectOption, SessionConfigSelectOptions,
        };

        let options = vec![
            SessionConfigOption::select(
                "model",
                "Model",
                "auto",
                SessionConfigSelectOptions::Ungrouped(vec![
                    SessionConfigSelectOption::new("auto", "Auto"),
                    SessionConfigSelectOption::new("gpt-5.6-luna", "Luna"),
                ]),
            )
            .category(SessionConfigOptionCategory::Model),
        ];
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_config_options(&options);
        chat.set_input("/model lun".into());

        assert!(chat.accept_autocomplete());
        assert_eq!(chat.input, "/model gpt-5.6-luna");
        assert!(chat.autocomplete.is_none());
    }

    #[test]
    fn exact_config_value_is_ready_to_submit_during_session_refreshes() {
        use agent_client_protocol::schema::v1::{
            SessionConfigOptionCategory, SessionConfigSelectOption, SessionConfigSelectOptions,
        };

        let options = vec![
            SessionConfigOption::select(
                "effort",
                "Effort",
                "high",
                SessionConfigSelectOptions::Ungrouped(vec![
                    SessionConfigSelectOption::new("high", "Thinking High"),
                    SessionConfigSelectOption::new("max", "Thinking Max"),
                ]),
            )
            .category(SessionConfigOptionCategory::ThoughtLevel),
        ];
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_config_options(&options);
        chat.set_input("/effort ma".into());

        assert!(chat.autocomplete.is_some());
        assert_eq!(chat.handle_key(key(KeyCode::Enter)), ChatAction::None);
        assert_eq!(chat.input, "/effort max");
        assert!(chat.autocomplete.is_none());

        // A running session can publish another snapshot between key presses.
        chat.set_config_options(&options);
        assert!(chat.autocomplete.is_none());
        assert_eq!(
            chat.handle_key(key(KeyCode::Enter)),
            ChatAction::SetConfig {
                key: "effort".into(),
                value: "max".into(),
            }
        );
    }

    #[test]
    fn fast_is_a_hel_command_only_while_the_codex_selector_is_available() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        advertise(&mut chat, 1, &["fast"]);
        assert!(
            !chat
                .command_choices
                .iter()
                .any(|command| command.name == "fast")
        );

        chat.set_config_options(&[fast_mode_option("off")]);
        assert!(
            chat.command_choices
                .iter()
                .any(|command| { command.name == "fast" && command.source == CommandSource::Hel })
        );

        chat.set_config_options(&[]);
        assert!(
            !chat
                .command_choices
                .iter()
                .any(|command| command.name == "fast")
        );
    }

    #[test]
    fn plan_is_listed_as_a_hel_command_for_supported_surfaces() {
        let lists_plan = |chat: &ChatState| {
            chat.command_choices
                .iter()
                .any(|command| command.name == "plan" && command.source == CommandSource::Hel)
        };

        let mut chat = grok_chat();
        assert!(lists_plan(&chat));

        // The adapter's command never replaces Hel's cross-profile contract.
        advertise(&mut chat, 1, &["plan"]);
        assert!(lists_plan(&chat));

        assert!(!lists_plan(&ChatState::new(&snapshot(), &[])));

        let mut config_mode = ChatState::new(&snapshot(), &[]);
        config_mode.set_config_options(&[mode_config_option("default", &["default", "plan"])]);
        assert!(lists_plan(&config_mode));

        let mut deepseek = grok_chat();
        deepseek.set_harness_kind(hel::hel_config::HarnessKind::Deepseek);
        advertise(&mut deepseek, 2, &["plan", "implement"]);
        assert!(!lists_plan(&deepseek));
        assert!(
            !deepseek
                .command_choices
                .iter()
                .any(|command| { matches!(command.name.as_str(), "plan" | "implement") })
        );
    }
}
