//! Compact, anchored `/model` and `/effort` dropdowns with type-to-filter.

use crate::theme;
use crossterm::event::{Event, KeyCode, KeyEvent, MouseButton, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use mj_core::acp::SessionConfigChoice;
use mj_core::relay::WorkerPhase;

use super::autocomplete::{config_choice_name, matching_indices};
use super::{ChatAction, ChatState};
use crate::components::{
    AutocompletePopup, ChoiceList, ControlKind, Form, Interaction, ListActivation, PopupSide,
    TextField,
};
use crate::text_input::TextInput;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigControl {
    Filter,
    Values,
}

/// Choices are snapshotted so a refresh cannot reorder the list under the
/// cursor. Form owns the cursor and pointer gesture; live values are checked
/// before applying the choice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ConfigPicker {
    key: &'static str,
    choices: Vec<SessionConfigChoice>,
    current: Option<String>,
    filter: TextInput,
    form: Form<ConfigControl>,
    filtered: Vec<usize>,
    area: Rect,
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
        form.set_list_activation(ConfigControl::Values, ListActivation::SingleClick);
        form.end_frame(ConfigControl::Values);
        Self {
            key,
            choices,
            current,
            filter: TextInput::new(),
            form,
            filtered,
            area: Rect::default(),
        }
    }

    fn selected(&self) -> usize {
        self.form.selected(ConfigControl::Values).unwrap_or(0)
    }

    fn selection(&self) -> Option<&SessionConfigChoice> {
        self.filtered
            .get(self.selected())
            .and_then(|&index| self.choices.get(index))
    }

    fn refilter(&mut self) {
        let kept = self.filtered.get(self.selected()).copied();
        self.filtered = if self.filter.is_empty() {
            (0..self.choices.len()).collect()
        } else {
            matching_indices(&self.choices, self.filter.value(), |choice| {
                (&choice.value, Some(choice.name.as_str()))
            })
        };
        let selected = kept
            .and_then(|index| self.filtered.iter().position(|&i| i == index))
            .unwrap_or(0);
        // Filtering invalidates old row hitboxes even if the match count is unchanged.
        self.form.reset_geometry();
        self.form.declare(
            ConfigControl::Values,
            ControlKind::ChoiceList {
                len: self.filtered.len(),
                selected,
            },
        );
    }
}

impl ChatState {
    pub(super) fn advertised_config_values(&self, key: &str) -> &[SessionConfigChoice] {
        match key {
            "model" => &self.model_values,
            "effort" => &self.effort_values,
            _ => &[],
        }
    }

    pub(super) fn open_config_picker(&mut self, key: &'static str) -> bool {
        let current = match key {
            "model" => self.current_model().map(str::to_owned),
            "effort" => self.current_effort().map(str::to_owned),
            _ => return false,
        };
        let choices = self.advertised_config_values(key).to_vec();
        if choices.is_empty() {
            return false;
        }
        self.config_picker = Some(ConfigPicker::new(key, choices, current));
        true
    }

    pub(super) fn config_picker_active(&self) -> bool {
        self.config_picker.is_some()
    }

