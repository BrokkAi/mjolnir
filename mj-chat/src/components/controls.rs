//! Small Ratatui controls backed by [`Form`](super::Form).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::text_layout::multiline_rows;
use super::{AutocompletePopup, ControlKind, Form, Interaction, PopupSide};
use crate::text_input::TextInput;
use crate::theme;

fn focus_style() -> Style {
    Style::new()
        .fg(theme::palette().background)
        .bg(theme::palette().accent)
        .add_modifier(Modifier::BOLD)
}
fn normal_style() -> Style {
    Style::new()
        .fg(theme::palette().text)
        .bg(theme::palette().surface_raised)
}
fn disabled_style() -> Style {
    Style::new()
        .fg(theme::palette().muted)
        .bg(theme::palette().surface_raised)
}

fn control_style<K: Copy + Eq>(form: &Form<K>, id: K, enabled: bool) -> Style {
    if !enabled {
        disabled_style()
    } else if form.is_focused(id) || form.is_armed(id) {
        focus_style()
    } else if form.is_default_action(id) {
        normal_style()
            .fg(theme::palette().accent)
            .add_modifier(Modifier::BOLD)
    } else {
        normal_style()
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
        let paragraph = Paragraph::new(Line::from(Span::raw(format!("  {label}  "))))
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
        for (id, _, enabled) in buttons {
            form.declare_with_enabled(*id, ControlKind::Button, *enabled);
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

    /// Draws a vertically scrolling field and registers its two-dimensional
    /// cursor map. The map is built from the same grapheme-aware layout used
    /// by the composer, so mouse clicks follow wrapped Unicode text.
    pub fn render_multiline<K: Copy + Eq>(
        frame: &mut Frame<'_>,
        area: Rect,
        input: &TextInput,
        enabled: bool,
        focused: bool,
        form: &mut Form<K>,
        id: K,
    ) {
        let (rows, (cursor_column, cursor_row)) =
            multiline_rows(input.value(), input.cursor(), usize::from(area.width));
        let scroll = cursor_row
            .saturating_add(1)
            .saturating_sub(usize::from(area.height));
        let mut cursor_map = Vec::new();
        for (row, visual) in rows
            .iter()
            .enumerate()
            .skip(scroll)
            .take(usize::from(area.height))
        {
            let screen_row = area.y.saturating_add((row - scroll) as u16);
            let mut column = 0usize;
            cursor_map.push((area.x, screen_row, visual.start));
            for grapheme in &visual.graphemes {
                column = column.saturating_add(grapheme.width);
                cursor_map.push((
                    area.x.saturating_add(column as u16),
                    screen_row,
                    grapheme.end,
                ));
            }
        }
        if cursor_row >= rows.len()
            && cursor_row >= scroll
            && cursor_row < scroll.saturating_add(usize::from(area.height))
        {
            cursor_map.push((
                area.x,
                area.y.saturating_add((cursor_row - scroll) as u16),
                input.value().len(),
            ));
        }
        form.register_with_multiline_cursor_map(
            id,
            ControlKind::TextField,
            area,
            enabled,
            cursor_map,
        );
        frame.render_widget(
            Paragraph::new(input.value())
                .wrap(Wrap { trim: false })
                .scroll((scroll.min(u16::MAX as usize) as u16, 0)),
            area,
        );
        if enabled && focused && form.is_focused(id) && area.width > 0 && area.height > 0 {
            let cursor_row = cursor_row.saturating_sub(scroll);
            if cursor_row < usize::from(area.height) {
                frame.set_cursor_position((
                    area.x.saturating_add(
                        cursor_column.min(usize::from(area.width).saturating_sub(1)) as u16,
                    ),
                    area.y.saturating_add(cursor_row as u16),
                ));
            }
        }
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
        let style = if focused && form.is_focused(id) {
            normal_style().add_modifier(Modifier::UNDERLINED)
        } else {
            normal_style()
        };
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
        let mark = if checked { '✓' } else { ' ' };
        frame.render_widget(
            Paragraph::new(format!("[{mark}] {label}")).style(control_style(form, id, enabled)),
            area,
        );
    }
}

/// State for one screen-local combobox interaction.
///
/// The state deliberately keeps only one pending cursor. A screen can render
/// several comboboxes, but opening a second one replaces the first so popup
/// interactions cannot leak into another field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComboBoxState<K: Copy + Eq> {
    open: Option<K>,
    pending: Option<usize>,
}

impl<K: Copy + Eq> Default for ComboBoxState<K> {
    fn default() -> Self {
        Self {
            open: None,
            pending: None,
        }
    }
}

impl<K: Copy + Eq> ComboBoxState<K> {
    /// Returns the currently expanded field, if any.
    #[must_use]
    pub fn open_id(&self) -> Option<K> {
        self.open
    }

    /// Returns whether `id` currently owns the popup.
    #[must_use]
    pub fn is_open(&self, id: K) -> bool {
        self.open == Some(id)
    }

    /// Opens `id`, taking a snapshot of its committed selection.
    pub fn open(&mut self, id: K, selected: usize) {
        self.open = Some(id);
        self.pending = Some(selected);
    }

    /// Returns the selection to render for a field.
    #[must_use]
    pub fn selection(&self, id: K, committed: usize) -> usize {
        self.is_open(id)
            .then_some(self.pending)
            .flatten()
            .unwrap_or(committed)
    }

    /// Records an uncommitted popup cursor movement.
    pub fn preview(&mut self, id: K, selected: usize) -> bool {
        if self.is_open(id) {
            let changed = self.pending != Some(selected);
            self.pending = Some(selected);
            changed
        } else {
            false
        }
    }

    /// Accepts and closes the active popup, returning its pending selection.
    pub fn accept(&mut self, id: K) -> Option<usize> {
        if self.is_open(id) {
            let selected = self.pending.take();
            self.open = None;
            selected
        } else {
            None
        }
    }

    /// Closes the active popup without changing its committed value.
    pub fn dismiss(&mut self, id: K) -> bool {
        if self.is_open(id) {
            self.open = None;
            self.pending = None;
            true
        } else {
            false
        }
    }

    /// Routes interactions produced by a combobox form control.
    ///
    /// Preview selections are consumed locally. Accepted selections are
    /// returned to the screen as [`Interaction::ComboBoxCommit`]. Interactions
    /// for other controls pass through unchanged, so callers can feed a form's
    /// action stream through this helper without special casing every screen.
    pub fn route(&mut self, interaction: Option<Interaction<K>>) -> Option<Interaction<K>> {
        let interaction = interaction?;
        let Some(open) = self.open else {
            return Some(interaction);
        };
        match interaction {
            Interaction::Select(id, selected) if id == open => {
                self.preview(id, selected);
                None
            }
            Interaction::ComboBoxCommit(id, selected) if id == open => {
                self.open = None;
                self.pending = None;
                Some(Interaction::ComboBoxCommit(id, selected))
            }
            Interaction::ComboBoxDismiss(id) if id == open => {
                self.dismiss(id);
                Some(Interaction::ComboBoxDismiss(id))
            }
            other => {
                self.open = None;
                self.pending = None;
                Some(other)
            }
        }
    }
}

/// A compact scalar field with an anchored list of choices.
pub struct ComboBox;

impl ComboBox {
    /// The compact affordance appended to a collapsed choice value.
    pub const GLYPH: &'static str = "▾";

    /// Returns a collapsed field value with the dropdown affordance.
    #[must_use]
    pub fn display_value(value: &str) -> String {
        format!("{value} {}", Self::GLYPH)
    }

    /// Draws a combobox and, when expanded, its anchored popup.
    #[allow(clippy::too_many_arguments)]
    pub fn render<K: Copy + Eq>(
        frame: &mut Frame<'_>,
        bounds: Rect,
        area: Rect,
        value: &str,
        options: &[Line<'_>],
        selected: usize,
        expanded: bool,
        enabled: bool,
        title: &str,
        preferred_side: PopupSide,
        form: &mut Form<K>,
        id: K,
    ) -> Option<(Rect, Rect)> {
        let selected = selected.min(options.len().saturating_sub(1));
        let kind = ControlKind::ComboBox {
            len: options.len(),
            selected,
            expanded,
        };
        form.register_combobox(id, kind, area, enabled, Rect::default(), Vec::new());
        let display = clipped_display(value, usize::from(area.width));
        frame.render_widget(
            Paragraph::new(display).style(control_style(form, id, enabled)),
            area,
        );

        let popup = if expanded {
            let option_width = options.iter().map(Line::width).max().unwrap_or(0);
            let width = option_width
                .saturating_add(4)
                .max(title.width().saturating_add(2));
            AutocompletePopup::render(
                frame,
                bounds,
                area,
                u16::try_from(width).unwrap_or(u16::MAX),
                options.len(),
                title,
                preferred_side,
            )
        } else {
            None
        };

        if let Some((outer, inner)) = popup {
            let items = options
                .iter()
                .map(|row| ListItem::new(row.clone()))
                .collect::<Vec<_>>();
            let mut state = ListState::default();
            state.select((!options.is_empty()).then_some(selected));
            frame.render_stateful_widget(
                List::new(items).highlight_style(if form.is_focused(id) {
                    theme::selection(true)
                } else {
                    theme::selection(false)
                }),
                inner,
                &mut state,
            );
            let offset = state.offset();
            let mut popup_row_map = vec![None; usize::from(outer.height)];
            for (row, option) in popup_row_map
                .iter_mut()
                .enumerate()
                .skip(1)
                .take(usize::from(inner.height))
            {
                *option = offset
                    .checked_add(row.saturating_sub(1))
                    .filter(|index| *index < options.len());
            }
            form.register_combobox(id, kind, area, enabled, outer, popup_row_map);
            Some((outer, inner))
        } else {
            None
        }
    }
}

fn clipped_display(value: &str, width: usize) -> String {
    let suffix = format!(" {}", ComboBox::GLYPH);
    if width == 0 {
        return String::new();
    }
    if width < suffix.width() {
        return ComboBox::GLYPH.to_owned();
    }
    if width == suffix.width() {
        return suffix
            .graphemes(true)
            .scan(0usize, |used, grapheme| {
                let next = (*used).saturating_add(grapheme.width());
                (next <= width).then(|| {
                    *used = next;
                    grapheme
                })
            })
            .collect();
    }
    let available = width.saturating_sub(suffix.width());
    let mut result = String::new();
    let mut used = 0usize;
    for grapheme in value.graphemes(true) {
        let next = used.saturating_add(grapheme.width());
        if next > available {
            break;
        }
        result.push_str(grapheme);
        used = next;
    }
    result.push_str(&suffix);
    result
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
        form.set_list_contents(id, rows.iter().map(ToString::to_string).collect());
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
        form.set_list_contents(id, rows.iter().map(ToString::to_string).collect());
        let items = rows
            .iter()
            .enumerate()
            .map(|(index, row)| {
                let item = ListItem::new(row.clone());
                if enabled[index] {
                    item
                } else {
                    item.style(disabled_style())
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
                disabled_style()
            } else if form.is_focused(id) {
                theme::selection(true)
            } else {
                theme::selection(false)
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
                disabled_style()
            } else if index == selected {
                if form.is_focused(id) {
                    focus_style().add_modifier(Modifier::BOLD)
                } else {
                    normal_style().add_modifier(Modifier::UNDERLINED)
                }
            } else {
                normal_style()
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
    fn tabbing_to_a_clipped_button_scrolls_it_into_view() {
        let mut form = Form::new();
        form.declare(1, ControlKind::Button);
        form.declare(2, ControlKind::Button);
        form.end_frame(1);
        let mut terminal = Terminal::new(TestBackend::new(10, 1)).unwrap();
        for selected in [1, 2] {
            terminal
                .draw(|frame| {
                    form.begin_frame();
                    ButtonRow::render(
                        frame,
                        frame.area(),
                        &[(1, "First", true), (2, "Last", true)],
                        &mut form,
                    );
                    form.end_frame(1);
                })
                .unwrap();
            assert_eq!(form.focused(), Some(selected));
            assert_eq!(
                click(&mut form, 5, 0),
                Some(Interaction::Activate(selected))
            );
            form.handle(&Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
        }
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("Last"));
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
    fn multiline_field_click_tracks_wrapped_newlines_and_unicode() {
        let mut form = Form::new();
        let mut input = TextInput::multiline();
        input.set_value("abcdefghi\nab界e\u{301}z\nfinal");
        input.set_cursor(input.value().len());
        form.declare(1, ControlKind::TextField);
        form.end_frame(1);
        let area = Rect::new(2, 1, 8, 2);
        let mut terminal = Terminal::new(TestBackend::new(14, 5)).unwrap();
        terminal
            .draw(|frame| {
                form.begin_frame();
                TextField::render_multiline(frame, area, &input, true, true, &mut form, 1);
                form.end_frame(1);
            })
            .unwrap();

        // The caret is on the final line, so the field scrolls to show the
        // second logical line and the final hard-newline-delimited line.
        let click = |form: &mut Form<i32>, column, row| {
            form.handle(&Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column,
                row,
                modifiers: KeyModifiers::NONE,
            }))
            .action
        };
        let first = click(&mut form, area.x + 4, area.y);
        assert_eq!(
            first,
            Some(Interaction::Edit(
                1,
                FieldEdit::Cursor("abcdefghi\n".len() + "ab界".len())
            ))
        );
        let Some(Interaction::Edit(1, first_edit)) = first else {
            panic!("wrapped row click should edit the field");
        };
        TextField::apply(&mut input, first_edit);
        TextField::apply(
            &mut input,
            FieldEdit::Key(KeyEvent::new(KeyCode::Char('X'), KeyModifiers::NONE)),
        );

        input.set_cursor(input.value().len());
        terminal
            .draw(|frame| {
                form.begin_frame();
                TextField::render_multiline(frame, area, &input, true, true, &mut form, 1);
                form.end_frame(1);
            })
            .unwrap();

        let second = click(&mut form, area.x, area.y + 1);
        assert_eq!(
            second,
            Some(Interaction::Edit(
                1,
                FieldEdit::Cursor("abcdefghi\nab界Xe\u{301}z\n".len()),
            ))
        );
        let Some(Interaction::Edit(1, second_edit)) = second else {
            panic!("hard-newline row click should edit the field");
        };
        TextField::apply(&mut input, second_edit);
        TextField::apply(
            &mut input,
            FieldEdit::Key(KeyEvent::new(KeyCode::Char('Y'), KeyModifiers::NONE)),
        );
        assert_eq!(input.value(), "abcdefghi\nab界Xe\u{301}z\nYfinal");
    }

    #[test]
    fn multiline_field_click_scales_with_a_large_pasted_prompt() {
        let mut form = Form::new();
        let mut input = TextInput::multiline();
        input.set_value("x".repeat(70_000));
        input.set_cursor(input.value().len());
        form.declare(1, ControlKind::TextField);
        form.end_frame(1);
        let area = Rect::new(1, 1, 20, 3);
        let mut terminal = Terminal::new(TestBackend::new(24, 6)).unwrap();
        terminal
            .draw(|frame| {
                form.begin_frame();
                TextField::render_multiline(frame, area, &input, true, true, &mut form, 1);
                form.end_frame(1);
            })
            .unwrap();

        let result = form.handle(&Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: area.right() - 1,
            row: area.bottom() - 1,
            modifiers: KeyModifiers::NONE,
        }));
        assert_eq!(
            result.action,
            Some(Interaction::Edit(1, FieldEdit::Cursor(70_000)))
        );
        let Some(Interaction::Edit(1, edit)) = result.action else {
            panic!("large prompt click should edit the field");
        };
        TextField::apply(&mut input, edit);
        TextField::apply(
            &mut input,
            FieldEdit::Key(KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE)),
        );
        assert_eq!(input.value().len(), 70_001);
        assert_eq!(&input.value()[69_995..], "xxxxx!");
    }

    #[test]
    fn multiline_field_click_after_a_zero_width_tab_keeps_the_byte_offset() {
        let mut form = Form::new();
        let mut input = TextInput::multiline();
        input.set_value("a\t界");
        input.set_cursor(input.value().len());
        form.declare(1, ControlKind::TextField);
        form.end_frame(1);
        let area = Rect::new(1, 0, 8, 1);
        let mut terminal = Terminal::new(TestBackend::new(10, 2)).unwrap();
        terminal
            .draw(|frame| {
                form.begin_frame();
                TextField::render_multiline(frame, area, &input, true, true, &mut form, 1);
                form.end_frame(1);
            })
            .unwrap();

        // Ratatui skips the tab control character, so the visible wide glyph
        // starts in the same cell as the tab's end boundary.
        let result = form.handle(&Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: area.x + 1,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        }));
        assert_eq!(
            result.action,
            Some(Interaction::Edit(1, FieldEdit::Cursor("a\t".len())))
        );
        let Some(Interaction::Edit(1, edit)) = result.action else {
            panic!("tab click should edit the field");
        };
        TextField::apply(&mut input, edit);
        TextField::apply(
            &mut input,
            FieldEdit::Key(KeyEvent::new(KeyCode::Char('X'), KeyModifiers::NONE)),
        );
        assert_eq!(input.value(), "a\tX界");
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

    #[test]
    fn combobox_state_consumes_preview_and_returns_commit_or_dismissal() {
        let mut state = ComboBoxState::default();
        state.open(1, 0);
        assert_eq!(state.selection(1, 2), 0);
        assert_eq!(state.route(Some(Interaction::Select(1, 2))), None);
        assert_eq!(state.selection(1, 0), 2);
        assert_eq!(
            state.route(Some(Interaction::ComboBoxCommit(1, 1))),
            Some(Interaction::ComboBoxCommit(1, 1))
        );
        assert_eq!(state.open_id(), None);

        state.open(1, 1);
        assert_eq!(
            state.route(Some(Interaction::ComboBoxDismiss(1))),
            Some(Interaction::ComboBoxDismiss(1))
        );
        assert!(!state.is_open(1));
        assert_eq!(
            state.route(Some(Interaction::Activate(2))),
            Some(Interaction::Activate(2))
        );
    }

    #[test]
    fn combobox_collapsed_value_keeps_a_visible_dropdown_glyph() {
        assert_eq!(clipped_display("long value", 1), ComboBox::GLYPH);
        let mut terminal = Terminal::new(TestBackend::new(12, 1)).expect("terminal");
        terminal
            .draw(|frame| {
                let mut form = Form::new();
                form.begin_frame();
                ComboBox::render(
                    frame,
                    frame.area(),
                    Rect::new(0, 0, 6, 1),
                    "long value",
                    &[Line::raw("one"), Line::raw("two")],
                    0,
                    false,
                    true,
                    " choices ",
                    PopupSide::Below,
                    &mut form,
                    1,
                );
                form.end_frame(1);
            })
            .expect("draw combobox");
        let rendered = (0..6)
            .map(|x| terminal.backend().buffer()[(x, 0)].symbol())
            .collect::<String>();
        assert!(rendered.contains(ComboBox::GLYPH), "{rendered}");
    }

    #[test]
    fn rendered_combobox_popup_rows_commit_with_a_mouse_click() {
        let mut terminal = Terminal::new(TestBackend::new(20, 10)).expect("terminal");
        let mut form = Form::new();
        terminal
            .draw(|frame| {
                form.begin_frame();
                ComboBox::render(
                    frame,
                    frame.area(),
                    Rect::new(0, 1, 10, 1),
                    "one",
                    &[Line::raw("one"), Line::raw("two")],
                    0,
                    true,
                    true,
                    " choices ",
                    PopupSide::Below,
                    &mut form,
                    1,
                );
                form.end_frame(1);
            })
            .expect("draw popup");
        assert_eq!(
            click(&mut form, 1, 4),
            Some(Interaction::ComboBoxCommit(1, 1))
        );
    }
}
