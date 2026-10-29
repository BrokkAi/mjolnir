use super::*;

pub(super) const SETTING_GUTTER: &str = "  ";

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
    let popup = centered_modal_fixed(
        frame,
        surfaces,
        dialog.preferred_width,
        dialog.preferred_height,
        area,
    );
    if let Some(review) = &dialog.review_editor {
        render_review_settings(frame, popup, review, dialog.saving);
        return;
    }
    let inner = popup.inner(ratatui::layout::Margin {
        horizontal: 2,
        vertical: 1,
    });
    if dialog.managed_setup.is_some() {
        render_managed_setup(frame, popup, inner, dialog);
        return;
    }
    if dialog.search.is_some() {
        render_search(frame, popup, inner, dialog);
        return;
    }
    if dialog.report.is_some() {
        render_report(frame, popup, inner, dialog);
        return;
    }
    use SetupControl::*;
    let mut form = dialog.form.borrow_mut();
    form.begin_frame();
    let title = dismissible_modal_title(
        &mut form,
        popup,
        "Settings",
        theme::title(true),
        !dialog.saving && dialog.combo.open_id().is_none(),
    );
    frame.render_widget(
        theme::modal().title(title).title_bottom(Line::raw(
            " ↑/↓ move · Enter apply/open · Tab moves · Esc back ",
        )),
        popup,
    );
    let mut top = inner.y;
    if !dialog.path.is_empty() {
        frame.render_widget(
            Line::styled(breadcrumb(&dialog.path, &dialog.draft), theme::title(true)),
            Rect::new(inner.x, top, inner.width, 1),
        );
        top += 1;
    }
    let help = Paragraph::new(schema::page_help(&dialog.path, &dialog.draft))
        .wrap(Wrap { trim: false })
        .style(theme::muted());
    let help_rows = u16::try_from(help.line_count(inner.width))
        .unwrap_or(u16::MAX)
        .clamp(2, (inner.height / 3).max(2));
    frame.render_widget(help, Rect::new(inner.x, top, inner.width, help_rows));
    top += help_rows + 1;
    let footer_row = mj_chat::components::DialogShell::layout(inner, 0).actions;
    let notice_rows = dialog.notice.as_ref().map_or(0, |notice| {
        u16::try_from(
            Paragraph::new(notice.as_str())
                .wrap(Wrap { trim: false })
                .line_count(inner.width),
        )
        .unwrap_or(u16::MAX)
        .min(footer_row.y.saturating_sub(top).saturating_sub(2))
        .max(1)
    });
    let band = Rect::new(
        inner.x,
        top,
        inner.width,
        footer_row.y.saturating_sub(top + notice_rows),
    );
    let page_actions = dialog.page_actions();
    let ColumnSplit {
        body,
        actions: column,
    } = split_page(&form, band, &page_actions);
    let mut popups = (None, None);
    let initial = if let Some(editor) = &dialog.editor {
        frame.render_widget(
            Line::raw("Name for the new entry"),
            Rect::new(body.x, body.y, body.width, 1),
        );
        TextField::render(
            frame,
            Rect::new(body.x, body.y + 1, body.width, u16::from(body.height > 1)),
            &editor.input,
            &mut form,
            Field,
        );
        Field
    } else {
        let bounds = Rect::new(
            inner.x,
            inner.y,
            inner.width,
            footer_row.y.saturating_sub(inner.y),
        );
        popups = render_controls(frame, bounds, body, dashboard, dialog, &mut form);
        dialog
            .descriptors
            .iter()
            .find(|c| c.kind != SettingKind::ReadOnly)
            .map_or(Save, |c| c.id)
    };
    if dialog.combo.open_id().is_some() {
        Dialog::render_actions_stacked_inert(
            frame,
            column,
            &page_actions,
            &form,
            ColumnAlign::Right,
        );
        Dialog::render_actions_inert(frame, footer_row, &dialog.footer_actions(), &form);
    } else {
        Dialog::render_actions_stacked(frame, column, &page_actions, &mut form, ColumnAlign::Right);
        Dialog::render_actions(frame, footer_row, &dialog.footer_actions(), &mut form);
    }
    if let Some(notice) = &dialog.notice {
        frame.render_widget(
            Paragraph::new(notice.as_str()).wrap(Wrap { trim: false }),
            Rect::new(
                inner.x,
                footer_row.y.saturating_sub(notice_rows),
                inner.width,
                notice_rows,
            ),
        );
    }
    // Popups draw last, above both the page controls and its action column.
    if let Some(id) = dialog.combo.open_id()
        && let Some(control) = dialog.descriptor(id)
        && matches!(&control.kind, SettingKind::Choices(_))
        && let Some(field) = popups.0
    {
        render_combo(frame, inner, field, control, dialog, &mut form, true);
    }
    if let Some((id, field)) = popups.1
        && let Some(control) = dialog.descriptor(id)
        && let EditorInput::Path(input) = &dialog.fields[&control.path].input
    {
        let bounds = Rect::new(
            inner.x,
            inner.y,
            inner.width,
            footer_row.y.saturating_sub(inner.y),
        );
        PathField::render_within(frame, bounds, field, input, &mut form, id);
    }
    form.end_frame(initial);
}

