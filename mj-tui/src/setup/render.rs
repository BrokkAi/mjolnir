use super::*;

pub(super) fn setting_row(name: &str, value: &str, width: u16) -> Line<'static> {
    const MIN_LABEL: usize = 8;
    let width = usize::from(width).max(SETTING_GUTTER.len() + 4);
    // Everything between the gutter and the trailing space.
    let inner = width - SETTING_GUTTER.len() - 1;
    let label_floor = name.chars().count().min(MIN_LABEL);
    let value = truncate(value, inner.saturating_sub(label_floor + 1));
    let value_width = value.chars().count();
    let name = truncate(name, inner.saturating_sub(value_width + 1));
    let gap = inner
        .saturating_sub(name.chars().count() + value_width)
        .max(1);
    Line::from(vec![
        Span::raw(SETTING_GUTTER),
        Span::raw(name),
        Span::raw(" ".repeat(gap)),
        Span::styled(value, theme::muted()),
        Span::raw(" "),
    ])
}

pub(super) const SETTING_GUTTER: &str = "  ";

pub(super) fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    text.chars()
        .take(width.saturating_sub(1))
        .chain(['…'])
        .collect()
}

pub(super) fn preferred_size(draft: &Value) -> SetupSize {
    fn walk(path: &mut Vec<String>, draft: &Value, max_width: &mut usize, max_height: &mut u16) {
        let Some(value) = draft.pointer(&pointer(path)) else {
            return;
        };
        let keys = visible_keys(path, value);
        *max_width = (*max_width).max(Line::raw(breadcrumb(path, draft)).width());
        *max_height = (*max_height).max(
            u16::try_from(page_plan(path, &keys).len())
                .unwrap_or(u16::MAX)
                .saturating_add(10),
        );
        for key in keys {
            let parent = path.clone();
            path.push(key.clone());
            let Some(child) = draft.pointer(&pointer(path)) else {
                path.pop();
                continue;
            };
            let name = row_label(&parent, value, &key, Some(child));
            let mut summary = row_summary(&parent, &key, child, draft, None);
            if !child.is_object()
                && !child.is_array()
                && !child.is_boolean()
                && schema::choices(&storage_path(path), draft).is_empty()
            {
                summary = summary.chars().take(24).collect();
            }
            // The gutter, the name, the gap the value is pushed away by, and
            // the trailing column the row ends with.
            let line = format!("{}{name}    {summary} ", SETTING_GUTTER);
            *max_width = (*max_width).max(Line::raw(line).width());
            // `interface` resolves to the draft root, which holds most of the
            // settings its page gathers; the prefix has its own nested mapping.
            if (child.is_object() || child.is_array()) && path.as_slice() != ["review"] {
                walk(path, draft, max_width, max_height);
            }
            path.pop();
        }
    }

    let mut max_width = 0usize;
    let mut max_height = 20;
    walk(&mut Vec::new(), draft, &mut max_width, &mut max_height);
    // The page's own actions stack in a column at its right edge, so the
    // dialog is as wide as the widest page body plus that column. Back and the
    // commit sit in the footer row instead and take no width here.
    let column = [
        "Add",
        "Create",
        "Remove",
        "Detect profiles",
        "Detect runtimes",
        "Use default",
        "Apply",
    ]
    .iter()
    .map(|label| Line::raw(*label).width() + 4)
    .max()
    .unwrap_or(0)
    .saturating_add(usize::from(mj_chat::components::ButtonColumn::BODY_GAP));
    max_width = max_width.saturating_add(column);
    SetupSize {
        // The body, its gap to the column, the column, and the inner margin.
        width: u16::try_from(max_width.saturating_add(4))
            .unwrap_or(u16::MAX)
            .clamp(64, 96),
        // One extra row over the page body for the footer action row.
        height: max_height.saturating_add(1).clamp(21, 33),
    }
}
pub(crate) fn render_setup(
    frame: &mut Frame,
    area: Rect,
    dashboard: &DashboardState,
    dialog: &SetupDialog,
    surfaces: &mut FrameSurfaces,
) {
    if let Some(review) = &dialog.review_editor {
        let popup = centered_modal_fixed(
            frame,
            surfaces,
            dialog.preferred_width,
            dialog.preferred_height,
            area,
        );
        render_review_settings(frame, popup, review, dialog.saving);
        return;
    }
    use SetupControl::*;
    let popup = centered_modal_fixed(
        frame,
        surfaces,
        dialog.preferred_width,
        dialog.preferred_height,
        area,
    );
    let inner = popup.inner(ratatui::layout::Margin {
        horizontal: 2,
        vertical: 1,
    });
    if inner.height < 7 {
        let mut form = dialog.form.borrow_mut();
        form.begin_frame();
        let title = dismissible_modal_title(
            &mut form,
            popup,
            "Settings",
            theme::title(true),
            !dialog.saving,
        );
        frame.render_widget(theme::modal().title(title), popup);
        let layout = mj_chat::components::DialogShell::layout(inner, 0);
        // Too short for the page body: keep only the way out and the commit.
        let footer = dialog.footer_actions();
        let page_actions = dialog
            .page_actions()
            .into_iter()
            .filter(|(id, _, _)| matches!(id, Apply))
            .collect::<Vec<_>>();
        let ColumnSplit {
            body: message,
            actions: column,
        } = split_page(&form, layout.body, &page_actions);
        frame.render_widget(
            Paragraph::new("Enlarge the terminal to edit these settings.")
                .wrap(Wrap { trim: false }),
            message,
        );
        let initial = footer
            .first()
            .or(page_actions.first())
            .map_or(Save, |(id, _, _)| *id);
        Dialog::render_actions_stacked(frame, column, &page_actions, &mut form, ColumnAlign::Right);
        Dialog::render_actions(frame, layout.actions, &footer, &mut form);
        form.end_frame(initial);
        return;
    }
    if dialog.search.is_some() {
        render_search(frame, popup, inner, dialog);
        return;
    }
    let text_editor = dialog
        .editor
        .as_ref()
        .is_some_and(|editor| editor.choices.is_empty());
    let choice_editor = dialog
        .editor
        .as_ref()
        .is_some_and(|editor| !editor.choices.is_empty());
    let path = if text_editor {
        &dialog.editor.as_ref().expect("text editor").path
    } else {
        &dialog.path
    };
    let nested = !path.is_empty();
    if nested {
        frame.render_widget(
            Paragraph::new(breadcrumb(path, &dialog.draft)).style(theme::title(true)),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );
    }
    let help_y = inner.y + u16::from(nested);
    // The hint wraps in full; two rows is the least it keeps, so pages with
    // short hints do not move.
    let help = Paragraph::new(schema::page_help(path, &dialog.draft))
        .wrap(Wrap { trim: false })
        .style(theme::muted());
    let help_rows = u16::try_from(help.line_count(inner.width))
        .unwrap_or(u16::MAX)
        .clamp(2, (inner.height / 3).max(2));
    frame.render_widget(help, Rect::new(inner.x, help_y, inner.width, help_rows));
    let body_y = help_y + help_rows + 1;
    // Back and the commit share the dialog's bottom row; the page's own
    // actions stack in a column beside the body.
    let footer_row = mj_chat::components::DialogShell::layout(inner, 0).actions;
    // A notice takes three rows, or as many as its wrapped text needs, as
    // long as the page keeps a few rows of its own above it.
    let notice_rows = dialog.notice.as_ref().map_or(3, |notice| {
        let needed = Paragraph::new(notice.as_str())
            .wrap(Wrap { trim: false })
            .line_count(inner.width);
        let room = footer_row.y.saturating_sub(body_y).saturating_sub(3);
        u16::try_from(needed).unwrap_or(u16::MAX).min(room).max(3)
    });
    let band = Rect::new(
        inner.x,
        body_y,
        inner.width,
        inner
            .height
            .saturating_sub(2 + help_rows + notice_rows + u16::from(nested))
            .max(1),
    );
    // The three rows a notice would occupy belong to the page while no notice
    // is showing in them, never to the footer's row.
    let band = if dialog.notice.is_some() {
        band
    } else {
        Rect::new(
            band.x,
            band.y,
            band.width,
            footer_row.y.saturating_sub(band.y).max(1),
        )
    };
    let mut form = dialog.form.borrow_mut();
    form.begin_frame();
    let footer = dialog.footer_actions();
    let page_actions = dialog.page_actions();
    // The body gives up exactly the width the column needs, and the whole
    // width on a page that has no actions of its own.
    let ColumnSplit {
        body,
        actions: column,
    } = split_page(&form, band, &page_actions);
    let notice = dialog.notice.as_ref();
    // A column taller than the body may also use the rows the notice would
    // occupy, but only while no notice is showing in them, and never the
    // footer's row.
    let column = if notice.is_some() {
        column
    } else {
        Rect::new(
            column.x,
            column.y,
            column.width,
            footer_row.y.saturating_sub(column.y),
        )
    };
    let title = dismissible_modal_title(
        &mut form,
        popup,
        "Settings",
        theme::title(true),
        !dialog.saving && !choice_editor,
    );
    frame.render_widget(
        theme::modal()
            .title(title)
            .title_bottom(mj_chat::components::DialogShell::hints(false)),
        popup,
    );
    let mut initial;
    let mut background_offset = form.list_offset(List);
    // Where a rejected value is reported while a field is being edited: with
    // the field, not on the dialog's bottom rows far below it.
    let mut editor_notice = None;
    if text_editor {
        let editor = dialog.editor.as_ref().expect("text editor");
        let label = if editor.adding {
            "Name for the new entry".to_owned()
        } else {
            schema::field_label(&editor.path)
        };
        frame.render_widget(
            Paragraph::new(label),
            Rect::new(body.x, body.y, body.width, 1),
        );
        let area = Rect::new(body.x, body.y + 1, body.width, 1);
        match &editor.input {
            EditorInput::Text(input) => TextField::render(frame, area, input, &mut form, Field),
            EditorInput::Path(input) => PathField::render(frame, area, input, &mut form, Field),
        }
        let mut next_row = body.y + 2;
        // The archive window's estimate follows the number as it is typed, so
        // it sits right under the input rather than on the notice line.
        if editor
            .path
            .last()
            .is_some_and(|key| key == "archive_after_days")
            && body.height > 2
            && let Some(estimate) = dialog.archive_space_estimate()
        {
            frame.render_widget(
                Paragraph::new(estimate).style(theme::muted()),
                Rect::new(body.x, next_row, body.width, 1),
            );
            next_row += 1;
        }
        // The field's own column is narrower than the dialog, so the message
        // keeps every row left below it to wrap into.
        if next_row < body.bottom() {
            editor_notice = Some(Rect::new(
                body.x,
                next_row,
                body.width,
                body.bottom() - next_row,
            ));
        }
        initial = Field;
    } else {
        let keys = dialog.keys();
        let mut rows = Vec::new();
        let mut row_map = Vec::new();
        // Settings owned by host mbx are shown but edited through mbx.
        let mut row_enabled = Vec::new();
        let mbx_unavailable = dialog.mbx_unavailable().map(str::to_owned);
        for row in page_plan(&dialog.path, &keys) {
            let index = match row {
                PageRow::Gap => {
                    rows.push(Line::raw(""));
                    row_map.push(None);
                    row_enabled.push(true);
                    continue;
                }
                PageRow::Heading(heading) => {
                    rows.push(Line::styled(
                        format!("{SETTING_GUTTER}{heading}"),
                        theme::muted().add_modifier(Modifier::BOLD),
                    ));
                    row_map.push(None);
                    row_enabled.push(true);
                    continue;
                }
                PageRow::Setting(index) => index,
            };
            let key = &keys[index];
            let mut child_path = dialog.path.clone();
            child_path.push(key.clone());
            let value = dialog.draft.pointer(&pointer(&child_path));
            let name = row_label(&dialog.path, dialog.current(), key, value);
            let summary = match value {
                Some(value) => row_summary(
                    &dialog.path,
                    key,
                    value,
                    &dialog.draft,
                    dialog
                        .build_cache_automatic_label(key)
                        .or_else(|| dialog.archive_space_automatic_label(key))
                        .or_else(|| {
                            (key == "build_cache")
                                .then(|| dialog.build_cache_row_unavailable())
                                .flatten()
                        }),
                ),
                None => String::new(),
            };
            // An open dropdown draws the value itself over this row.
            let summary = if dialog.build_cache_user_managed()
                && is_build_cache_field(&child_path, "directory")
            {
                dialog.build_cache_automatic_label(key).unwrap_or(summary)
            } else {
                summary
            };
            let missing_runtime = (dialog.path == ["machines"])
                .then(|| machine_without_runtime_note(dashboard, dialog, key))
                .flatten();
            let summary = if missing_runtime.is_some() {
                "No runtime  ›".to_owned()
            } else {
                summary
            };
            let summary = if choice_editor && index == dialog.selected {
                String::new()
            } else {
                summary
            };
            // Sign-in status is part of the profile list, not a detail hidden
            // inside the profile page: a signed-out row is visible before a
            // session is started.
            let summary = if !choice_editor
                && dialog.path == ["profiles"]
                && dialog.saved_identity_matches(key)
            {
                match dashboard.profile_login_status(key) {
                    Some(status) => {
                        let base = summary.strip_suffix("  ›").unwrap_or(&summary);
                        format!("{base}  {}  ›", status.label())
                    }
                    None => summary,
                }
            } else {
                summary
            };
            rows.push(setting_row(&name, &summary, body.width));
            row_map.push(Some(index));
            // A machine no runtime runs on is no session target, so its row
            // says why, the same reason the session picker gives.
            if let Some(note) = missing_runtime {
                let width = body.width.saturating_sub(SETTING_GUTTER.len() as u16 + 4);
                let warning = ratatui::style::Style::default().fg(theme::palette().warning);
                let lines = mj_chat::components::wrap_lines([Line::raw(note)], width);
                let cut = lines.len() > MACHINE_NOTE_LINES;
                for (number, line) in lines.into_iter().take(MACHINE_NOTE_LINES).enumerate() {
                    let mut text = line.to_string();
                    if cut && number + 1 == MACHINE_NOTE_LINES {
                        text = truncate(&format!("{text}…"), usize::from(width));
                    }
                    rows.push(Line::styled(format!("{SETTING_GUTTER}    {text}"), warning));
                    row_map.push(None);
                    row_enabled.push(true);
                }
            }
            let unavailable = mbx_unavailable
                .as_deref()
                .filter(|_| is_build_cache_field(&child_path, "enabled"));
            row_enabled.push(
                !((is_build_cache_budget(&child_path)
                    || is_build_cache_field(&child_path, "directory"))
                    && dialog.build_cache_user_managed()),
            );
            // Explain mbx availability without disabling native tool caches.
            if let Some(reason) = unavailable {
                rows.push(Line::styled(
                    truncate(
                        &format!("{SETTING_GUTTER}    mbx: {reason}"),
                        usize::from(body.width),
                    ),
                    theme::muted(),
                ));
                row_map.push(None);
                row_enabled.push(true);
            }
        }
        for note in dialog.build_cache_policy_lines() {
            let line = truncate(&note, usize::from(body.width).saturating_sub(4));
            rows.push(Line::styled(
                format!("{SETTING_GUTTER}{line}"),
                theme::muted(),
            ));
            row_map.push(None);
            row_enabled.push(true);
        }
        // What the cache has done, under the fields that configure it.
        if let Some(activity) = dialog.build_cache_stats_line() {
            for line in [String::new(), format!("{SETTING_GUTTER}{activity}")] {
                rows.push(Line::styled(
                    truncate(&line, usize::from(body.width)),
                    theme::muted(),
                ));
                row_map.push(None);
                row_enabled.push(true);
            }
        }
        if choice_editor {
            // The page remains visible behind a choice popup, but its controls
            // must not remain interactive through the overlay.
            let selected_row = row_map
                .iter()
                .position(|item| *item == Some(dialog.selected));
            let mut state = ListState::default()
                .with_offset(background_offset)
                .with_selected(selected_row);
            frame.render_stateful_widget(
                RatatuiList::new(rows.iter().cloned().map(ListItem::new).collect::<Vec<_>>())
                    .highlight_style(theme::selection(false)),
                body,
                &mut state,
            );
            background_offset = state.offset();
        } else {
            ChoiceList::render_with_rows(
                frame,
                body,
                &rows,
                dialog.selected,
                &row_map,
                &row_enabled,
                &mut form,
                List,
            );
        }
        initial = List;
    }
    if choice_editor {
        // Inert copies of the column and the footer, built from the same lists
        // so they cannot drift from what the page shows when no popup covers
        // them.
        Dialog::render_actions_stacked_inert(
            frame,
            column,
            &page_actions,
            &form,
            ColumnAlign::Right,
        );
        Dialog::render_actions_inert(frame, footer_row, &footer, &form);
    } else {
        Dialog::render_actions_stacked(frame, column, &page_actions, &mut form, ColumnAlign::Right);
        Dialog::render_actions(frame, footer_row, &footer, &mut form);
    }
    if choice_editor {
        let editor = dialog.editor.as_ref().expect("choice editor");
        let selected_row = dialog.selected.saturating_sub(background_offset);
        let field = Rect::new(
            body.x.saturating_add(34.min(body.width.saturating_sub(1))),
            body.y.saturating_add(
                u16::try_from(selected_row)
                    .unwrap_or(u16::MAX)
                    .min(body.height.saturating_sub(1)),
            ),
            body.width.saturating_sub(34),
            1,
        );
        let title = " values · ↑/↓ select · Tab/Enter accept ";
        let rows = editor
            .choices
            .iter()
            .map(|value| Line::raw(schema::choice_label(&editor.path, value, &dialog.draft)))
            .collect::<Vec<_>>();
        let selected = editor.combo.selection(Choices, editor.selected);
        let value = editor
            .choices
            .get(selected)
            .map(|value| schema::choice_label(&editor.path, value, &dialog.draft))
            .unwrap_or_default();
        ComboBox::render(
            frame,
            inner,
            field,
            &value,
            &rows,
            selected,
            editor.combo.is_open(Choices),
            true,
            title,
            PopupSide::Below,
            &mut form,
            Choices,
        );
        initial = Choices;
    }
    if let Some(notice) = notice {
        frame.render_widget(
            Paragraph::new(notice.as_str()).wrap(Wrap { trim: false }),
            editor_notice.unwrap_or_else(|| {
                Rect::new(
                    inner.x,
                    footer_row.y.saturating_sub(notice_rows),
                    inner.width,
                    notice_rows,
                )
            }),
        );
    }

    form.end_frame(initial);
}