    pub(super) fn prompt_config_chip_at(&self, column: u16, row: u16) -> Option<&'static str> {
        self.config_chip_areas
            .iter()
            .find(|(_, area)| area.contains((column, row).into()))
            .map(|(key, _)| *key)
    }

    pub(super) fn open_prompt_config_picker(&mut self, key: &'static str) {
        if matches!(self.phase, WorkerPhase::Closing | WorkerPhase::Closed) {
            self.set_notice("The worker is closing; this configuration change was not sent");
            return;
        }
        if !self.open_config_picker(key) {
            self.set_notice(format!(
                "The agent does not advertise {key} values; usage: /{key} <value>"
            ));
        }
    }

    pub(super) fn cancel_config_picker_pointer(&mut self) {
        if let Some(picker) = self.config_picker.as_mut() {
            picker.form.cancel_pointer();
        }
    }

    pub(super) fn reset_config_picker_geometry(&mut self) {
        if let Some(picker) = self.config_picker.as_mut() {
            picker.form.reset_geometry();
            picker.area = Rect::default();
        }
    }

    pub(super) fn handle_config_picker_mouse(
        &mut self,
        mouse: crossterm::event::MouseEvent,
    ) -> (bool, ChatAction) {
        let Some(picker) = self.config_picker.as_mut() else {
            return (false, ChatAction::None);
        };
        if mouse.kind == MouseEventKind::Down(MouseButton::Left)
            && !picker.area.contains((mouse.column, mouse.row).into())
        {
            self.config_picker = None;
            return (true, ChatAction::None);
        }
        let result = picker.form.handle(&Event::Mouse(mouse));
        let action = if matches!(
            result.action,
            Some(Interaction::Activate(ConfigControl::Values))
        ) {
            self.apply_config_picker_selection()
        } else {
            ChatAction::None
        };
        (true, action)
    }

    pub(super) fn handle_config_picker_event(&mut self, key: KeyEvent) -> ChatAction {
        let Some(picker) = self.config_picker.as_mut() else {
            return ChatAction::None;
        };
        // Editing remains available after arrow navigation or a scroll gesture.
        let editing = matches!(
            key.code,
            KeyCode::Char(_)
                | KeyCode::Backspace
                | KeyCode::Delete
                | KeyCode::Left
                | KeyCode::Right
        );
        picker.form.focus(if editing {
            ConfigControl::Filter
        } else {
            ConfigControl::Values
        });
        let action = picker.form.handle(&Event::Key(key)).action;
        match action {
            Some(Interaction::Edit(ConfigControl::Filter, edit)) => {
                TextField::apply(&mut picker.filter, edit);
                picker.refilter();
            }
            Some(Interaction::Activate(ConfigControl::Values)) => {
                return self.apply_config_picker_selection();
            }
            Some(Interaction::Cancel) => {
                self.config_picker = None;
            }
            _ => {}
        }
        if let Some(picker) = self.config_picker.as_mut() {
            picker.form.focus(ConfigControl::Values);
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
        if !self
            .advertised_config_values(key)
            .iter()
            .any(|choice| choice.value == value)
        {
            self.set_notice(format!("The agent no longer advertises {key} value {value}; this configuration change was not sent"));
            return ChatAction::None;
        }
        ChatAction::SetConfig {
            key: key.to_owned(),
            value,
        }
    }
}

pub(super) fn render_config_picker(
    frame: &mut Frame,
    area: Rect,
    chat: &mut ChatState,
) -> Option<Rect> {
    let picker = chat.config_picker.as_mut()?;
    let anchor = chat
        .config_chip_areas
        .iter()
        .find(|(key, _)| *key == picker.key)
        .map(|(_, area)| *area)
        .or(chat.voice_button_area)
        .unwrap_or(Rect::new(area.x, area.bottom().saturating_sub(1), 1, 1));
    let rows = picker
        .filtered
        .iter()
        .map(|&index| {
            let choice = &picker.choices[index];
            let marker = if picker.current.as_deref() == Some(choice.value.as_str()) {
                theme::glyphs().check
            } else {
                " "
            };
            Line::from(format!("{marker} {}", config_choice_name(choice)))
        })
        .collect::<Vec<_>>();
    let title = if picker.filter.is_empty() {
        format!(" {} ", picker.key)
    } else {
        format!(" {}: {} ", picker.key, picker.filter.value())
    };
    let width = rows
        .iter()
        .map(Line::width)
        .max()
        .unwrap_or("No matches".len())
        .max(title.width())
        .saturating_add(2);
    let selected = picker.selected();
    picker.area = Rect::default();
    let Some((outer, inner)) = AutocompletePopup::render(
        frame,
        area,
        anchor,
        u16::try_from(width).unwrap_or(u16::MAX),
        rows.len().max(1),
        &title,
        PopupSide::Above,
    ) else {
        picker.form.reset_geometry();
        return None;
    };
    picker.form.begin_frame();
    picker.area = outer;
    picker
        .form
        .declare(ConfigControl::Filter, ControlKind::TextField);
    ChoiceList::render(
        frame,
        inner,
        &rows,
        selected,
        &mut picker.form,
        ConfigControl::Values,
    );
    if rows.is_empty() {
        frame.render_widget(Paragraph::new("No matches").style(theme::muted()), inner);
    }
    picker.form.end_frame(ConfigControl::Values);
    Some(outer)
}

