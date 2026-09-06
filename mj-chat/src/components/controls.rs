//! Small Ratatui controls backed by [`Form`](super::Form).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::{ControlKind, Form};
use crate::hel_text_input::TextInput;

const FOCUS_STYLE: Style = Style::new().fg(Color::Black).bg(Color::Cyan);
const NORMAL_STYLE: Style = Style::new().fg(Color::White);
const DISABLED_STYLE: Style = Style::new().fg(Color::DarkGray);

fn control_style<K: Copy + Eq>(form: &Form<K>, id: K, enabled: bool) -> Style {
    if !enabled {
        DISABLED_STYLE
    } else if form.is_focused(id) || form.is_armed(id) {
        FOCUS_STYLE
    } else {
        NORMAL_STYLE
    }
}

/// A push button.
pub struct Button;

impl Button {
    /// Draws and registers one button.
    pub fn render<K: Copy + Eq>(
        frame: &mut Frame<'_>,
        area: Rect,
        label: &str,
        enabled: bool,
        form: &mut Form<K>,
        id: K,
    ) {
        form.register(id, ControlKind::Button, area, enabled);
        let paragraph = Paragraph::new(Line::from(Span::raw(format!("[ {label} ]"))))
            .style(control_style(form, id, enabled))
            .alignment(ratatui::layout::Alignment::Center);
        frame.render_widget(paragraph, area);
    }
}

/// A row of equally aligned buttons with content-sized hitboxes.
pub struct ButtonRow;

impl ButtonRow {
    /// Draws and registers buttons from left to right.
    pub fn render<K: Copy + Eq>(
        frame: &mut Frame<'_>,
        area: Rect,
        buttons: &[(K, &str, bool)],
        form: &mut Form<K>,
    ) {
        if buttons.is_empty() {
            return;
        }
        if area.width == 0 || area.height == 0 {
            for (id, _, enabled) in buttons {
                form.register(*id, ControlKind::Button, Rect::default(), *enabled);
            }
            return;
        }
        let widths = buttons
            .iter()
            .map(|(_, label, _)| u16::try_from(label.width() + 4).unwrap_or(u16::MAX))
            .collect::<Vec<_>>();
        let mut start = 0usize;
        let mut scroll = 0usize;
        for ((id, _, _), width) in buttons.iter().zip(&widths) {
            if form.is_focused(*id) {
                scroll = start
                    .saturating_add(usize::from(*width))
                    .saturating_sub(usize::from(area.width))
                    .min(start);
            }
            start = start.saturating_add(usize::from(*width)).saturating_add(1);
        }
        start = 0;
        for ((id, label, enabled), width) in buttons.iter().zip(widths) {
            let end = start.saturating_add(usize::from(width));
            let visible_start = start.max(scroll);
            let visible_end = end.min(scroll.saturating_add(usize::from(area.width)));
            let rect = if visible_start < visible_end {
                Rect::new(
                    area.x.saturating_add((visible_start - scroll) as u16),
                    area.y,
                    (visible_end - visible_start) as u16,
                    area.height,
                )
            } else {
                Rect::default()
            };
            Button::render(frame, rect, label, *enabled, form, *id);
            start = end.saturating_add(1);
        }
    }
}

/// A readline text field using Hel's existing [`TextInput`] editor.
pub struct TextField;

impl TextField {
    /// Draws a horizontally scrolling field and registers its cursor map.
    pub fn render<K: Copy + Eq>(
        frame: &mut Frame<'_>,
        area: Rect,
        input: &TextInput,
        form: &mut Form<K>,
        id: K,
    ) {
        Self::render_editor(frame, area, input, false, false, true, form, id);
    }

    /// Draws an inline editor owned by an existing compound control.
    #[allow(clippy::too_many_arguments)]
    pub fn render_inline<K: Copy + Eq>(
        frame: &mut Frame<'_>,
        area: Rect,
        input: &TextInput,
        secret: bool,
        focused: bool,
        form: &mut Form<K>,
        id: K,
    ) {
        Self::render_editor(frame, area, input, secret, true, focused, form, id);
    }

