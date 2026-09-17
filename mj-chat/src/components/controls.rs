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
        Self::render_aligned(
            frame,
            area,
            label,
            enabled,
            form,
            id,
            ratatui::layout::Alignment::Center,
        );
    }

    /// Draws and registers one button with an explicit label alignment.
    pub fn render_aligned<K: Copy + Eq>(
        frame: &mut Frame<'_>,
        area: Rect,
        label: &str,
        enabled: bool,
        form: &mut Form<K>,
        id: K,
        alignment: ratatui::layout::Alignment,
    ) {
        form.register(id, ControlKind::Button, area, enabled);
        let paragraph = Paragraph::new(Line::from(Span::raw(format!("  {label}  "))))
            .style(control_style(form, id, enabled))
            .alignment(alignment);
        frame.render_widget(paragraph, area);
    }
}

/// Which edge of its area a [`ButtonRow`] packs against when it fits.
///
/// A row wider than its area ignores this and stays left-anchored so the
/// focus-following scroll keeps working.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowAlign {
    Left,
    Right,
}

fn button_width(label: &str) -> u16 {
    u16::try_from(label.width() + 4).unwrap_or(u16::MAX)
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
        Self::render_aligned(frame, area, buttons, form, RowAlign::Left);
    }

    /// Draws and registers buttons in order, packed against `align`.
    pub fn render_aligned<K: Copy + Eq>(
        frame: &mut Frame<'_>,
        area: Rect,
        buttons: &[(K, &str, bool)],
        form: &mut Form<K>,
        align: RowAlign,
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
            .map(|(_, label, _)| button_width(label))
            .collect::<Vec<_>>();
        // Only a row that fits can be pushed to the right edge; when it
        // overflows the offset is zero and the scroll below behaves as before.
        let total = widths
            .iter()
            .map(|width| usize::from(*width))
            .sum::<usize>()
            .saturating_add(widths.len().saturating_sub(1));
        let offset = match align {
            RowAlign::Left => 0,
            RowAlign::Right => usize::from(area.width).saturating_sub(total),
        };
        let mut start = offset;
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
        start = offset;
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

    /// Draws the row as inert text, registering no hitboxes.
    ///
    /// A page that stays visible behind a popup draws this mirror so its
    /// controls keep their places, while a click on the overlay covering them
    /// cannot reach a control behind it.
    pub fn render_inert(frame: &mut Frame<'_>, area: Rect, labels: &[&str]) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let mut start = 0usize;
        for label in labels {
            if start >= usize::from(area.width) {
                return;
            }
            let width = usize::from(button_width(label));
            let visible = width.min(usize::from(area.width) - start);
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!("  {label}  "),
                    theme::muted(),
                ))),
                Rect::new(
                    area.x.saturating_add(start as u16),
                    area.y,
                    visible as u16,
                    1,
                ),
            );
            start = start.saturating_add(width).saturating_add(1);
        }
    }
}

/// Which edge of its area a [`ButtonColumn`] packs against when it fits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnAlign {
    Left,
    Right,
}

/// A vertical stack of buttons sharing one width, one button per row.
///
/// The stack is the column counterpart of [`ButtonRow`]: callers set it beside
/// the page body so the dialog's controls sit in a right-hand column. Every row
/// is as wide as the longest label, so the buttons line up on both edges, and a
/// stack taller than its area scrolls to keep the focused button visible.
pub struct ButtonColumn;

/// The page body and the [`ButtonColumn`] set beside it.
///
/// Page content renders into [`Self::body`]; [`Self::actions`] takes the
/// column, which is packed against the edge of the area that was split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColumnSplit {
    pub body: Rect,
    pub actions: Rect,
}

impl ButtonColumn {
    /// The gap between a stacked action column and the body beside it.
    pub const BODY_GAP: u16 = 2;

    /// The width a column of `buttons` needs, including button padding.
    pub fn width<K>(buttons: &[(K, &str, bool)]) -> u16 {
        buttons
            .iter()
            .map(|(_, label, _)| button_width(label))
            .max()
            .unwrap_or(0)
    }

