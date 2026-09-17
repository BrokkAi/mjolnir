use super::*;

#[derive(Debug, Clone, Copy)]
pub(crate) struct PickerNavigation {
    pub(crate) has_back: bool,
    /// Row the keyboard is on, highlighted while the content has focus.
    pub(crate) selected: usize,
    pub(crate) control: WizardControl,
    pub(crate) next_enabled: bool,
    /// A secondary action pinned to the right edge of the action row, e.g. the
    /// bundle step's "New bundle…" opener.
    pub(crate) pinned_action: Option<(WizardControl, &'static str, bool)>,
    /// Muted line drawn in place of an empty list, so the step never renders a
    /// blank picker.
    pub(crate) empty_hint: Option<&'static str>,
}

/// One cell of a picker table row.
#[derive(Debug, Clone)]
pub(crate) struct PickerCell {
    text: String,
    style: Style,
}

impl PickerCell {
    pub(crate) fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: Style::default(),
        }
    }

    pub(crate) fn styled(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
        }
    }

    /// Empty cell that keeps the columns after it aligned on rows that need no
    /// entry here.
    pub(crate) fn blank() -> Self {
        Self::text("")
    }

    /// The yellow triangle that marks a row the picker footnote explains.
    pub(crate) fn warning_marker() -> Self {
        Self::styled("⚠", Style::default().fg(theme::palette().warning))
    }

    pub(crate) fn width(&self) -> usize {
        Line::raw(self.text.as_str()).width()
    }
}

/// One picker row. A disabled row stays in the list so row numbers keep
/// matching the underlying map order; it is greyed out and refuses Enter.
///
/// Rows of several cells draw as a table: every cell but the row's last is
/// padded to its column, so the columns line up down the list.
#[derive(Debug, Clone)]
pub(crate) struct PickerChoice {
    cells: Vec<PickerCell>,
    disabled: bool,
    /// Heading rows name the columns and carry no item, so the picker leaves
    /// them out of its row map and item indexes keep naming their own rows.
    heading: bool,
}

impl PickerChoice {
    /// A single cell of plain, unstyled text.
    pub(crate) fn text(text: impl Into<String>) -> Self {
        Self::table(vec![PickerCell::text(text)])
    }

    /// A row of table cells padded into aligned columns.
    pub(crate) fn table(cells: Vec<PickerCell>) -> Self {
        Self {
            cells,
            disabled: false,
            heading: false,
        }
    }

    /// A greyed row that keeps its place in the list but refuses Enter.
    pub(crate) fn disabled(text: impl Into<String>) -> Self {
        Self {
            disabled: true,
            ..Self::text(text)
        }
    }

    /// A non-selectable heading row.
    pub(crate) fn heading(cells: Vec<PickerCell>) -> Self {
        Self {
            heading: true,
            ..Self::table(cells)
        }
    }

    /// Appends a trailing note, e.g. the resume step's lossy-transcript
    /// marker, after the padded columns.
    pub(crate) fn with_note(mut self, note: impl Into<String>) -> Self {
        self.cells.push(PickerCell::text(note));
        self
    }

    pub(crate) fn line(&self, widths: &[usize]) -> Line<'static> {
        let mut spans = Vec::new();
        for (index, cell) in self.cells.iter().enumerate() {
            let last = index + 1 == self.cells.len();
            let padding = if last {
                0
            } else {
                widths[index]
                    .saturating_add(COLUMN_GAP)
                    .saturating_sub(cell.width())
            };
            spans.push(Span::styled(cell.text.clone(), cell.style));
            if padding > 0 {
                spans.push(Span::raw(" ".repeat(padding)));
            }
        }
        Line::from(spans)
    }
}

/// Blank cells between adjacent table columns.
pub(crate) const COLUMN_GAP: usize = 2;

/// Widest cell of every column across the rows, so each cell can be padded to
/// its column.
pub(crate) fn picker_columns(choices: &[PickerChoice]) -> Vec<usize> {
    let mut widths: Vec<usize> = Vec::new();
    for choice in choices {
        for (index, cell) in choice.cells.iter().enumerate() {
            let width = cell.width();
            match widths.get_mut(index) {
                Some(column) => *column = (*column).max(width),
                None => widths.push(width),
            }
        }
    }
    widths
}

/// Heading of the profile tables, in the same columns as their rows.
pub(crate) fn profile_headings() -> PickerChoice {
    let style = theme::muted().add_modifier(Modifier::BOLD);
    PickerChoice::heading(vec![
        PickerCell::blank(),
        PickerCell::styled("PROFILE", style),
        PickerCell::styled("HARNESS", style),
        PickerCell::styled("WEEKLY", style),
        PickerCell::styled("5H", style),
    ])
}

/// Profiles whose harness cannot guard risky actions carry a warning triangle
/// in the table and the footnote `guardian_footnote` draws below it.
pub(crate) fn needs_guardian_warning(harness: HarnessKind) -> bool {
    !harness.supports_guardian_approvals()
}

