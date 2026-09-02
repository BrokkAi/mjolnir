//! The `/model` and `/effort` selector: a modal over the chat listing every
//! value the harness advertises, filtered as the user types.

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use hel::hel_acp::SessionConfigChoice;
use hel::hel_worker::WorkerPhase;

use super::autocomplete::{config_value_row, matching_indices};
use super::rendering::truncate_to_width;
use super::{ChatAction, ChatState};

/// Modal state for choosing one advertised config value.
///
/// The choices are snapshotted when the picker opens so a concurrent session
/// refresh cannot reorder the list under the cursor; the accepted value is
/// still validated against the live options when the change is applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ConfigPicker {
    key: &'static str,
    choices: Vec<SessionConfigChoice>,
    current: Option<String>,
    /// Cursor position within `filtered`.
    selected: usize,
    query: String,
    /// Indices into `choices` that match `query`; all of them when empty.
    filtered: Vec<usize>,
}

impl ConfigPicker {
    fn new(key: &'static str, choices: Vec<SessionConfigChoice>, current: Option<String>) -> Self {
        let selected = current
            .as_deref()
            .and_then(|current| choices.iter().position(|choice| choice.value == current))
            .unwrap_or(0);
        let filtered = (0..choices.len()).collect();
        Self {
            key,
            choices,
            current,
            selected,
            query: String::new(),
            filtered,
        }
    }

    fn selection(&self) -> Option<&SessionConfigChoice> {
        self.filtered
            .get(self.selected)
            .and_then(|&index| self.choices.get(index))
    }

    fn move_selection(&mut self, delta: isize) {
        let len = self.filtered.len();
        if len == 0 {
            return;
        }
        self.selected = if delta.is_negative() {
            self.selected.checked_sub(1).unwrap_or(len - 1)
        } else {
            (self.selected + 1) % len
        };
    }

    /// Recomputes the matches, keeping the cursor on the same choice when it
    /// survives the new filter.
    fn refilter(&mut self) {
        let kept = self.filtered.get(self.selected).copied();
        self.filtered = if self.query.is_empty() {
            (0..self.choices.len()).collect()
        } else {
            matching_indices(&self.choices, &self.query, |choice| {
                (&choice.value, Some(choice.name.as_str()))
            })
        };
        self.selected = kept
            .and_then(|index| self.filtered.iter().position(|&i| i == index))
            .unwrap_or(0);
    }
}

impl ChatState {
    /// Opens the selector for `key`, or reports that the harness advertises
    /// no values to choose from.
    pub(super) fn open_config_picker(&mut self, key: &'static str) -> bool {
        let (choices, current) = match key {
            "model" => (
                self.model_values.clone(),
                self.current_model().map(str::to_owned),
            ),
            "effort" => (
                self.effort_values.clone(),
                self.current_effort().map(str::to_owned),
            ),
            _ => return false,
        };
        if choices.is_empty() {
            return false;
        }
        self.config_picker = Some(ConfigPicker::new(key, choices, current));
        true
    }

    pub(super) fn config_picker_active(&self) -> bool {
        self.config_picker.is_some()
    }

    /// Drives the selector from one key press. Every key belongs to the modal
    /// while it is up: printable characters build the filter rather than
    /// reaching the composer.
    pub(super) fn handle_config_picker_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> ChatAction {
        let Some(picker) = self.config_picker.as_mut() else {
            return ChatAction::None;
        };
        match code {
            KeyCode::Up => {
                picker.move_selection(-1);
                ChatAction::None
            }
            KeyCode::Down => {
                picker.move_selection(1);
                ChatAction::None
            }
            KeyCode::Esc => {
                self.config_picker = None;
                ChatAction::None
            }
            KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                self.config_picker = None;
                ChatAction::None
            }
            KeyCode::Enter | KeyCode::Tab => {
                let Some(choice) = picker.selection() else {
                    return ChatAction::None;
                };
                let key = picker.key;
                let value = choice.value.clone();
                self.config_picker = None;
                if matches!(self.phase, WorkerPhase::Closing | WorkerPhase::Closed) {
                    self.set_notice(
                        "The worker is closing; this configuration change was not sent",
                    );
                    return ChatAction::None;
                }
                // A busy agent does not refuse the change: it waits in the
                // command queue and applies when its turn comes.
                ChatAction::SetConfig {
                    key: key.to_owned(),
                    value,
                }
            }
            KeyCode::Backspace => {
                if picker.query.pop().is_some() {
                    picker.refilter();
                }
                ChatAction::None
            }
            KeyCode::Char(character)
                if !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                picker.query.push(character);
                picker.refilter();
                ChatAction::None
            }
            _ => ChatAction::None,
        }
    }
}