    #[allow(clippy::too_many_arguments)]
    fn render_editor<K: Copy + Eq>(
        frame: &mut Frame<'_>,
        area: Rect,
        input: &TextInput,
        secret: bool,
        inline: bool,
        focused: bool,
        form: &mut Form<K>,
        id: K,
    ) {
        let grapheme_width = |text: &str| if secret { 1 } else { text.width() };
        let content = area;
        let width = usize::from(content.width);
        let cursor_width = input.value()[..input.cursor()]
            .graphemes(true)
            .map(grapheme_width)
            .sum::<usize>();
        let mut scroll = cursor_width.saturating_sub(width.saturating_sub(1));
        let graphemes = input.value().graphemes(true);
        let mut start_byte = 0;
        let mut consumed = 0;
        for grapheme in graphemes {
            let grapheme_width = grapheme_width(grapheme);
            if consumed >= scroll {
                break;
            }
            consumed += grapheme_width;
            start_byte += grapheme.len();
        }
        scroll = consumed;

        let mut visible = String::new();
        let mut cursor_map = Vec::new();
        let mut display_width = 0usize;
        let mut byte = start_byte;
        cursor_map.push((content.x, start_byte));
        for grapheme in input.value()[start_byte..].graphemes(true) {
            let grapheme_width = grapheme_width(grapheme);
            if display_width + grapheme_width > width {
                break;
            }
            visible.push_str(if secret { "•" } else { grapheme });
            display_width += grapheme_width;
            byte += grapheme.len();
            cursor_map.push((
                content
                    .x
                    .saturating_add(u16::try_from(display_width).unwrap_or(u16::MAX)),
                byte,
            ));
        }
        if inline {
            form.register_inline_editor(id, area, cursor_map);
        } else {
            form.register_with_cursor_map(id, ControlKind::TextField, area, true, cursor_map);
        }
        let style = control_style(form, id, true);
        frame.render_widget(Paragraph::new(visible).style(style), area);
        if focused && form.is_focused(id) && content.width > 0 && content.height > 0 {
            let cursor = cursor_width.saturating_sub(scroll);
            let x = content.x.saturating_add(
                u16::try_from(cursor.min(width.saturating_sub(1))).unwrap_or(u16::MAX),
            );
            frame.set_cursor_position((x, content.y));
        }
    }

    /// Applies an edit emitted by a form to a text input.
    pub fn apply(input: &mut TextInput, edit: super::FieldEdit) -> rat_event::Outcome {
        super::apply_field_edit(input, edit)
    }
}

/// A checkbox with a text label.
pub struct Checkbox;

impl Checkbox {
    /// Draws and registers a checkbox.
    pub fn render<K: Copy + Eq>(
        frame: &mut Frame<'_>,
        area: Rect,
        label: &str,
        checked: bool,
        enabled: bool,
        form: &mut Form<K>,
        id: K,
    ) {
        form.register(id, ControlKind::Checkbox, area, enabled);
        let mark = if checked { 'x' } else { ' ' };
        frame.render_widget(
            Paragraph::new(format!("[{mark}] {label}")).style(control_style(form, id, enabled)),
            area,
        );
    }
}

/// A vertically navigable list.
pub struct ChoiceList;

