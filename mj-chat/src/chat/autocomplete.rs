//! Slash commands: what they parse to, what the popup offers, and how a
//! chosen completion lands back in the composer.

use crate::theme;
use agent_client_protocol::schema::v1::{AvailableCommandInput, SessionConfigOption};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::{List, ListItem};

use mj_core::acp::{SessionConfigChoice, session_config_choices};
use mj_core::transcript::{ChatEntry, ChatRole};

use super::ChatState;
use super::rendering::truncate_to_width;
use crate::components::{AutocompletePopup, PopupSide};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LocalCommand {
    Clear,
    GoalControl(mj_core::goal::GoalControlAction),
    Help,
    Detach,
    Model,
    Effort,
    Fast,
    Plan,
    Implement,
    Review,
    Attach,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandSource {
    Hel,
    Agent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CommandChoice {
    pub(super) name: String,
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
        let selected = if delta.is_negative() {
            autocomplete.selected.checked_sub(1).unwrap_or(len - 1)
        } else {
            (autocomplete.selected + 1) % len
        };
        if autocomplete.selected != selected {
            autocomplete.selected = selected;
        }
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
        self.set_autocomplete(None);
        true
    }

    #[cfg(test)]
    pub(super) fn lists_command(&self, name: &str) -> bool {
        self.command_choices
            .iter()
            .any(|command| command.name == name)
    }

    pub(super) fn update_autocomplete(&mut self) {
        // A standby composer has no session to answer commands, and its host
        // does not run the overlay pass that draws the popup, so offering
        // completion would only open an invisible state that eats arrows.
        if self.standby {
            self.set_autocomplete(None);
            return;
        }
        if !self.input_images.is_empty() {
            self.set_autocomplete(None);
            return;
        }
        if self.history_search.is_some() || self.input_cursor != self.input.len() {
            self.set_autocomplete(None);
            return;
        }
        if let Some(query) = self.input.strip_prefix("/model ") {
            let next = value_autocomplete(query, &self.model_values, "model");
            self.set_autocomplete(next);
            return;
        }
        if let Some(query) = self.input.strip_prefix("/effort ") {
            let next = value_autocomplete(query, &self.effort_values, "effort");
            self.set_autocomplete(next);
            return;
        }
        let Some(query) = self.input.strip_prefix('/') else {
            self.set_autocomplete(None);
            return;
        };
        if query.contains(char::is_whitespace) && !query.starts_with("goal ") {
            self.set_autocomplete(None);
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
            self.set_autocomplete(None);
            return;
        }
        let matches = matching_indices(&self.command_choices, query, |command| {
            (&command.name, Some(command.description.as_str()))
        });
        self.set_autocomplete((!matches.is_empty()).then_some(Autocomplete {
            kind: AutocompleteKind::Commands,
            selected: 0,
            matches,
        }));
    }

    fn set_autocomplete(&mut self, autocomplete: Option<Autocomplete>) {
        if self.autocomplete != autocomplete {
            self.autocomplete = autocomplete;
        }
    }

    pub(super) fn rebuild_command_choices(&mut self) {
        let mut commands = builtin_command_choices();
        if self.clear_context_supported {
            commands.push(CommandChoice {
                name: "clear".into(),
                description: "start a new conversation, keeping workspace and history".into(),
                input_hint: None,
                source: CommandSource::Hel,
            });
        }
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
        for action in mj_core::goal::GoalControlAction::ALL {
            if self.goal_state.supports(action) {
                commands.push(CommandChoice {
                    name: format!("goal {}", action.as_str()),
                    description: match action {
                        mj_core::goal::GoalControlAction::Pause => {
                            "pause goal continuation, preserving the goal"
                        }
                        mj_core::goal::GoalControlAction::Resume => "resume the existing goal",
                        mj_core::goal::GoalControlAction::Clear => {
                            "remove the goal without interrupting the current turn"
                        }
                    }
                    .into(),
                    input_hint: None,
                    source: CommandSource::Hel,
                });
            }
        }
        for command in self.acp_surface.agent_commands() {
            let name = command.name.trim();
            if name.is_empty()
                || (matches!(
                    name.to_ascii_lowercase().as_str(),
                    "fast" | "plan" | "implement"
                ) && !(name.eq_ignore_ascii_case("plan")
                    && self.acp_surface.forwards_plan_command()))
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
        if self.command_choices != commands {
            self.command_choices = commands;
        }
        self.update_autocomplete();
    }

    pub(super) fn set_config_options(&mut self, options: &[SessionConfigOption]) {
        self.acp_surface.set_config_options(options);
        let model_values = session_config_choices(options, "model");
        let effort_values = session_config_choices(options, "effort");
        if self.model_values != model_values || self.effort_values != effort_values {
            self.model_values = model_values;
            self.effort_values = effort_values;
        }
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
            format!("Clipboard: {} paste text/image (Ctrl-Alt-V if intercepted by your terminal) · Backspace/Delete remove image markers · Ctrl-Alt-R restore a failed submission (empty composer)\n\nAvailable commands:\n!<command> — run a Bash command in this session [mj]\n{commands}", crate::clipboard::PASTE_SHORTCUT),
        ));
    }
}