/// Rows of the list to draw so the cursor stays visible in the middle of a
/// long value list.
fn visible_range(total: usize, selected: usize, visible: usize) -> std::ops::Range<usize> {
    if total <= visible {
        return 0..total;
    }
    let start = selected
        .saturating_sub(visible / 2)
        .min(total.saturating_sub(visible));
    start..(start + visible)
}

/// Draws the selector over the chat and reports the rows it owns.
pub(super) fn render_config_picker(
    frame: &mut Frame,
    area: Rect,
    chat: &ChatState,
) -> Option<Rect> {
    let picker = chat.config_picker.as_ref()?;
    let visible = picker.filtered.len().clamp(1, 8);
    let rect = super::active::centered(area, 72, visible as u16 + 6);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" Choose a {} ", picker.key))
        .border_style(Style::default().fg(Color::LightMagenta));
    let inner = block.inner(rect);
    frame.render_widget(Clear, rect);
    frame.render_widget(block, rect);

    let dim = Style::default().fg(Color::DarkGray);
    let mut lines = vec![if picker.query.is_empty() {
        Line::from(Span::styled("filter: (type to filter)", dim))
    } else {
        Line::from(vec![
            Span::styled("filter: ", dim),
            Span::raw(picker.query.clone()),
        ])
    }];
    lines.push(Line::from(""));
    if picker.filtered.is_empty() {
        lines.push(Line::from(Span::styled("no matches", dim)));
    } else {
        let range = visible_range(
            picker.filtered.len(),
            picker.selected,
            usize::from(inner.height.saturating_sub(4)).max(1),
        );
        let start = range.start;
        for (offset, &index) in picker.filtered[range].iter().enumerate() {
            let Some(choice) = picker.choices.get(index) else {
                continue;
            };
            let selected = start + offset == picker.selected;
            let marker = if selected { "› " } else { "  " };
            let mut row = config_value_row(choice).unwrap_or_else(|| choice.value.clone());
            if picker.current.as_deref() == Some(choice.value.as_str()) {
                row.push_str("  (current)");
            }
            let style = if selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            lines.push(Line::from(Span::styled(
                truncate_to_width(&format!("{marker}{row}"), usize::from(inner.width)),
                style,
            )));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "↑/↓ choose · type to filter · Enter apply · Esc cancel",
        dim,
    )));
    frame.render_widget(Paragraph::new(lines), inner);
    Some(inner)
}

#[cfg(test)]
mod tests {
    use crate::hel_chat::ChatAction;
    use crate::hel_chat::ChatState;
    use crate::hel_chat::test_support::{drawn_transcript, key, snapshot};
    use agent_client_protocol::schema::v1::{
        SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelectOption,
        SessionConfigSelectOptions,
    };
    use crossterm::event::KeyCode;

    fn model_option(current: &str, values: &[(&str, &str)]) -> SessionConfigOption {
        SessionConfigOption::select(
            "model",
            "Model",
            current.to_owned(),
            SessionConfigSelectOptions::Ungrouped(
                values
                    .iter()
                    .map(|(value, name)| {
                        SessionConfigSelectOption::new((*value).to_owned(), (*name).to_owned())
                    })
                    .collect(),
            ),
        )
        .category(SessionConfigOptionCategory::Model)
    }

    fn effort_option(current: &str, values: &[&str]) -> SessionConfigOption {
        SessionConfigOption::select(
            "effort",
            "Effort",
            current.to_owned(),
            SessionConfigSelectOptions::Ungrouped(
                values
                    .iter()
                    .map(|value| {
                        SessionConfigSelectOption::new((*value).to_owned(), (*value).to_owned())
                    })
                    .collect(),
            ),
        )
        .category(SessionConfigOptionCategory::ThoughtLevel)
    }

    fn chat_with_models() -> ChatState {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_config_options(&[
            model_option(
                "gpt-5.6-luna",
                &[
                    ("auto", "Auto"),
                    ("gpt-5.6-luna", "Luna"),
                    ("gpt-5.6-terra", "Terra"),
                ],
            ),
            effort_option("high", &["low", "medium", "high", "max"]),
        ]);
        chat
    }