impl ChoiceList {
    /// Draws and registers a list. The selected row remains metadata in the form until the
    /// screen applies the emitted [`Interaction`](super::Interaction).
    pub fn render<K: Copy + Eq>(
        frame: &mut Frame<'_>,
        area: Rect,
        rows: &[Line<'_>],
        selected: usize,
        form: &mut Form<K>,
        id: K,
    ) {
        Self::render_with_rows(frame, area, rows, selected, &[], &[], form, id);
    }

    /// Draws wrapped descriptions and options with exact option hitboxes.
    #[allow(clippy::too_many_arguments)]
    pub fn render_wrapped<K: Copy + Eq>(
        frame: &mut Frame<'_>,
        area: Rect,
        rows: &[Line<'_>],
        row_map: &[Option<usize>],
        selected: usize,
        scroll: u16,
        form: &mut Form<K>,
        id: K,
    ) {
        let mut mapped = Vec::new();
        for (index, line) in rows.iter().enumerate() {
            let height = Paragraph::new(line.clone())
                .wrap(ratatui::widgets::Wrap { trim: false })
                .line_count(area.width);
            mapped.extend(std::iter::repeat_n(
                row_map.get(index).copied().flatten(),
                height,
            ));
        }
        form.register_with_rows(
            id,
            ControlKind::ChoiceList {
                len: mapped.len(),
                selected,
            },
            area,
            true,
            mapped,
            vec![],
        );
        form.set_list_offset(id, usize::from(scroll));
        frame.render_widget(
            Paragraph::new(rows.to_vec())
                .wrap(ratatui::widgets::Wrap { trim: false })
                .scroll((scroll, 0)),
            area,
        );
    }

    /// Draws a list with a display-to-option map and per-row enabled state.
    ///
    /// `row_map` may contain `None` for headings or separators. A missing entry in
    /// `row_enabled` is treated as enabled.
    // The two row metadata slices extend the same render contract as other controls.
    #[allow(clippy::too_many_arguments)]
    pub fn render_with_rows<K: Copy + Eq>(
        frame: &mut Frame<'_>,
        area: Rect,
        rows: &[Line<'_>],
        selected: usize,
        row_map: &[Option<usize>],
        row_enabled: &[bool],
        form: &mut Form<K>,
        id: K,
    ) {
        let mapped = if row_map.len() == rows.len() {
            row_map.to_vec()
        } else {
            (0..rows.len()).map(Some).collect()
        };
        let enabled = if row_enabled.len() == rows.len() {
            row_enabled.to_vec()
        } else {
            vec![true; rows.len()]
        };
        form.register_with_rows(
            id,
            ControlKind::ChoiceList {
                len: rows.len(),
                selected,
            },
            area,
            true,
            mapped.clone(),
            enabled.clone(),
        );
        let items = rows
            .iter()
            .enumerate()
            .map(|(index, row)| {
                let item = ListItem::new(row.clone());
                if enabled[index] {
                    item
                } else {
                    item.style(DISABLED_STYLE)
                }
            })
            .collect::<Vec<_>>();
        let mut state = ListState::default();
        let selected_row = mapped
            .iter()
            .enumerate()
            .find(|(_, option)| **option == Some(selected))
            .map(|(index, _)| index);
        state.select(selected_row);
        frame.render_stateful_widget(
            List::new(items).highlight_style(if selected_row.is_some_and(|row| !enabled[row]) {
                DISABLED_STYLE
            } else if form.is_focused(id) {
                FOCUS_STYLE
            } else {
                Style::default().bg(Color::DarkGray)
            }),
            area,
            &mut state,
        );
        form.set_list_offset(id, state.offset());
    }
}

/// A horizontally navigable tab strip.
pub struct TabStrip;

impl TabStrip {
    /// Draws and registers tabs.
    pub fn render<K: Copy + Eq>(
        frame: &mut Frame<'_>,
        area: Rect,
        tabs: &[&str],
        selected: usize,
        form: &mut Form<K>,
        id: K,
    ) {
        Self::render_enabled(frame, area, tabs, selected, true, form, id);
    }

    /// Draws a tab strip with explicit enabled state.
    #[allow(clippy::too_many_arguments)]
    pub fn render_enabled<K: Copy + Eq>(
        frame: &mut Frame<'_>,
        area: Rect,
        tabs: &[&str],
        selected: usize,
        enabled: bool,
        form: &mut Form<K>,
        id: K,
    ) {
        form.declare_with_enabled(
            id,
            ControlKind::Tabs {
                len: tabs.len(),
                selected,
            },
            enabled,
        );
        let mut starts = Vec::with_capacity(tabs.len());
        let mut x = 0usize;
        for tab in tabs {
            starts.push(x);
            x = x.saturating_add(tab.width()).saturating_add(1);
        }
        let selected = selected.min(tabs.len().saturating_sub(1));
        let selected_start = starts.get(selected).copied().unwrap_or(0);
        let selected_end =
            selected_start.saturating_add(tabs.get(selected).map_or(0, |tab| tab.width()));
        let available = usize::from(area.width);
        let mut scroll = selected_end.saturating_sub(available);
        scroll = scroll.min(selected_start);

        let mut regions = Vec::with_capacity(tabs.len());
        frame.render_widget(Paragraph::new(""), area);
        for (index, tab) in tabs.iter().enumerate() {
            let tab_start = starts[index];
            let tab_end = tab_start.saturating_add(tab.width());
            let visible_start = tab_start.max(scroll);
            let visible_end = tab_end.min(scroll.saturating_add(available));
            if visible_start >= visible_end {
                continue;
            }
            let style = if !enabled {
                DISABLED_STYLE
            } else if index == selected {
                if form.is_focused(id) {
                    FOCUS_STYLE.add_modifier(Modifier::BOLD)
                } else {
                    NORMAL_STYLE.add_modifier(Modifier::UNDERLINED)
                }
            } else {
                NORMAL_STYLE
            };
            let skip = visible_start - tab_start;
            let end = visible_end - tab_start;
            let mut rendered = String::new();
            let mut width = 0usize;
            for grapheme in tab.graphemes(true) {
                let next = width.saturating_add(grapheme.width());
                let clipped = next.min(end).saturating_sub(width.max(skip));
                if clipped > 0 {
                    if width >= skip && next <= end {
                        rendered.push_str(grapheme);
                    } else {
                        rendered.extend(std::iter::repeat_n(' ', clipped));
                    }
                }
                width = next;
            }
            let x = area
                .x
                .saturating_add(u16::try_from(visible_start - scroll).unwrap_or(u16::MAX));
            let width = u16::try_from(visible_end - visible_start).unwrap_or(u16::MAX);
            frame.render_widget(
                Paragraph::new(rendered).style(style),
                Rect::new(x, area.y, width, area.height),
            );
            regions.push((x, x.saturating_add(width), index));
        }
        form.register_with_regions(
            id,
            ControlKind::Tabs {
                len: tabs.len(),
                selected,
            },
            area,
            enabled,
            regions,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::{FieldEdit, Interaction};
    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::{Terminal, backend::TestBackend};

    fn click(form: &mut Form<u8>, x: u16, y: u16) -> Option<Interaction<u8>> {
        let event = |kind| {
            Event::Mouse(MouseEvent {
                kind,
                column: x,
                row: y,
                modifiers: KeyModifiers::NONE,
            })
        };
        form.handle(&event(MouseEventKind::Down(MouseButton::Left)));
        form.handle(&event(MouseEventKind::Up(MouseButton::Left)))
            .action
    }

    #[test]
    fn wrapped_choice_rows_keep_navigation_and_inline_editor_hitboxes_distinct() {
        let mut form = Form::new();
        form.declare(
            1,
            ControlKind::ChoiceList {
                len: 2,
                selected: 0,
            },
        );
        form.end_frame(1);
        let mut terminal = Terminal::new(TestBackend::new(12, 7)).unwrap();
        let input = TextInput::from_value("a界z");
        terminal
            .draw(|frame| {
                form.begin_frame();
                ChoiceList::render_wrapped(
                    frame,
                    Rect::new(0, 0, 12, 7),
                    &[
                        Line::from("Heading"),
                        Line::from("First option has long text"),
                        Line::from("Second"),
                        Line::from(""),
                    ],
                    &[None, Some(0), Some(1), None],
                    0,
                    0,
                    &mut form,
                    1,
                );
                TextField::render_inline(
                    frame,
                    Rect::new(2, 5, 8, 1),
                    &input,
                    false,
                    true,
                    &mut form,
                    1,
                );
                form.end_frame(1);
            })
            .unwrap();
        assert_eq!(
            form.handle(&Event::Key(KeyEvent::new(
                KeyCode::Down,
                KeyModifiers::NONE
            )))
            .action,
            Some(Interaction::Select(1, 1))
        );
        assert_eq!(click(&mut form, 1, 4), Some(Interaction::Select(1, 1)));
        assert_eq!(click(&mut form, 1, 0), None);
        let edit = form
            .handle(&Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 5,
                row: 5,
                modifiers: KeyModifiers::NONE,
            }))
            .action;
        assert_eq!(edit, Some(Interaction::Edit(1, FieldEdit::Cursor(4))));
    }

    #[test]
    fn drawn_unicode_field_click_uses_display_cells_and_grapheme_boundaries() {
        let mut form = Form::new();
        let mut input = TextInput::from_value("a界e\u{301}z");
        input.set_cursor(0);
        form.declare(1, ControlKind::TextField);
        form.end_frame(1);
        let mut terminal = Terminal::new(TestBackend::new(10, 2)).unwrap();
        terminal
            .draw(|frame| {
                form.begin_frame();
                TextField::render(frame, Rect::new(1, 0, 8, 1), &input, &mut form, 1);
                form.end_frame(1);
            })
            .unwrap();
        assert_eq!(terminal.backend().buffer()[(2, 0)].symbol(), "界");
        let result = form.handle(&Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 4,
            row: 0,
            modifiers: KeyModifiers::NONE,
        }));
        assert_eq!(
            result.action,
            Some(Interaction::Edit(1, FieldEdit::Cursor(4)))
        );
        TextField::apply(&mut input, FieldEdit::Cursor(4));
        assert_eq!(input.cursor(), "a界".len());
    }

    #[test]
    fn scrolling_tabs_hit_the_drawn_label_and_ignore_separator() {
        let mut form = Form::new();
        form.declare(
            1,
            ControlKind::Tabs {
                len: 3,
                selected: 2,
            },
        );
        form.end_frame(1);
        let mut terminal = Terminal::new(TestBackend::new(10, 1)).unwrap();
        terminal
            .draw(|frame| {
                form.begin_frame();
                TabStrip::render(
                    frame,
                    Rect::new(0, 0, 10, 1),
                    &["Long first", "界", "Last"],
                    2,
                    &mut form,
                    1,
                );
                form.end_frame(1);
            })
            .unwrap();
        assert_eq!(terminal.backend().buffer()[(3, 0)].symbol(), "界");
        assert_eq!(terminal.backend().buffer()[(6, 0)].symbol(), "L");
        assert_eq!(click(&mut form, 5, 0), None);
        assert_eq!(click(&mut form, 3, 0), Some(Interaction::Select(1, 1)));
        assert_eq!(click(&mut form, 6, 0), Some(Interaction::Select(1, 2)));
    }

    #[test]
    fn clipped_list_click_selects_the_visible_row_after_scrolling() {
        let mut form = Form::new();
        form.declare(
            1,
            ControlKind::ChoiceList {
                len: 20,
                selected: 19,
            },
        );
        form.end_frame(1);
        let lines = (0..20)
            .map(|index| Line::raw(format!("row {index}")))
            .collect::<Vec<_>>();
        let mut terminal = Terminal::new(TestBackend::new(15, 3)).unwrap();
        terminal
            .draw(|frame| {
                form.begin_frame();
                ChoiceList::render(frame, frame.area(), &lines, 19, &mut form, 1);
                form.end_frame(1);
            })
            .unwrap();
        assert_eq!(form.list_offset(1), 17);
        assert_eq!(click(&mut form, 1, 0), Some(Interaction::Select(1, 17)));
    }
}