#[cfg(test)]
mod tests {
    use crate::chat::ChatAction;
    use crate::chat::ChatState;
    use crate::chat::test_support::{drawn_transcript, key, snapshot};
    use agent_client_protocol::schema::v1::{
        SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelectOption,
        SessionConfigSelectOptions,
    };
    use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

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
    fn clicking_a_prompt_title_chip_opens_that_keys_selector() {
        let mut chat = chat_with_models();
        let rows = drawn_transcript(&mut chat, 100, 24);
        let chips = chat.config_chip_areas.clone();
        let (column, row) = chips
            .iter()
            .find(|(key, _)| *key == "model")
            .map(|(_, area)| (area.x, area.y))
            .expect("the model chip is registered");
        assert!(
            chips.iter().any(|(key, _)| *key == "effort"),
            "the effort chip is registered too: {chips:?}"
        );
        // The hitbox covers the model text drawn in the prompt's top border.
        let title = &rows[usize::from(row)];
        let covered = title
            .chars()
            .skip(usize::from(column))
            .take(usize::from(
                chips
                    .iter()
                    .find(|(key, _)| *key == "model")
                    .map(|(_, area)| area.width)
                    .unwrap_or(0),
            ))
            .collect::<String>();
        assert!(
            covered.trim_start().starts_with("Luna ▾"),
            "chip covers {covered:?} in {title:?}"
        );

        let press = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        assert!(chat.component_handles_mouse(press));
        assert_eq!(chat.handle_mouse(press), ChatAction::None);
        assert!(chat.config_picker_active());
    }

    #[test]
    fn outside_click_closes_the_selector_just_like_escape() {
        let mut clicked = chat_with_models();
        assert!(clicked.open_config_picker("model"));
        drawn_transcript(&mut clicked, 100, 24);
        let column = 99;
        let row = 23;
        let press = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(clicked.handle_mouse(press), ChatAction::None);
        assert_eq!(
            clicked.handle_mouse(MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                ..press
            }),
            ChatAction::None
        );
        assert!(!clicked.config_picker_active());

        let mut escaped = chat_with_models();
        assert!(escaped.open_config_picker("model"));
        assert_eq!(escaped.handle_key(key(KeyCode::Esc)), ChatAction::None);
        assert!(!escaped.config_picker_active());
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
    fn dropdown_is_anchored_and_shows_names_with_a_current_marker() {
        let mut chat = chat_with_models();
        assert!(chat.open_config_picker("model"));
        let rows = drawn_transcript(&mut chat, 100, 24);
        let body = rows.join("\n");
        assert!(body.contains("✓ Luna"), "{body}");
        assert!(body.contains("  Terra"), "{body}");
        assert!(!body.contains("gpt-5.6-luna"));
        assert!(!body.contains("Apply"));
        let picker = chat.config_picker.as_ref().unwrap();
        let anchor = chat
            .config_chip_areas
            .iter()
            .find(|(key, _)| *key == "model")
            .unwrap()
            .1;
        assert_eq!(picker.area.x, anchor.x);
        assert_eq!(picker.area.bottom(), anchor.y);
        assert!(picker.area.width < 30);
        assert_eq!(picker.selection().unwrap().value, "gpt-5.6-luna");
    }