    #[test]
    fn bare_model_command_opens_the_selector_on_the_current_value() {
        let mut chat = chat_with_models();
        chat.set_input("/model".into());
        assert_eq!(chat.handle_key(key(KeyCode::Enter)), ChatAction::None);
        assert!(chat.config_picker_active());
        assert!(chat.input.is_empty(), "the composer was cleared");
        let picker = chat.config_picker.as_ref().unwrap();
        assert_eq!(
            picker.selection().map(|choice| choice.value.as_str()),
            Some("gpt-5.6-luna")
        );
    }

    #[test]
    fn a_completed_bare_command_still_submits_into_the_selector() {
        // Enter on "/mod" first accepts the command completion ("/model "),
        // and the value popup must not swallow the next Enter: with nothing
        // typed after the command, Enter opens the selector instead.
        let mut chat = chat_with_models();
        chat.set_input("/mod".into());
        assert_eq!(chat.handle_key(key(KeyCode::Enter)), ChatAction::None);
        assert_eq!(chat.input, "/model ");
        assert_eq!(chat.handle_key(key(KeyCode::Enter)), ChatAction::None);
        assert!(chat.config_picker_active());
    }

    #[test]
    fn typing_filters_choices_and_enter_applies_the_selection() {
        let mut chat = chat_with_models();
        assert!(chat.open_config_picker("model"));
        for character in "terra".chars() {
            chat.handle_key(key(KeyCode::Char(character)));
        }
        assert_eq!(
            chat.handle_key(key(KeyCode::Enter)),
            ChatAction::SetConfig {
                key: "model".into(),
                value: "gpt-5.6-terra".into(),
            }
        );
        assert!(!chat.config_picker_active());
    }

    #[test]
    fn effort_selector_wraps_and_escape_closes_without_a_change() {
        let mut chat = chat_with_models();
        chat.set_input("/effort".into());
        assert_eq!(chat.handle_key(key(KeyCode::Enter)), ChatAction::None);
        let picker = chat.config_picker.as_ref().unwrap();
        assert_eq!(
            picker.selection().map(|choice| choice.value.as_str()),
            Some("high")
        );
        // Down from "high" reaches "max"; another Down wraps to "low".
        chat.handle_key(key(KeyCode::Down));
        chat.handle_key(key(KeyCode::Down));
        assert_eq!(
            chat.config_picker
                .as_ref()
                .unwrap()
                .selection()
                .map(|choice| choice.value.as_str()),
            Some("low")
        );
        assert_eq!(chat.handle_key(key(KeyCode::Esc)), ChatAction::None);
        assert!(!chat.config_picker_active());
    }

    #[test]
    fn bare_command_without_advertised_values_reports_instead_of_opening() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_input("/model".into());
        assert_eq!(chat.handle_key(key(KeyCode::Enter)), ChatAction::None);
        assert!(!chat.config_picker_active());
        assert!(
            chat.notice()
                .is_some_and(|notice| notice.contains("does not advertise model values")),
            "the footer says why nothing opened"
        );
    }

    #[test]
    fn a_session_refresh_while_open_does_not_move_the_cursor() {
        let mut chat = chat_with_models();
        assert!(chat.open_config_picker("model"));
        chat.handle_key(key(KeyCode::Down));
        let before = chat
            .config_picker
            .as_ref()
            .unwrap()
            .selection()
            .map(|choice| choice.value.clone());
        // The harness re-advertises a reordered list mid-selection.
        chat.set_config_options(&[model_option(
            "auto",
            &[("gpt-5.6-terra", "Terra"), ("auto", "Auto")],
        )]);
        assert_eq!(
            chat.config_picker
                .as_ref()
                .unwrap()
                .selection()
                .map(|choice| choice.value.clone()),
            before
        );
    }

    #[test]
    fn the_selector_draws_over_the_chat_and_marks_the_current_value() {
        let mut chat = chat_with_models();
        assert!(chat.open_config_picker("model"));
        let rows = drawn_transcript(&mut chat, 100, 24);
        let body = rows.join("\n");
        assert!(body.contains("Choose a model"), "modal title is drawn");
        assert!(body.contains("Luna (gpt-5.6-luna)  (current)"));
        assert!(body.contains("Terra (gpt-5.6-terra)"));
    }
}