fn render_combo(
    frame: &mut Frame,
    bounds: Rect,
    field: Rect,
    control: &SettingControl,
    dialog: &SetupDialog,
    form: &mut Dialog<SetupControl>,
    expanded: bool,
) {
    let SettingKind::Choices(values) = &control.kind else {
        return;
    };
    let committed = values
        .iter()
        .position(|v| dialog.draft.pointer(&pointer(&control.path)) == Some(v))
        .unwrap_or(0);
    let selected = dialog.combo.selection(control.id, committed);
    let rows = values
        .iter()
        .map(|v| Line::raw(dialog.choice_label(&control.path, v)))
        .collect::<Vec<_>>();
    let value = values
        .get(selected)
        .map(|v| dialog.choice_label(&control.path, v))
        .unwrap_or_default();
    ComboBox::render(
        frame,
        bounds,
        field,
        &value,
        &rows,
        selected,
        expanded,
        dialog.combo.open_id().is_none_or(|id| id == control.id),
        " values · ↑/↓ select · Tab/Enter accept ",
        PopupSide::Below,
        form,
        control.id,
    );
}

fn render_controls(
    frame: &mut Frame,
    bounds: Rect,
    body: Rect,
    dashboard: &DashboardState,
    dialog: &SetupDialog,
    form: &mut Dialog<SetupControl>,
) -> (Option<Rect>, Option<(SetupControl, Rect)>) {
    let mut expanded = None;
    let mut path_popup = None;
    // Layout entries refer to descriptors; no separate interactive row identity exists.
    let mut blocks = Vec::new();
    let mut at = 0u16;
    let keys = dialog
        .descriptors
        .iter()
        .map(|c| c.path.last().unwrap().clone())
        .collect::<Vec<_>>();
    for row in page_plan(&dialog.path, &keys) {
        match row {
            PageRow::Gap => {
                blocks.push((at, None, vec![Line::raw("")]));
                at += 1;
            }
            PageRow::Heading(heading) => {
                blocks.push((
                    at,
                    None,
                    vec![Line::styled(
                        format!("  {heading}"),
                        theme::muted().add_modifier(Modifier::BOLD),
                    )],
                ));
                at += 1;
            }
            PageRow::Setting(index) => {
                let control = &dialog.descriptors[index];
                let key = control.path.last().unwrap();
                let mut notes = Vec::new();
                if let Some(error) = dialog
                    .fields
                    .get(&control.path)
                    .and_then(|f| f.error.as_ref())
                {
                    notes.extend(mj_chat::components::wrap_lines(
                        [Line::styled(
                            error.clone(),
                            ratatui::style::Style::default().fg(theme::palette().warning),
                        )],
                        body.width,
                    ));
                }
                if is_build_cache_field(&control.path, "enabled") {
                    if let Some(preview) = dialog.current_build_cache_preview() {
                        notes.push(Line::styled(
                            if preview.off_reason.is_some() {
                                "Effective: Off"
                            } else {
                                "Effective: On"
                            },
                            theme::muted(),
                        ));
                    }
                    if let Some(reason) = dialog.mbx_unavailable() {
                        notes.extend(mj_chat::components::wrap_lines(
                            [Line::styled(format!("mbx: {reason}"), theme::muted())],
                            body.width,
                        ));
                    }
                }
                if key == "archive_after_days"
                    && let Some(estimate) = dialog.archive_space_estimate()
                {
                    notes.push(Line::styled(estimate, theme::muted()));
                }
                if dialog.path == ["machines"]
                    && let Some(note) = machine_without_runtime_note(dashboard, dialog, key)
                {
                    notes.extend(
                        mj_chat::components::wrap_lines(
                            [Line::styled(
                                note,
                                ratatui::style::Style::default().fg(theme::palette().warning),
                            )],
                            body.width,
                        )
                        .into_iter()
                        .take(MACHINE_NOTE_LINES),
                    );
                }
                if form.is_focused(control.id) {
                    let help = schema::help(&control.path);
                    if help
                        != "Enter opens or edits a setting. Changes stay in this draft until you save."
                        && help != schema::page_help(&dialog.path, &dialog.draft)
                    {
                        notes.extend(mj_chat::components::wrap_lines(
                            [Line::styled(help, theme::muted())],
                            body.width,
                        ));
                    }
                }
                let height = 1 + notes.len() as u16;
                blocks.push((at, Some(index), notes));
                at += height;
            }
        }
    }
    let mut tail = dialog.build_cache_policy_lines();
    if dialog.build_cache_user_managed() {
        tail.push(
            "This machine's mbx installation owns its budgets. Mjolnir does not modify them."
                .into(),
        );
    }
    if let Some(stats) = dialog.build_cache_stats_line() {
        tail.push(stats);
    }
    let tail = mj_chat::components::wrap_lines(tail.into_iter().map(Line::raw), body.width);
    let focused = form.focused();
    let focused_block = blocks.iter().find_map(|(start, index, notes)| {
        index
            .filter(|index| Some(dialog.descriptors[*index].id) == focused)
            .map(|_| *start..*start + 1 + notes.len() as u16)
    });
    let viewport = form.viewport(body, at + tail.len() as u16, focused_block);
    for (start, index, notes) in blocks {
        let Some(index) = index else {
            for (i, note) in notes.into_iter().enumerate() {
                frame.render_widget(note, viewport.row(start + i as u16, 1));
            }
            continue;
        };
        let control = &dialog.descriptors[index];
        let enabled = dialog.combo.open_id().is_none_or(|id| id == control.id);
        let key = control.path.last().unwrap();
        let value = dialog.draft.pointer(&pointer(&control.path)).unwrap();
        let parent = &control.path[..control.path.len() - 1];
        let label = row_label(
            parent,
            dialog.draft.pointer(&pointer(parent)).unwrap(),
            key,
            Some(value),
        );
        let row = viewport.row(start, 1);
        let label_width = (body.width / 2).min(34);
        let label_area = Rect::new(row.x, row.y, label_width.min(row.width), row.height);
        let field_area = Rect::new(
            row.x + label_area.width,
            row.y,
            row.width.saturating_sub(label_area.width),
            row.height,
        );
        match &control.kind {
            SettingKind::Page => {
                mj_chat::components::Button::render_aligned(
                    frame,
                    row,
                    &label,
                    enabled,
                    form,
                    control.id,
                    ratatui::layout::Alignment::Left,
                );
                let mut summary = row_summary(
                    &dialog.path,
                    key,
                    value,
                    &dialog.draft,
                    (key == "build_cache")
                        .then(|| dialog.build_cache_row_unavailable())
                        .flatten(),
                );
                if dialog.path == ["profiles"]
                    && dialog.saved_identity_matches(key)
                    && let Some(status) = dashboard.profile_login_status(key)
                {
                    summary = format!(
                        "{}  {}  ›",
                        summary.strip_suffix("  ›").unwrap_or(&summary),
                        status.label()
                    );
                }
                if dialog.path == ["machines"]
                    && machine_without_runtime_note(dashboard, dialog, key).is_some()
                {
                    summary = "No runtime  ›".to_owned();
                }
                frame.render_widget(Line::styled(summary, theme::muted()), field_area);
            }
            SettingKind::Checkbox => Checkbox::render(
                frame,
                row,
                &label,
                value.as_bool().unwrap_or(false),
                enabled,
                form,
                control.id,
            ),
            SettingKind::Choices(_) => {
                frame.render_widget(Line::raw(label), label_area);
                render_combo(frame, bounds, field_area, control, dialog, form, false);
                if dialog.combo.is_open(control.id) {
                    expanded = Some(field_area);
                }
            }
            SettingKind::Field => {
                frame.render_widget(Line::raw(label), label_area);
                let input = &dialog.fields[&control.path].input;
                if !enabled {
                    frame.render_widget(Line::raw(input.to_string()), field_area);
                } else {
                    match input {
                        EditorInput::Text(input) => {
                            TextField::render(frame, field_area, input, form, control.id)
                        }
                        EditorInput::Path(input) => PathField::render_within(
                            frame, bounds, field_area, input, form, control.id,
                        ),
                    }
                }
                if enabled
                    && form.is_focused(control.id)
                    && let EditorInput::Path(input) = input
                    && PathField::popup_rows(input) > 0
                {
                    path_popup = Some((control.id, field_area));
                }
                if input.is_empty()
                    && let Some(hint) = dialog.empty_field_hint(&control.path)
                {
                    frame.render_widget(
                        Line::styled(hint, theme::muted().add_modifier(Modifier::ITALIC)),
                        field_area,
                    );
                }
            }

            SettingKind::ManagedHarness(_) => unreachable!("managed chooser renders separately"),
            SettingKind::ReadOnly => {
                frame.render_widget(Line::styled(label, theme::muted()), label_area);
                let summary = dialog
                    .build_cache_automatic_label(key)
                    .unwrap_or_else(|| row_summary(&dialog.path, key, value, &dialog.draft, None));
                frame.render_widget(Line::styled(summary, theme::muted()), field_area);
            }
        }
        for (i, note) in notes.into_iter().enumerate() {
            frame.render_widget(note, viewport.row(start + 1 + i as u16, 1));
        }
    }
    for (i, note) in tail.into_iter().enumerate() {
        frame.render_widget(note.style(theme::muted()), viewport.row(at + i as u16, 1));
    }
    (expanded, path_popup)
}

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

