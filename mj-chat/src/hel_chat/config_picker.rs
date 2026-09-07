//! The `/model` and `/effort` selector: a modal over the chat listing every
//! value the harness advertises, filtered as the user types.

use crossterm::event::{Event, KeyCode, KeyEvent};
use rat_event::ConsumedEvent;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use hel::hel_acp::SessionConfigChoice;
use hel::hel_worker::WorkerPhase;

use super::autocomplete::{config_value_row, matching_indices};
use super::{ChatAction, ChatState};
use crate::components::{ButtonRow, ChoiceList, ControlKind, Form, Interaction, TextField};
use crate::hel_text_input::TextInput;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigControl {
    Filter,
    Values,
    Apply,
    Cancel,
}

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
    filter: TextInput,
    form: Form<ConfigControl>,
    /// Cursor position within `filtered`, mirrored in the list's typed metadata.
    selected: usize,
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
        let mut form = Form::new();
        form.declare(ConfigControl::Filter, ControlKind::TextField);
        form.declare(
            ConfigControl::Values,
            ControlKind::ChoiceList {
                len: choices.len(),
                selected,
            },
        );
        form.declare(ConfigControl::Apply, ControlKind::Button);
        form.declare(ConfigControl::Cancel, ControlKind::Button);
        form.end_frame(ConfigControl::Filter);
        Self {
            key,
            choices,
            current,
            filter: TextInput::new(),
            form,
            selected,
            filtered,
        }
    }

    fn selection(&self) -> Option<&SessionConfigChoice> {
        self.filtered
            .get(self.selected)
            .and_then(|&index| self.choices.get(index))
    }

    /// Recomputes the matches, keeping the cursor on the same choice when it
    /// survives the new filter.
    fn refilter(&mut self) {
        let kept = self.filtered.get(self.selected).copied();
        self.filtered = if self.filter.is_empty() {
            (0..self.choices.len()).collect()
        } else {
            matching_indices(&self.choices, self.filter.value(), |choice| {
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

    pub(super) fn config_picker_handles_mouse(&self, column: u16, row: u16) -> bool {
        self.config_picker.as_ref().is_some_and(|picker| {
            picker.form.captures_pointer() || picker.form.contains(column, row)
        })
    }

    pub(super) fn cancel_config_picker_pointer(&mut self) {
        if let Some(picker) = self.config_picker.as_mut() {
            picker.form.cancel_pointer();
        }
    }

    pub(super) fn reset_config_picker_geometry(&mut self) {
        if let Some(picker) = self.config_picker.as_mut() {
            picker.form.reset_geometry();
        }
    }

    pub(super) fn handle_config_picker_mouse(
        &mut self,
        mouse: crossterm::event::MouseEvent,
    ) -> (bool, ChatAction) {
        let event = Event::Mouse(mouse);
        let result = match self.config_picker.as_mut() {
            Some(picker) => picker.form.handle(&event),
            None => return (false, ChatAction::None),
        };
        match result.action {
            Some(Interaction::Edit(ConfigControl::Filter, edit)) => {
                if let Some(picker) = self.config_picker.as_mut() {
                    TextField::apply(&mut picker.filter, edit);
                    picker.refilter();
                }
                (true, ChatAction::None)
            }
            Some(Interaction::Select(ConfigControl::Values, selected)) => {
                if let Some(picker) = self.config_picker.as_mut() {
                    picker.selected = selected;
                }
                (true, ChatAction::None)
            }
            Some(Interaction::Activate(ConfigControl::Values | ConfigControl::Apply)) => {
                (true, self.apply_config_picker_selection())
            }
            Some(Interaction::Activate(ConfigControl::Cancel) | Interaction::Cancel) => {
                self.config_picker = None;
                (true, ChatAction::None)
            }
            _ => (result.outcome.is_consumed(), ChatAction::None),
        }
    }

    pub(super) fn handle_config_picker_event(&mut self, key: KeyEvent) -> ChatAction {
        let Some(picker) = self.config_picker.as_mut() else {
            return ChatAction::None;
        };
        if picker.form.is_focused(ConfigControl::Filter)
            && key.modifiers.is_empty()
            && matches!(key.code, KeyCode::Up | KeyCode::Down)
        {
            picker.form.focus(ConfigControl::Values);
        }
        let event = Event::Key(key);
        let result = picker.form.handle(&event);
        if let Some(action) = result.action {
            match action {
                Interaction::Edit(ConfigControl::Filter, edit) => {
                    TextField::apply(&mut picker.filter, edit);
                    picker.refilter();
                }
                Interaction::Select(ConfigControl::Values, selected) => {
                    picker.selected = selected;
                }
                Interaction::Activate(ConfigControl::Values | ConfigControl::Apply) => {
                    return self.apply_config_picker_selection();
                }
                Interaction::Activate(ConfigControl::Filter) => {
                    return self.apply_config_picker_selection();
                }
                Interaction::Activate(ConfigControl::Cancel) | Interaction::Cancel => {
                    self.config_picker = None;
                }
                _ => {}
            }
            return ChatAction::None;
        }
        if result.outcome.is_consumed() {
            return ChatAction::None;
        }
        ChatAction::None
    }

    fn apply_config_picker_selection(&mut self) -> ChatAction {
        let Some(picker) = self.config_picker.as_ref() else {
            return ChatAction::None;
        };
        let Some(choice) = picker.selection() else {
            return ChatAction::None;
        };
        let key = picker.key;
        let value = choice.value.clone();
        self.config_picker = None;
        if matches!(self.phase, WorkerPhase::Closing | WorkerPhase::Closed) {
            self.set_notice("The worker is closing; this configuration change was not sent");
            return ChatAction::None;
        }
        ChatAction::SetConfig {
            key: key.to_owned(),
            value,
        }
    }
}

/// Draws the selector over the chat and reports the rows it owns.
pub(super) fn render_config_picker(
    frame: &mut Frame,
    area: Rect,
    chat: &mut ChatState,
) -> Option<Rect> {
    let picker = chat.config_picker.as_mut()?;
    let visible = picker.filtered.len().clamp(1, 8);
    let rect = crate::hel_modal::centered_modal_rect_fixed(frame, 72, visible as u16 + 7, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" Choose a {} ", picker.key))
        .border_style(Style::default().fg(Color::LightMagenta));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(inner);
    let label = Paragraph::new(Line::from(Span::styled(
        "filter:",
        Style::default().fg(Color::DarkGray),
    )));
    frame.render_widget(label, chunks[0]);
    let filter_area = chunks[1];
    picker.form.begin_frame();
    TextField::render(
        frame,
        filter_area,
        &picker.filter,
        &mut picker.form,
        ConfigControl::Filter,
    );

    let rows = picker
        .filtered
        .iter()
        .filter_map(|&index| picker.choices.get(index))
        .map(|choice| {
            let mut row = config_value_row(choice).unwrap_or_else(|| choice.value.clone());
            if picker.current.as_deref() == Some(choice.value.as_str()) {
                row.push_str("  (current)");
            }
            Line::from(row)
        })
        .collect::<Vec<_>>();
    let row_area = chunks[2];
    ChoiceList::render(
        frame,
        row_area,
        &rows,
        picker.selected,
        &mut picker.form,
        ConfigControl::Values,
    );
    ButtonRow::render(
        frame,
        chunks[3],
        &[
            (ConfigControl::Apply, "Apply", !picker.filtered.is_empty()),
            (ConfigControl::Cancel, "Cancel", true),
        ],
        &mut picker.form,
    );
    frame.render_widget(
        Paragraph::new("↑/↓ choose · type to filter · Tab controls · Enter apply · Esc cancel")
            .style(Style::default().fg(Color::DarkGray)),
        chunks[4],
    );
    picker.form.end_frame(ConfigControl::Filter);
    Some(inner)
}

#[cfg(test)]
mod tests {
    use crate::hel_chat::ChatAction;
    use crate::hel_chat::ChatState;
    use crate::hel_chat::test_support::{drawn_transcript, key, snapshot};
    use crate::hel_modal::MODAL_SCREEN_MARGIN;
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
    fn effort_selector_stops_at_the_end_and_escape_closes_without_a_change() {
        let mut chat = chat_with_models();
        chat.set_input("/effort".into());
        assert_eq!(chat.handle_key(key(KeyCode::Enter)), ChatAction::None);
        let picker = chat.config_picker.as_ref().unwrap();
        assert_eq!(
            picker.selection().map(|choice| choice.value.as_str()),
            Some("high")
        );
        // Down from "high" reaches "max" and stays there at the end.
        chat.handle_key(key(KeyCode::Down));
        chat.handle_key(key(KeyCode::Down));
        assert_eq!(
            chat.config_picker
                .as_ref()
                .unwrap()
                .selection()
                .map(|choice| choice.value.as_str()),
            Some("max")
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

    #[test]
    fn the_selector_keeps_blank_cells_beside_it_on_a_terminal_narrower_than_its_box() {
        const WIDTH: u16 = 70;
        let mut chat = chat_with_models();
        assert!(chat.open_config_picker("model"));
        // The box wants 72 cells, so without a margin rule it would clamp to the
        // full width and sit flush against the chat behind it.
        let rows = drawn_transcript(&mut chat, WIDTH, 24);
        let title = rows
            .iter()
            .find(|row| row.contains("Choose a model"))
            .expect("the selector draws a titled border");

        // Columns, not byte offsets: the border glyphs are multi-byte.
        let column_of = |corner: char| {
            title
                .chars()
                .position(|character| character == corner)
                .unwrap_or_else(|| panic!("no {corner} in {title:?}"))
        };
        let left = column_of('┌');
        let right = column_of('┐');
        assert!(
            left >= usize::from(MODAL_SCREEN_MARGIN),
            "selector starts at column {left} in {title:?}"
        );
        assert!(
            usize::from(WIDTH) - (right + 1) >= usize::from(MODAL_SCREEN_MARGIN),
            "selector ends at column {right} of {WIDTH} in {title:?}"
        );
    }
}