    /// Splits `area` into the page body and the action column beside it.
    ///
    /// The column is content-sized and packed against the right edge of
    /// `area`; the body keeps what is left, minus [`Self::BODY_GAP`].
    pub fn split<K>(area: Rect, buttons: &[(K, &str, bool)]) -> ColumnSplit {
        let width = Self::width(buttons).min(area.width);
        ColumnSplit {
            body: Rect::new(
                area.x,
                area.y,
                area.width
                    .saturating_sub(width.saturating_add(Self::BODY_GAP)),
                area.height,
            ),
            actions: Rect::new(
                area.x.saturating_add(area.width.saturating_sub(width)),
                area.y,
                width,
                area.height,
            ),
        }
    }

    /// Draws and registers buttons from top to bottom, packed to the right.
    pub fn render<K: Copy + Eq>(
        frame: &mut Frame<'_>,
        area: Rect,
        buttons: &[(K, &str, bool)],
        form: &mut Form<K>,
    ) {
        Self::render_aligned(frame, area, buttons, form, ColumnAlign::Right);
    }

    /// Draws and registers buttons in order, packed against `align`.
    pub fn render_aligned<K: Copy + Eq>(
        frame: &mut Frame<'_>,
        area: Rect,
        buttons: &[(K, &str, bool)],
        form: &mut Form<K>,
        align: ColumnAlign,
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
        let width = Self::width(buttons).min(area.width);
        let x = match align {
            ColumnAlign::Left => area.x,
            ColumnAlign::Right => area.x.saturating_add(area.width.saturating_sub(width)),
        };
        let mut scroll = 0usize;
        for (row, (id, _, _)) in buttons.iter().enumerate() {
            if form.is_focused(*id) {
                scroll = row
                    .saturating_add(1)
                    .saturating_sub(usize::from(area.height))
                    .min(row);
            }
        }
        for (row, (id, label, enabled)) in buttons.iter().enumerate() {
            let rect = if row >= scroll && row - scroll < usize::from(area.height) {
                Rect::new(x, area.y.saturating_add((row - scroll) as u16), width, 1)
            } else {
                Rect::default()
            };
            // Every row shares one width, so labels line up on the left the way
            // the rows of a table align.
            Button::render_aligned(
                frame,
                rect,
                label,
                *enabled,
                form,
                *id,
                ratatui::layout::Alignment::Left,
            );
        }
    }

    /// Draws the stack as inert text, registering no hitboxes.
    ///
    /// A page that stays visible behind a popup draws this mirror so its
    /// controls keep their places, while a click on the overlay covering them
    /// cannot reach a control behind it.
    pub fn render_inert(frame: &mut Frame<'_>, area: Rect, labels: &[&str], align: ColumnAlign) {
        if labels.is_empty() || area.width == 0 || area.height == 0 {
            return;
        }
        let width = labels
            .iter()
            .map(|label| button_width(label))
            .max()
            .unwrap_or(0)
            .min(area.width);
        let x = match align {
            ColumnAlign::Left => area.x,
            ColumnAlign::Right => area.x.saturating_add(area.width.saturating_sub(width)),
        };
        for (row, label) in labels.iter().enumerate() {
            if row >= usize::from(area.height) {
                return;
            }
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!("  {label}  "),
                    theme::muted(),
                ))),
                Rect::new(x, area.y.saturating_add(row as u16), width, 1),
            );
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
    pub fn apply(input: &mut TextInput, edit: super::FieldEdit) -> crate::text_input::EditOutcome {
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
        // `len` counts items, not display rows: the host declares the list
        // between frames with its item count, and a length that disagreed
        // would read as a different list and drop the registered geometry.
        let items = mapped.iter().flatten().count();
        form.register_with_rows(
            id,
            ControlKind::ChoiceList {
                len: items,
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
                // The active tab is always highlighted; underline marks keyboard focus.
                if form.is_focused(id) {
                    focus_style().add_modifier(Modifier::UNDERLINED)
                } else {
                    focus_style()
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
mod tests;