fn managed_harness_description(kind: mj_core::config::HarnessKind) -> &'static str {
    use mj_core::config::HarnessKind;

    match kind {
        HarnessKind::Codex => "ChatGPT subscription or an OpenAI-compatible provider",
        HarnessKind::Claude => "Claude Pro, Max, Team, or an Anthropic API key",
        HarnessKind::Kimi => "Kimi Code subscription",
        HarnessKind::Grok => "Grok Build subscription",
        HarnessKind::Muse => "Muse account; container use is supported",
        HarnessKind::OpenCode => "OpenCode provider login or an API key",
        HarnessKind::Mimo => "MiMo Code account",
        HarnessKind::Dsh => "DeepSeek API key",
    }
}

fn render_managed_setup(frame: &mut Frame, popup: Rect, inner: Rect, dialog: &SetupDialog) {
    let mut form = dialog.form.borrow_mut();
    form.begin_frame();
    let title = dismissible_modal_title(
        &mut form,
        popup,
        " Set up a managed agent ",
        theme::title(true),
        !dialog.saving,
    );
    frame.render_widget(theme::modal().title(title), popup);
    let intro = Paragraph::new(
        "Choose an agent. Mjolnir will install its exact pinned version and open its sign-in flow.",
    )
    .wrap(Wrap { trim: false })
    .style(theme::muted());
    let intro_height = intro.line_count(inner.width) as u16;
    frame.render_widget(
        intro,
        Rect::new(inner.x, inner.y, inner.width, intro_height),
    );
    let footer = mj_chat::components::DialogShell::layout(inner, 0).actions;
    let guidance = Paragraph::new("Where it runs is chosen when you create a session. Use a local Podman, Docker, or Apple container when you want the agent isolated from your normal host account.").wrap(Wrap { trim: false }).style(theme::muted());
    let guidance_height = guidance.line_count(inner.width) as u16;
    let guidance_y = footer.y.saturating_sub(guidance_height + 1);
    frame.render_widget(
        guidance,
        Rect::new(inner.x, guidance_y, inner.width, guidance_height),
    );
    let top = inner.y + intro_height + 1;
    let body = Rect::new(inner.x, top, inner.width, guidance_y.saturating_sub(top));
    let focused = dialog
        .descriptors
        .iter()
        .position(|c| form.is_focused(c.id));
    let viewport = form.viewport(
        body,
        dialog.descriptors.len() as u16 * 2,
        focused.map(|i| i as u16 * 2..i as u16 * 2 + 2),
    );
    for (position, control) in dialog.descriptors.iter().enumerate() {
        let SettingKind::ManagedHarness(index) = control.kind else {
            continue;
        };
        let kind = MANAGED_SETUP_HARNESSES[index];
        mj_chat::components::Button::render_aligned(
            frame,
            viewport.row(position as u16 * 2, 1),
            kind.display_name(),
            !dialog.saving,
            &mut form,
            control.id,
            ratatui::layout::Alignment::Left,
        );
        frame.render_widget(
            Line::styled(managed_harness_description(kind), theme::muted()),
            viewport.row(position as u16 * 2 + 1, 1),
        );
    }
    Dialog::render_actions(frame, footer, &dialog.actions(), &mut form);
    let initial = dialog
        .descriptors
        .first()
        .map_or(SetupControl::Back, |c| c.id);
    form.end_frame(initial);
}