/// The marker cell of a profile row: the warning triangle for a harness that
/// cannot guard risky actions, and a blank cell that keeps the columns aligned
/// for one that can.
pub(crate) fn guardian_warning_marker(harness: HarnessKind) -> PickerCell {
    if needs_guardian_warning(harness) {
        PickerCell::warning_marker()
    } else {
        PickerCell::blank()
    }
}

/// The row below a profile table that explains its warning triangles.
pub(crate) fn guardian_footnote() -> Line<'static> {
    Line::from(vec![
        Span::styled("⚠  ", Style::default().fg(theme::palette().warning)),
        Span::styled(
            "No guardian approval mode; do not run on a raw, unsandboxed target.",
            theme::muted(),
        ),
    ])
}

/// The profile tables of the new-session and resume wizards: the column
/// headings followed by `rows`. An empty list keeps its hint instead of
/// rendering a bare heading.
pub(crate) fn profile_table(rows: Vec<PickerChoice>) -> Vec<PickerChoice> {
    if rows.is_empty() {
        return rows;
    }
    let mut table = Vec::with_capacity(rows.len() + 1);
    table.push(profile_headings());
    table.extend(rows);
    table
}

/// Muted help row of a picker step.
pub(crate) fn picker_help(text: &str) -> Line<'static> {
    Line::styled(text.to_owned(), theme::muted())
}

// The form and surface registry are distinct rendering owners.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_picker(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    choices: Vec<PickerChoice>,
    help: Vec<Line<'static>>,
    navigation: PickerNavigation,
    form: &mut Dialog<WizardControl>,
    surfaces: &mut FrameSurfaces,
) {
    let width_percent = if area.width < 64 { 100 } else { 68 };
    let popup = centered_modal(
        frame,
        surfaces,
        width_percent,
        (choices.len() as u16 + help.len() as u16 + 6).clamp(9, 19),
        area,
    );
    let content = popup.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 1,
    });
    let list_height = u16::try_from(choices.len())
        .unwrap_or(u16::MAX)
        .max(u16::from(
            choices.is_empty() && navigation.empty_hint.is_some(),
        ))
        .min(content.height.saturating_sub(help.len() as u16 + 2));
    let list_area = Rect::new(content.x, content.y, content.width, list_height);
    let widths = picker_columns(&choices);
    let rows = choices
        .iter()
        .map(|choice| choice.line(&widths))
        .collect::<Vec<_>>();
    let mut row_map = Vec::with_capacity(choices.len());
    let mut items = 0usize;
    for choice in &choices {
        if choice.heading {
            row_map.push(None);
        } else {
            row_map.push(Some(items));
            items += 1;
        }
    }
    let row_enabled = choices
        .iter()
        .map(|choice| !choice.disabled)
        .collect::<Vec<_>>();
    let help_y = list_area.y.saturating_add(list_area.height);
    let help_height = (help.len() as u16).min(content.bottom().saturating_sub(help_y + 1));
    let help_area = Rect::new(content.x, help_y, content.width, help_height);
    let button_area = mj_chat::components::DialogShell::layout(content, 0).actions;
    let title_line = dismissible_modal_title(form, popup, title.trim(), theme::title(true), true);
    frame.render_widget(theme::modal().title(title_line), popup);
    ChoiceList::render_with_rows(
        frame,
        list_area,
        &rows,
        navigation.selected,
        &row_map,
        &row_enabled,
        form,
        navigation.control,
    );
    if choices.is_empty()
        && let Some(hint) = navigation.empty_hint
    {
        frame.render_widget(
            Paragraph::new(Line::styled(
                hint,
                Style::default().fg(theme::palette().muted),
            )),
            list_area,
        );
    }
    frame.render_widget(Paragraph::new(help), help_area);
    let mut buttons = vec![(WizardControl::Cancel, "Cancel", true)];
    if navigation.has_back {
        buttons.push((WizardControl::Back, "Back", true));
    }
    buttons.push((WizardControl::Next, "Next", navigation.next_enabled));
    let row_width = |buttons: &[(WizardControl, &str, bool)]| {
        buttons
            .iter()
            .map(|(_, label, _)| Line::raw(*label).width() + 4)
            .sum::<usize>()
            .saturating_add(buttons.len().saturating_sub(1))
    };
    match navigation.pinned_action {
        // The pinned action keeps its own right-aligned row when it fits next
        // to the navigation buttons, matching the Workspaces action row.
        Some(pinned)
            if row_width(&buttons) + 1 + Line::raw(pinned.1).width() + 4
                <= usize::from(button_area.width) =>
        {
            Dialog::render_actions(frame, button_area, &buttons, form);
            mj_chat::components::ButtonRow::render_aligned(
                frame,
                button_area,
                &[pinned],
                form,
                mj_chat::components::RowAlign::Right,
            );
        }
        pinned => {
            if let Some(pinned) = pinned {
                buttons.insert(buttons.len() - 1, pinned);
            }
            Dialog::render_actions(frame, button_area, &buttons, form);
        }
    }
}