fn value_autocomplete(
    query: &str,
    values: &[SessionConfigChoice],
    key: &'static str,
) -> Option<Autocomplete> {
    // A bare command submits into the full value selector; the inline popup
    // only completes a partially typed value.
    if query.is_empty() || values.iter().any(|choice| choice.value == query) {
        return None;
    }
    let matches = matching_indices(values, query, |choice| {
        (&choice.value, Some(choice.name.as_str()))
    });
    (!matches.is_empty()).then_some(Autocomplete {
        kind: AutocompleteKind::ConfigValues { key },
        selected: 0,
        matches,
    })
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
        (
            "attach",
            "add an image file (PNG, JPEG or WebP) to the prompt",
            Some("path"),
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
        "goal" => LocalCommand::GoalControl(mj_core::goal::GoalControlAction::parse(args)?),
        "clear" => LocalCommand::Clear,
        "help" => LocalCommand::Help,
        "detach" => LocalCommand::Detach,
        "model" => LocalCommand::Model,
        "effort" => LocalCommand::Effort,
        "fast" => LocalCommand::Fast,
        "plan" => LocalCommand::Plan,
        "implement" => LocalCommand::Implement,
        "review" => LocalCommand::Review,
        "attach" => LocalCommand::Attach,
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
    let title = match autocomplete.kind {
        AutocompleteKind::Commands => " commands · ↑/↓ select · Tab/Enter accept ",
        AutocompleteKind::ConfigValues { .. } => " values · ↑/↓ select · Tab/Enter accept ",
    };
    let (_, inner) = AutocompletePopup::render(
        frame,
        frame.area(),
        prompt_area,
        prompt_area.width,
        autocomplete.matches.len(),
        title,
        PopupSide::Above,
    )?;
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
                        theme::selection(true)
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

pub(super) fn config_choice_name(choice: &SessionConfigChoice) -> &str {
    if choice.name.trim().is_empty() {
        &choice.value
    } else {
        &choice.name
    }
}

pub(super) fn config_value_row(choice: &SessionConfigChoice) -> Option<String> {
    Some(config_choice_name(choice).to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::ChatAction;
    use crate::chat::test_support::{advertise, key, snapshot};
    use crossterm::event::KeyCode;

    #[test]
    fn config_rows_show_only_names_and_fall_back_for_blank_names() {
        let mut choice = SessionConfigChoice {
            value: "model-id".into(),
            name: "Model name".into(),
            description: Some("An explanation that should not appear".into()),
        };
        assert_eq!(config_value_row(&choice).as_deref(), Some("Model name"));
        choice.name = "  ".into();
        assert_eq!(config_value_row(&choice).as_deref(), Some("model-id"));
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

    fn goal_chat(actions: &[&str]) -> ChatState {
        let mut chat = ChatState::new(&snapshot(), &[]);
        advertise(&mut chat, 1, &["goal"]);
        chat.apply_session_update(2, &serde_json::json!({
            "sessionUpdate":"session_info_update", "_meta": {
                "mjGoalCapability":{"version":1,"controlMethod":"_session/goal","actions":actions},
                "goal":{"objective":"finish","status":"active"}
            }
        }));
        chat
    }

    #[test]
    fn goal_control_keeps_attached_drafts_and_bypasses_pending_plan_transition() {
        let mut chat = goal_chat(&["clear"]);
        chat.set_prompt_images_supported(true);
        chat.set_input("/goal clear ".into());
        assert!(chat.reserve_attachment(1));
        let draft = chat.input.clone();
        assert_eq!(chat.submit_input(), ChatAction::None);
        assert_eq!(chat.input, draft);
        assert_eq!(chat.input_images.len(), 1);
        assert!(
            chat.feedback
                .current()
                .unwrap()
                .contains("delete its marker")
        );
        let mut chat = goal_chat(&["clear"]);
        chat.plan_command_pending = true;
        chat.input = "/goal clear".into();
        assert_eq!(
            chat.submit_input(),
            ChatAction::GoalControl {
                action: mj_core::goal::GoalControlAction::Clear
            }
        );
    }
}
