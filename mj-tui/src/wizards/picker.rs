use super::*;

#[derive(Debug, Clone, Copy)]
pub(crate) struct PickerNavigation<'a> {
    pub(crate) resources: Option<ResourcePicker<'a>>,
    pub(crate) has_back: bool,
    /// Row the keyboard is on, highlighted while the content has focus.
    pub(crate) selected: usize,
    pub(crate) control: WizardControl,
    pub(crate) next_enabled: bool,
    /// Actions on the listed items, e.g. the project step's Add and Remove,
    /// stacked in a column to the right of the list as the Workspaces
    /// manager stacks its actions.
    pub(crate) side_actions: &'a [(WizardControl, &'static str, bool)],
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
    /// A row of table cells padded into aligned columns.
    pub(crate) fn table(cells: Vec<PickerCell>) -> Self {
        Self {
            cells,
            disabled: false,
            heading: false,
        }
    }

    /// Greys out a row that keeps its place in the list but refuses Enter.
    pub(crate) fn into_disabled(self) -> Self {
        Self {
            disabled: true,
            ..self
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

    /// The row as one line, each cell padded to its column. The last cell
    /// is cut with an ellipsis where the row would pass `max_width`, so a
    /// long note such as a target's unavailability reason shows it was cut
    /// (launch finding C-10).
    pub(crate) fn line(&self, widths: &[usize], max_width: usize) -> Line<'static> {
        let mut spans = Vec::new();
        let mut used = 0usize;
        for (index, cell) in self.cells.iter().enumerate() {
            let last = index + 1 == self.cells.len();
            if last {
                let room = max_width.saturating_sub(used);
                let text = if cell.width() <= room {
                    cell.text.clone()
                } else {
                    truncate_to_cells(&cell.text, room, Truncate::SUMMARY)
                };
                spans.push(Span::styled(text, cell.style));
                break;
            }
            let text = truncate_to_cells(&cell.text, widths[index], Truncate::PLAIN);
            let cell_width = Line::raw(text.as_str()).width();
            let padding = widths[index]
                .saturating_add(COLUMN_GAP)
                .saturating_sub(cell_width);
            spans.push(Span::styled(text, cell.style));
            if padding > 0 {
                spans.push(Span::raw(" ".repeat(padding)));
            }
            used = used.saturating_add(cell_width).saturating_add(padding);
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

/// The target tables of the new-session and resume wizards: the column
/// headings followed by `rows`. The blank first column indents the table as
/// the profile table's marker column does.
pub(crate) fn target_table(rows: Vec<PickerChoice>) -> Vec<PickerChoice> {
    let style = theme::muted().add_modifier(Modifier::BOLD);
    let mut table = Vec::with_capacity(rows.len() + 1);
    table.push(PickerChoice::heading(vec![
        PickerCell::blank(),
        PickerCell::styled("TARGET", style),
        PickerCell::styled("KIND", style),
        PickerCell::styled("CPU", style),
        PickerCell::styled("MEM (GiB)", style),
        PickerCell::styled("STATUS", style),
    ]));
    table.extend(rows);
    table
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
    navigation: PickerNavigation<'_>,
    form: &mut Dialog<WizardControl>,
    surfaces: &mut FrameSurfaces,
) {
    let width_percent = if navigation.control == WizardControl::TargetList {
        if area.width < 100 { 100 } else { 85 }
    } else if area.width < 64 {
        100
    } else {
        68
    };
    let instance_rows = if matches!(navigation.resources, Some(ResourcePicker::Ec2 { .. })) {
        3
    } else {
        0
    };
    let side_width = if navigation.side_actions.is_empty() {
        0
    } else {
        mj_chat::components::ButtonColumn::width(navigation.side_actions)
            + mj_chat::components::ButtonColumn::BODY_GAP
    };
    let estimated = mj_chat::components::dialog_rect(area, width_percent, 1);
    let help = wrap_lines(help, estimated.width.saturating_sub(4 + side_width));
    let popup = centered_modal(
        frame,
        surfaces,
        width_percent,
        (choices.len().max(1) as u16
            + help.len() as u16
            + u16::from(!help.is_empty())
            + instance_rows
            + 5)
        .clamp(6, 28),
        area,
    );
    let content = DialogShell::padded_inner(popup);
    let layout = DialogShell::layout(content, 1);
    let button_area = layout.actions;
    let columns = form.split_actions(layout.body, navigation.side_actions);
    let body = columns.body;
    let list_height = u16::try_from(choices.len())
        .unwrap_or(u16::MAX)
        .max(u16::from(
            choices.is_empty() && navigation.empty_hint.is_some(),
        ))
        .min(
            body.height
                .saturating_sub(help.len() as u16 + u16::from(!help.is_empty()) + instance_rows)
                .max(1)
                .min(body.height),
        );
    let list_area = Rect::new(body.x, body.y, body.width, list_height);
    let mut widths = picker_columns(&choices);
    if navigation.control == WizardControl::TargetList && widths.len() == 6 {
        widths[0] = 0;
        widths[3] = 6;
        widths[4] = 12;
        widths[5] = widths[5].clamp(6, 11);
        let remaining =
            usize::from(list_area.width).saturating_sub(6 + 12 + widths[5] + 5 * COLUMN_GAP);
        widths[1] = widths[1].min(remaining / 2);
        widths[2] = widths[2].min(remaining.saturating_sub(widths[1]));
    }
    let rows = choices
        .iter()
        .map(|choice| choice.line(&widths, usize::from(list_area.width)))
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
    let help_y = list_area
        .bottom()
        .saturating_add(instance_rows)
        .saturating_add(u16::from(!help.is_empty()));
    let help_height = (help.len() as u16).min(body.bottom().saturating_sub(help_y));
    let help_area = Rect::new(body.x, help_y, body.width, help_height);
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
    declare_resource_controls(form, navigation.resources);
    match navigation.resources {
        Some(ResourcePicker::Container(editor)) => {
            let selected_display_row = row_map
                .iter()
                .position(|row| *row == Some(navigation.selected));
            let offset = form.list_offset(navigation.control);
            if let Some(row) = selected_display_row
                .filter(|row| *row >= offset && *row - offset < usize::from(list_area.height))
            {
                let y = list_area.y + (row - offset) as u16;
                let mut x = list_area.x;
                for (column, width) in widths.iter().enumerate() {
                    if let Some((input, id)) = match column {
                        3 => Some((&editor.cpu, WizardControl::ResourceCpu)),
                        4 => Some((&editor.memory, WizardControl::ResourceMemory)),
                        _ => None,
                    } {
                        let field = Rect::new(
                            x,
                            y,
                            (*width as u16).min(list_area.right().saturating_sub(x)),
                            1,
                        );
                        TextField::render_underlined(frame, field, input, form, id);
                    }
                    x = x.saturating_add((*width + COLUMN_GAP) as u16);
                }
            }
        }
        Some(ResourcePicker::Ec2 {
            editor,
            options,
            selected,
            has_selection,
            loading,
        }) => {
            let label_y = list_area.bottom().saturating_add(1);
            let label = Rect::new(body.x, label_y, body.width, 1).intersection(body);
            let field =
                Rect::new(body.x, label_y.saturating_add(1), body.width, 1).intersection(body);
            frame.render_widget(Paragraph::new("Instance type:"), label);
            let selected = editor
                .instances
                .selection(WizardControl::ResourceInstance, selected);
            let lines = options
                .iter()
                .map(|option| Line::raw(resource_allocation_description(Some(option))))
                .collect::<Vec<_>>();
            let value = if loading {
                "Loading instance types…".into()
            } else if options.is_empty() {
                "No instance types available".into()
            } else if has_selection || editor.instances.is_open(WizardControl::ResourceInstance) {
                resource_allocation_description(options.get(selected))
            } else {
                "Choose an instance type".into()
            };
            ComboBox::render(
                frame,
                popup,
                field,
                &ComboBox::display_value(&value),
                &lines,
                selected,
                editor.instances.is_open(WizardControl::ResourceInstance),
                !options.is_empty(),
                " Instance types ",
                PopupSide::Above,
                form,
                WizardControl::ResourceInstance,
            );
        }
        None => {}
    }
    frame.render_widget(Paragraph::new(help), help_area);
    let mut buttons = vec![(WizardControl::Cancel, "Cancel", true)];
    if navigation.has_back {
        buttons.push((WizardControl::Back, "Back", true));
    }
    buttons.push((WizardControl::Next, "Next", navigation.next_enabled));
    Dialog::render_actions(frame, button_area, &buttons, form);
    if !navigation.side_actions.is_empty() {
        Dialog::render_actions_stacked(
            frame,
            columns.actions,
            navigation.side_actions,
            form,
            mj_chat::components::ColumnAlign::Right,
        );
    }
}