/// Draws the search in place of the page: the query, then every setting that
/// matches it with the section it lives in and the value it holds now.
fn render_search(frame: &mut Frame, popup: Rect, inner: Rect, dialog: &SetupDialog) {
    use SetupControl::*;
    let search = dialog.search.as_ref().expect("an open search");
    let mut form = dialog.form.borrow_mut();
    form.begin_frame();
    let title = dismissible_modal_title(
        &mut form,
        popup,
        "Settings",
        theme::title(true),
        !dialog.saving,
    );
    frame.render_widget(
        theme::modal().title(title).title(
            Line::styled(
                format!(" {} ", counted(search.matches.len(), "setting", "settings")),
                theme::muted(),
            )
            .right_aligned(),
        ),
        popup,
    );
    frame.render_widget(
        Paragraph::new(
            "Search every setting by name, by what it does, or by its value. Esc returns to the list.",
        )
            .wrap(Wrap { trim: false })
            .style(theme::muted()),
        Rect::new(inner.x, inner.y, inner.width, 2),
    );
    let query = Rect::new(inner.x, inner.y + 2, inner.width, 1);
    TextField::render(frame, query, &search.input, &mut form, Search);
    if search.input.is_empty() {
        frame.render_widget(
            Line::styled(
                "Type to filter…",
                theme::muted().add_modifier(Modifier::ITALIC),
            ),
            query,
        );
    }
    let footer_row = mj_chat::components::DialogShell::layout(inner, 0).actions;
    let top = inner.y + 4;
    let results = Rect::new(
        inner.x,
        top,
        inner.width,
        footer_row.y.saturating_sub(top).max(1),
    );
    if search.matches.is_empty() {
        frame.render_widget(Line::raw("No matching setting"), results);
        form.register(
            Results,
            ControlKind::ChoiceList {
                len: 0,
                selected: 0,
            },
            results,
            false,
        );
    } else {
        let rows = search
            .matches
            .iter()
            .filter_map(|index| search.entries.get(*index))
            .map(|entry| search::row(entry, results.width))
            .collect::<Vec<_>>();
        ChoiceList::render(frame, results, &rows, search.selected, &mut form, Results);
    }
    Dialog::render_actions(frame, footer_row, &dialog.footer_actions(), &mut form);
    form.end_frame(Search);
}

/// Splits a settings page into its body and the stacked column of `actions`.
///
/// A page with no actions of its own keeps the full width; splitting on an
/// empty list would still surrender the body gap to an empty column.
fn split_page(
    form: &Dialog<SetupControl>,
    area: Rect,
    actions: &[(SetupControl, &'static str, bool)],
) -> ColumnSplit {
    if actions.is_empty() {
        return ColumnSplit {
            body: area,
            actions: Rect::new(area.right(), area.y, 0, area.height),
        };
    }
    form.split_actions(area, actions)
}