    #[test]
    fn clicking_a_row_applies_that_row_once_on_release() {
        let mut chat = chat_with_models();
        chat.open_config_picker("model");
        let rows = drawn_transcript(&mut chat, 100, 24);
        let row = rows.iter().position(|row| row.contains("Terra")).unwrap() as u16;
        let column = chat.config_picker.as_ref().unwrap().area.x + 2;
        let press = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(chat.handle_mouse(press), ChatAction::None);
        // A redraw between press and release must preserve the gesture and cursor.
        drawn_transcript(&mut chat, 100, 24);
        let release = MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            ..press
        };
        assert_eq!(
            chat.handle_mouse(release),
            ChatAction::SetConfig {
                key: "model".into(),
                value: "gpt-5.6-terra".into()
            }
        );
        assert!(!chat.config_picker_active());
        assert_eq!(chat.handle_mouse(release), ChatAction::None);
    }

    #[test]
    fn filtering_after_navigation_handles_empty_results_and_backspace() {
        let mut chat = chat_with_models();
        chat.open_config_picker("model");
        chat.handle_key(key(KeyCode::Down));
        for c in "autoz".chars() {
            chat.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(chat.handle_key(key(KeyCode::Enter)), ChatAction::None);
        assert!(
            drawn_transcript(&mut chat, 100, 24)
                .join("\n")
                .contains("No matches")
        );
        chat.handle_key(key(KeyCode::Backspace));
        assert_eq!(
            chat.handle_key(key(KeyCode::Enter)),
            ChatAction::SetConfig {
                key: "model".into(),
                value: "auto".into()
            }
        );
    }

    #[test]
    fn stale_choices_and_shutdown_do_not_submit() {
        let mut chat = chat_with_models();
        chat.open_config_picker("model");
        chat.set_config_options(&[model_option("auto", &[("auto", "Auto")])]);
        assert_eq!(chat.handle_key(key(KeyCode::Enter)), ChatAction::None);
        assert!(chat.notice().unwrap().contains("no longer advertises"));
        chat.open_config_picker("model");
        chat.phase = mj_core::relay::WorkerPhase::Closing;
        assert_eq!(chat.handle_key(key(KeyCode::Enter)), ChatAction::None);
        assert!(chat.notice().unwrap().contains("closing"));
    }

    #[test]
    fn long_lists_scroll_and_stay_inside_narrow_panes() {
        let mut chat = chat_with_models();
        let names = (0..24).map(|i| format!("model-{i:02}")).collect::<Vec<_>>();
        let choices = names
            .iter()
            .map(|name| (name.as_str(), name.as_str()))
            .collect::<Vec<_>>();
        chat.set_config_options(&[model_option("model-00", &choices)]);
        chat.open_config_picker("model");
        for _ in 0..23 {
            chat.handle_key(key(KeyCode::Down));
        }
        for width in [100, 30, 12] {
            let rows = drawn_transcript(&mut chat, width, 24);
            let picker = chat.config_picker.as_ref().unwrap();
            assert!(picker.area.right() <= width);
            assert!(picker.area.bottom() <= 24);
            assert!(picker.area.height <= 10);
            assert!(rows.join("\n").contains("model-23"));
            assert_eq!(picker.selection().unwrap().value, "model-23");
        }
    }
    #[test]
    fn outside_click_on_subagents_only_dismisses_the_dropdown() {
        let mut chat = chat_with_models();
        chat.set_subagent_count(1);
        drawn_transcript(&mut chat, 100, 24);
        chat.open_config_picker("model");
        drawn_transcript(&mut chat, 100, 24);
        let area = chat.subagent_control_area.unwrap();
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: area.x,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        };
        assert!(chat.component_handles_mouse(click));
        assert_eq!(chat.handle_mouse(click), ChatAction::None);
        assert!(!chat.config_picker_active());
    }

    #[test]
    fn dropdown_owns_enter_even_if_a_navigation_control_had_focus() {
        let mut chat = chat_with_models();
        chat.set_subagent_count(1);
        chat.focus_subagent_control();
        chat.open_config_picker("model");
        assert_eq!(
            chat.handle_key(key(KeyCode::Enter)),
            ChatAction::SetConfig {
                key: "model".into(),
                value: "gpt-5.6-luna".into()
            }
        );
    }

    #[test]
    fn unavailable_and_closing_configuration_has_no_dropdown_hitbox() {
        let mut chat = chat_with_models();
        chat.model_values.clear();
        drawn_transcript(&mut chat, 100, 24);
        assert!(
            !chat
                .config_chip_areas
                .iter()
                .any(|(key, _)| *key == "model")
        );
        assert!(
            chat.config_chip_areas
                .iter()
                .any(|(key, _)| *key == "effort")
        );
        chat.phase = mj_core::relay::WorkerPhase::Closed;
        drawn_transcript(&mut chat, 100, 24);
        assert!(chat.config_chip_areas.is_empty());
    }

    #[test]
    fn ascii_dropdown_uses_names_and_ascii_markers_in_every_theme() {
        for palette in crate::theme::UiTheme::ALL {
            crate::theme::with_theme(palette, || {
                crate::theme::with_symbols(crate::theme::SymbolSet::Ascii, || {
                    let mut chat = chat_with_models();
                    chat.set_subagent_count(1);
                    chat.open_config_picker("model");
                    let body = drawn_transcript(&mut chat, 100, 24).join("\n");
                    assert!(body.contains("Luna v"), "{body}");
                    assert!(body.contains("x Luna"), "{body}");
                    assert!(body.contains("Subagents - 0 working >"), "{body}");
                    assert!(!body.contains('▾'));
                    assert!(!body.contains('›'));
                })
            });
        }
    }
}
