use super::*;

/// Button labels for a confirmation dialog, ordered Cancel first and the primary
/// action last. This is the single declaration used by both key handling and
/// rendering.
pub(crate) fn confirmation_buttons(confirmation: &Confirmation) -> &'static [&'static str] {
    match confirmation {
        Confirmation::RepairRepositoryRemotes { .. } => &["Cancel", "Repair and continue"],
        Confirmation::ConfigurationRepair { .. } => {
            &["Dismiss", "Open transcript", "Open settings"]
        }
        Confirmation::LaunchFailed { retry: Some(_), .. } => {
            &["Dismiss", "Retry launch", "Settings"]
        }
        Confirmation::LaunchFailed { .. } => &["Dismiss", "Settings"],
        Confirmation::Dismiss {
            intent: DismissalIntent::DiscardSetup,
            ..
        } => &["Keep editing", "Discard settings"],
        Confirmation::Dismiss {
            intent: DismissalIntent::CancelImport,
            ..
        } => &["Keep importing", "Cancel import"],
        Confirmation::ConvertRawCheckout { .. } => &["Cancel", "Confirm"],
        Confirmation::DestroyStopped { .. } => {
            &["Cancel", "Destroy session", "Destroy and delete branch"]
        }
        Confirmation::CloseFailed {
            can_discard: false, ..
        } => &["Cancel", "Retry suspension"],
        Confirmation::CloseFailed {
            can_discard: true, ..
        } => &[
            "Cancel",
            "Discard changes since checkpoint…",
            "Retry suspension",
        ],
        Confirmation::DiscardSinceCheckpoint { .. } => &["Cancel", "Discard changes"],
        Confirmation::SuspendSession { .. } => &["Cancel", "Suspend session"],
        Confirmation::InterruptWork { restart: false, .. } => &["Cancel", "Suspend now"],
        Confirmation::InterruptWork { restart: true, .. } => &["Cancel", "Restart now"],
        Confirmation::RecoverFailed {
            recoverable: true, ..
        } => &["Cancel", "Open transcript", "Recover"],
        Confirmation::RecoverFailed { .. } => &["Cancel", "Open transcript"],
        Confirmation::RecoverMove { operation }
            if operation.queue_admission_started && !operation.queue_admission_finished =>
        {
            &["Cancel", "Open transcript", "Retry move"]
        }
        Confirmation::RecoverMove { .. } => &[
            "Cancel",
            "Open transcript",
            "Retry move",
            "Resume previous settings",
        ],
        Confirmation::ForceDestroy { .. } => {
            &["Cancel", "Destroy session", "Destroy and delete branch"]
        }
    }
}

/// One letter per button, in button order, so every confirmation answers a
/// key without Tab. The letter is the first letter of the label's first word
/// that no earlier button took, then of a later word, then any later letter
/// of the label; a label with nothing left gets no letter (`\0`).
///
/// `Yes` and `Yes, delete branch` therefore answer `y` and `d`; `Cancel` and
/// `Confirm` answer `c` and `o`.
pub(crate) fn confirmation_accelerators(labels: &[&str]) -> Vec<char> {
    let mut taken = Vec::new();
    labels
        .iter()
        .map(|label| {
            let lower = label.to_lowercase();
            let candidates = lower
                .split(|character: char| !character.is_alphanumeric())
                .filter_map(|word| word.chars().next())
                .chain(lower.chars().filter(char::is_ascii_alphanumeric));
            let letter = candidates
                .into_iter()
                .find(|letter| !taken.contains(letter))
                .unwrap_or('\0');
            taken.push(letter);
            letter
        })
        .collect()
}

/// The line under a confirmation's text naming each button's letter, in the
/// same order the buttons are drawn: `n No · y Yes · d Yes, delete branch`.
pub(crate) fn confirmation_key_line(labels: &[&str]) -> Line<'static> {
    let accelerators = confirmation_accelerators(labels);
    let mut spans = Vec::new();
    for (index, (label, letter)) in labels.iter().zip(accelerators).enumerate() {
        if index > 0 {
            spans.push(Span::styled("  ·  ", theme::muted()));
        }
        if letter != '\0' {
            spans.push(Span::styled(
                format!("{letter} "),
                Style::default()
                    .fg(theme::palette().secondary)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        spans.push(Span::styled((*label).to_owned(), theme::muted()));
    }
    Line::from(spans)
}

/// Index of the primary (rightmost) button, which is focused when a dialog opens.
pub(crate) fn primary_button(labels: &[&str]) -> usize {
    labels.len().saturating_sub(1)
}

pub(crate) fn initial_confirmation_button(confirmation: &Confirmation, labels: &[&str]) -> usize {
    if matches!(
        confirmation,
        Confirmation::Dismiss { .. }
            | Confirmation::ConfigurationRepair { .. }
            | Confirmation::LaunchFailed { .. }
            | Confirmation::ForceDestroy { .. }
            | Confirmation::DestroyStopped { .. }
            | Confirmation::CloseFailed { .. }
            | Confirmation::DiscardSinceCheckpoint { .. }
            | Confirmation::SuspendSession { .. }
            | Confirmation::InterruptWork { .. }
            | Confirmation::RepairRepositoryRemotes { .. }
            | Confirmation::ConvertRawCheckout { .. }
    ) {
        0
    } else {
        primary_button(labels)
    }
}

pub(crate) fn import_progress_status(progress: &ImportProgress) -> Line<'static> {
    let stalled_for = progress.last_updated.elapsed();
    if stalled_for >= IMPORT_STALL_WARNING_AFTER {
        Line::styled(
            format!(
                "No progress for {}s; the filesystem may be stalled.",
                stalled_for.as_secs()
            ),
            Style::default().fg(theme::palette().warning),
        )
    } else {
        Line::styled(
            "The dashboard remains responsive while the import runs.",
            Style::default().fg(theme::palette().muted),
        )
    }
}

/// Finds an active import even while a cancellation confirmation temporarily
/// owns the foreground. Progress replies must keep updating the preserved
/// dialog so rejecting the confirmation returns to current state.
pub(crate) fn import_progress_mut(mode: &mut Mode) -> Option<&mut ImportProgress> {
    match mode {
        Mode::Importing(progress) => Some(progress),
        Mode::Help(overlay) => import_progress_mut(overlay.return_to.as_mut()),
        Mode::Confirm(dialog) => match &mut dialog.confirmation {
            Confirmation::Dismiss { mode, .. } => import_progress_mut(mode.as_mut()),
            _ => None,
        },
        _ => None,
    }
}

pub(crate) fn render_import_progress(
    frame: &mut Frame,
    area: Rect,
    progress: &ImportProgress,
    surfaces: &mut FrameSurfaces,
) {
    let total = progress
        .total
        .map_or_else(|| "?".into(), |total| total.to_string());
    let status = import_progress_status(progress);
    let paragraph = Paragraph::new(vec![
        Line::styled(
            truncate_to_cells(&progress.session_title, 60, Truncate::SUMMARY),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Line::raw(""),
        Line::raw(progress.message.clone()),
        status,
        Line::raw(""),
    ])
    .wrap(Wrap { trim: false });
    let popup = centered_modal(
        frame,
        surfaces,
        76,
        popup_height(&paragraph, 76, 11, area),
        area,
    );
    let inner = popup.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 1,
    });
    if inner.height == 0 {
        clear_dialog_form_geometry(&mut progress.form.borrow_mut());
        return;
    }
    frame.render_widget(
        paragraph,
        Rect::new(
            inner.x,
            inner.y,
            inner.width,
            inner.height.saturating_sub(1),
        ),
    );
    let mut form = progress.form.borrow_mut();
    form.begin_frame();
    let title = dismissible_modal_title(
        &mut form,
        popup,
        format!("Importing session · progress {}/{}", progress.step, total),
        theme::title(true),
        true,
    );
    frame.render_widget(theme::modal().title(title), popup);
    Button::render(
        frame,
        Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1),
        "Cancel",
        true,
        &mut form,
        DialogControl::Cancel,
    );
    form.end_frame(DialogControl::Cancel);
}

pub(crate) fn render_import_bundle_confirmation(
    frame: &mut Frame,
    area: Rect,
    confirmation: &ImportBundleConfirmation,
    surfaces: &mut FrameSurfaces,
) {
    let mut lines = Vec::new();
    if !confirmation.dirty_git_roots.is_empty() {
        lines.push(Line::raw(
            "These Git roots have local changes; Mjolnir will archive tracked changes:",
        ));
        lines.extend(
            confirmation.dirty_git_roots.iter().map(|root| {
                Line::styled(root.clone(), Style::default().fg(theme::palette().warning))
            }),
        );
    }
    if !confirmation.omitted_non_git_dirs.is_empty() {
        if !lines.is_empty() {
            lines.push(Line::raw(""));
        }
        lines.push(Line::raw(
            "These edited directories are outside Git and cannot be included:",
        ));
        lines.extend(confirmation.omitted_non_git_dirs.iter().map(|directory| {
            Line::styled(
                directory.clone(),
                Style::default().fg(theme::palette().warning),
            )
        }));
    }
    if !confirmation.scratch_git_roots.is_empty() {
        if !lines.is_empty() {
            lines.push(Line::raw(""));
        }
        lines.push(Line::raw(
            "These scratch repositories are under temporary directories and stay out of the workspace:",
        ));
        lines.extend(
            confirmation.scratch_git_roots.iter().map(|root| {
                Line::styled(root.clone(), Style::default().fg(theme::palette().warning))
            }),
        );
    }
    lines.push(Line::raw(""));
    if confirmation.has_untracked_files {
        lines.push(Line::raw("Space toggles the checkbox."));
    }
    lines.push(Line::raw(if confirmation.managed_worktree.available {
        if confirmation.create_managed_worktree {
            "On resume, create a separate session-owned checkout."
        } else {
            "On resume, use the imported session's directory directly."
        }
    } else {
        "This import uses an isolated workspace."
    }));
    let control_lines = usize::from(confirmation.has_untracked_files)
        + usize::from(confirmation.managed_worktree.available)
        + 2;
    let body_paragraph = Paragraph::new(lines.clone()).wrap(Wrap { trim: false });
    let height = popup_height(
        &body_paragraph,
        76,
        u16::try_from(control_lines)
            .unwrap_or(u16::MAX)
            .saturating_add(10),
        area,
    );
    let popup = centered_modal(frame, surfaces, 76, height, area);
    let inner = popup.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 1,
    });
    if inner.height == 0 {
        clear_dialog_form_geometry(&mut confirmation.form.borrow_mut());
        return;
    }
    let body_height = inner
        .height
        .saturating_sub(u16::try_from(control_lines).unwrap_or(u16::MAX));
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }),
        Rect::new(inner.x, inner.y, inner.width, body_height),
    );
    let mut form = confirmation.form.borrow_mut();
    form.begin_frame();
    let title =
        dismissible_modal_title(&mut form, popup, "Confirm import", theme::title(true), true);
    frame.render_widget(theme::modal().title(title), popup);
    let y = inner.y.saturating_add(body_height);
    // The worktree choice only exists when the import can create one.
    if confirmation.managed_worktree.available {
        Checkbox::render(
            frame,
            Rect::new(inner.x, y, inner.width, 1),
            "Create managed worktree",
            confirmation.create_managed_worktree,
            true,
            &mut form,
            DialogControl::ImportManagedWorktree,
        );
    }
    let y = y.saturating_add(u16::from(confirmation.managed_worktree.available));
    if confirmation.has_untracked_files {
        Checkbox::render(
            frame,
            Rect::new(inner.x, y, inner.width, 1),
            "Ignore untracked files",
            confirmation.ignore_untracked,
            true,
            &mut form,
            DialogControl::ImportIgnore,
        );
    }
    let footer = Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1);
    Dialog::render_actions(
        frame,
        footer,
        &[
            (DialogControl::ImportCancel, "Cancel", true),
            (DialogControl::ImportContinue, "Continue", true),
        ],
        &mut form,
    );
    form.end_frame(DialogControl::ImportContinue);
}

/// Draws a one-field modal: a header line naming what is being renamed, the
/// field itself, and a Cancel/Save footer. The rename editors are the two
/// dialogs shaped this way.
fn render_text_prompt(
    frame: &mut Frame,
    area: Rect,
    form: &RefCell<Dialog<DialogControl>>,
    value: &TextInput,
    header: &str,
    title: &str,
    surfaces: &mut FrameSurfaces,
) {
    let popup = centered_modal(frame, surfaces, 60, 8, area);
    let inner = popup.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 1,
    });
    if inner.height == 0 {
        clear_dialog_form_geometry(&mut form.borrow_mut());
        return;
    }
    frame.render_widget(
        Paragraph::new(header),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    let field = Rect::new(inner.x, inner.y.saturating_add(2), inner.width, 1);
    let footer = Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1);
    let mut form = form.borrow_mut();
    form.begin_frame();
    let title = dismissible_modal_title(&mut form, popup, title, theme::title(true), true);
    frame.render_widget(theme::modal().title(title), popup);
    TextField::render(frame, field, value, &mut form, DialogControl::Field);
    Dialog::render_actions(
        frame,
        footer,
        &[
            (DialogControl::Cancel, "Cancel", true),
            (DialogControl::Save, "Save", true),
        ],
        &mut form,
    );
    form.end_frame(DialogControl::Field);
}

/// The age column of the notice log, one cell per age in seconds: each
/// "… ago" right-aligned to the widest one and followed by a space, so the
/// messages start in the same column whether an age reads "44s" or "1m17s".
pub(crate) fn notice_log_ages(ages: &[u64]) -> Vec<String> {
    let ages = ages
        .iter()
        .map(|&secs| format!("{} ago", mj_client::usage_format::format_clock(secs)))
        .collect::<Vec<_>>();
    let width = ages
        .iter()
        .map(|age| age.chars().count())
        .max()
        .unwrap_or(0);
    ages.into_iter()
        .map(|age| format!("{age:>width$} "))
        .collect()
}

/// The notice log: one row per remembered notice, newest first, with how
/// long ago it was reported.
pub(crate) fn render_notice_log(
    frame: &mut Frame,
    area: Rect,
    dashboard: &DashboardState,
    dialog: &NoticeLogDialog,
    surfaces: &mut FrameSurfaces,
) {
    let history = dashboard.notices.history();
    let popup_height = u16::try_from(history.len().saturating_add(5).clamp(8, 30)).unwrap_or(30);
    let popup = centered_modal(frame, surfaces, 80, popup_height, area);
    let inner = popup.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 1,
    });
    if inner.height < 2 {
        clear_dialog_form_geometry(&mut dialog.form.borrow_mut());
        return;
    }
    let mut form = dialog.form.borrow_mut();
    form.begin_frame();
    let title = dismissible_modal_title(
        &mut form,
        popup,
        "Recent messages",
        theme::title(true),
        true,
    );
    frame.render_widget(theme::modal().title(title), popup);
    let list_area = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner.height.saturating_sub(2),
    );
    let footer = Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1);
    if history.is_empty() {
        frame.render_widget(
            Paragraph::new("Nothing has been reported yet.").style(theme::muted()),
            list_area,
        );
    } else {
        let now = std::time::Instant::now();
        let ages = notice_log_ages(
            &history
                .iter()
                .map(|record| now.saturating_duration_since(record.at).as_secs())
                .collect::<Vec<_>>(),
        );
        // Every message is kept whole: a long failure wraps after its age
        // column rather than losing its tail, which is the part that says
        // what went wrong. The widget wraps by cell, so an unbroken path or
        // token still wraps instead of running off the edge.
        let lines = history
            .iter()
            .zip(ages)
            .map(|(record, age)| {
                let style = if record.failure {
                    Style::default().fg(theme::palette().warning)
                } else {
                    Style::default().fg(theme::palette().text)
                };
                Line::from(vec![
                    Span::styled(age, theme::muted()),
                    Span::styled(record.text.clone(), style),
                ])
            })
            .collect::<Vec<_>>();
        let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
        let total = paragraph.line_count(list_area.width.max(1));
        let max_scroll = total.saturating_sub(usize::from(list_area.height));
        dialog.max_scroll.set(max_scroll);
        let scroll = dialog.scroll.min(max_scroll);
        frame.render_widget(
            paragraph.scroll((u16::try_from(scroll).unwrap_or(u16::MAX), 0)),
            list_area,
        );
    }
    Dialog::render_actions(
        frame,
        footer,
        &[(DialogControl::NoticeLogClose, "Close", true)],
        &mut form,
    );
    form.end_frame(DialogControl::NoticeLogClose);
}

/// The changed-files overlay: the branch line, the totals, then one row per
/// file with its kind and line counts.
pub(crate) fn render_changed_files(
    frame: &mut Frame,
    area: Rect,
    dashboard: &DashboardState,
    dialog: &ChangedFilesDialog,
    surfaces: &mut FrameSurfaces,
) {
    let status = dashboard.git_status.get(&dialog.session_id);
    let files = status
        .and_then(|status| status.as_ref().ok())
        .map(|status| status.changed.as_slice())
        .unwrap_or_default();
    let popup_height = u16::try_from(files.len().saturating_add(7).clamp(9, 40)).unwrap_or(40);
    let popup = centered_modal(frame, surfaces, 80, popup_height, area);
    let inner = popup.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 1,
    });
    if inner.height < 3 {
        clear_dialog_form_geometry(&mut dialog.form.borrow_mut());
        return;
    }
    let mut form = dialog.form.borrow_mut();
    form.begin_frame();
    let name = dashboard
        .state
        .sessions
        .get(&dialog.session_id)
        .map(|session| session.display_title().to_owned())
        .unwrap_or_else(|| dialog.session_id.clone());
    let title = dismissible_modal_title(
        &mut form,
        popup,
        format!("Changed files · {name}"),
        theme::title(true),
        true,
    );
    frame.render_widget(theme::modal().title(title), popup);

    let header = Rect::new(inner.x, inner.y, inner.width, 1);
    let list_area = Rect::new(
        inner.x,
        inner.y.saturating_add(2),
        inner.width.saturating_sub(1),
        inner.height.saturating_sub(4),
    );
    let footer = Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1);
    let header_line = match status {
        None => Line::styled(
            format!("Reading the checkout{}", theme::glyphs().ellipsis),
            theme::muted(),
        ),
        Some(Err(error)) => Line::styled(
            format!("Could not read the checkout: {error}"),
            Style::default().fg(theme::palette().warning),
        ),
        Some(Ok(status)) => {
            let (added, removed) = status.totals();
            let branch = status.row_text();
            let branch = if branch.is_empty() {
                status.branch.clone()
            } else {
                branch
            };
            Line::from(vec![
                Span::styled(branch, theme::title(true)),
                Span::styled(
                    format!(
                        "   {} · +{added} −{removed}",
                        crate::widgets::counted(status.changed.len(), "file", "files")
                    ),
                    theme::muted(),
                ),
            ])
        }
    };
    frame.render_widget(Paragraph::new(header_line), header);

    if files.is_empty() {
        let text = match status {
            Some(Ok(_)) => "Nothing has changed since the last commit.",
            _ => "",
        };
        frame.render_widget(Paragraph::new(text).style(theme::muted()), list_area);
    } else {
        // The longest status word is eight cells (`modified`, `conflict`), so
        // the column is nine: a word that fills it must still leave a space
        // before the path.
        let kind_width = 9;
        let count_width = 12;
        let path_width = usize::from(list_area.width).saturating_sub(kind_width + count_width + 2);
        let rows = files
            .iter()
            .skip(dialog.scroll)
            .take(usize::from(list_area.height))
            .map(|file| {
                let counts = match (file.added, file.removed) {
                    (Some(added), Some(removed)) => format!("+{added} −{removed}"),
                    _ => String::new(),
                };
                Line::from(vec![
                    Span::styled(format!("{:<kind_width$}", file.kind()), theme::muted()),
                    Span::raw(truncate_to_cells(&file.path, path_width, Truncate::PLAIN)),
                    Span::styled(
                        format!(
                            "{:>width$}",
                            counts,
                            width = usize::from(list_area.width)
                                .saturating_sub(
                                    kind_width + path_width.min(file.path.chars().count())
                                )
                                .min(count_width + 2)
                        ),
                        theme::muted(),
                    ),
                ])
            })
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(rows), list_area);
        crate::render::sessions::render_session_scrollbar(
            frame,
            Rect::new(list_area.right(), list_area.y, 1, list_area.height),
            files.len(),
            dialog.scroll,
            usize::from(list_area.height).max(1),
        );
    }
    Dialog::render_actions(
        frame,
        footer,
        &[
            (DialogControl::ChangedFilesRefresh, "Refresh (r)", true),
            (DialogControl::ChangedFilesClose, "Close", true),
        ],
        &mut form,
    );
    form.end_frame(DialogControl::ChangedFilesClose);
}

pub(crate) fn render_rename_editor(
    frame: &mut Frame,
    area: Rect,
    editor: &RenameEditor,
    surfaces: &mut FrameSurfaces,
) {
    render_text_prompt(
        frame,
        area,
        &editor.form,
        &editor.title,
        &format!("Session: {}", editor.session_name),
        "Rename session",
        surfaces,
    );
}

pub(crate) fn render_config_id_editor(
    frame: &mut Frame,
    area: Rect,
    editor: &ConfigIdEditor,
    surfaces: &mut FrameSurfaces,
) {
    render_text_prompt(
        frame,
        area,
        &editor.form,
        &editor.value,
        &format!("Current {} ID: {}", editor.kind.label(), editor.old_id),
        &format!("Rename {} ID", editor.kind.label()),
        surfaces,
    );
}

pub(crate) fn render_target_actions(
    frame: &mut Frame,
    area: Rect,
    dashboard: &DashboardState,
    dialog: &TargetActionsDialog,
    surfaces: &mut FrameSurfaces,
) {
    let rows = dialog
        .target_ids
        .iter()
        .map(|id| {
            let kind = dashboard
                .config
                .targets
                .get(id)
                .map(target_kind_label)
                .unwrap_or("missing");
            Line::from(vec![
                Span::styled(format!("{id:<24} "), theme::title(false)),
                Span::styled(kind, theme::muted()),
            ])
        })
        .collect::<Vec<_>>();
    let list_rows = if rows.is_empty() {
        vec![Line::styled(
            "No targets configured.",
            Style::default().fg(theme::palette().muted),
        )]
    } else {
        rows
    };
    let height = u16::try_from(list_rows.len())
        .unwrap_or(u16::MAX)
        .saturating_add(8)
        .max(12);
    let popup = centered_modal(frame, surfaces, 72, height, area);
    let inner = popup.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 1,
    });
    if inner.height == 0 {
        clear_dialog_form_geometry(&mut dialog.form.borrow_mut());
        return;
    }
    let list_height = u16::try_from(list_rows.len())
        .unwrap_or(u16::MAX)
        .min(inner.height.saturating_sub(4));
    let list_area = Rect::new(inner.x, inner.y, inner.width, list_height.max(1));
    let status_y = list_area.bottom().saturating_add(1);
    if let Some(target_id) = &dialog.testing {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                mj_chat::spinner::compact_span(
                    dashboard.config.spinner,
                    mj_chat::spinner::elapsed_ms(),
                ),
                Span::styled(
                    format!(" Testing {target_id}{}", theme::glyphs().ellipsis),
                    Style::default().fg(theme::palette().accent),
                ),
                Span::styled(
                    match dashboard.first_key_label(crate::CommandId::CancelOperation) {
                        Some(key) => format!(" {key} cancels test"),
                        None => " Esc cancels test".to_owned(),
                    },
                    theme::muted(),
                ),
            ])),
            Rect::new(inner.x, status_y, inner.width, 1),
        );
    } else if let Some((target_id, result)) = &dialog.result {
        frame.render_widget(
            Paragraph::new(match result {
                Ok(()) => format!("{target_id}: ready"),
                Err(error) => format!("{target_id}: {error}"),
            })
            .style(Style::default().fg(if result.is_ok() {
                theme::palette().success
            } else {
                theme::palette().warning
            })),
            Rect::new(inner.x, status_y, inner.width, 1),
        );
    }
    let hint = Rect::new(inner.x, inner.bottom().saturating_sub(2), inner.width, 1);
    frame.render_widget(
        Paragraph::new("Up/Down selects target · Tab selects action · Esc closes")
            .style(Style::default().fg(theme::palette().muted)),
        hint,
    );
    let footer = Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1);
    let mut form = dialog.form.borrow_mut();
    form.begin_frame();
    let title =
        dismissible_modal_title(&mut form, popup, "Target actions", theme::title(true), true);
    frame.render_widget(theme::modal().title(title), popup);
    ChoiceList::render(
        frame,
        list_area,
        &list_rows,
        dialog.target_index,
        &mut form,
        DialogControl::TargetList,
    );
    Dialog::render_actions(
        frame,
        footer,
        &[
            (DialogControl::TargetRename, "Rename", true),
            (DialogControl::TargetTest, "Test", dialog.testing.is_none()),
            (
                DialogControl::TargetSettings,
                "Settings",
                dialog.testing.is_none(),
            ),
        ],
        &mut form,
    );
    form.end_frame(DialogControl::TargetList);
}

pub(crate) fn target_kind_label(target: &mj_core::config::TargetTemplate) -> &'static str {
    match target {
        mj_core::config::TargetTemplate::LocalBare => "local bare",
        mj_core::config::TargetTemplate::LocalPodman { .. } => "local Podman",
        mj_core::config::TargetTemplate::LocalDocker { .. } => "local Docker",
        mj_core::config::TargetTemplate::AppleContainer { .. } => "Apple container",
        mj_core::config::TargetTemplate::AwsEc2 { .. } => "AWS EC2",
        mj_core::config::TargetTemplate::SshBare { .. } => "SSH bare",
        mj_core::config::TargetTemplate::SshPodman { .. } => "SSH Podman",
        mj_core::config::TargetTemplate::SshDocker { .. } => "SSH Docker",
    }
}

pub(crate) fn render_web_dialog(
    frame: &mut Frame,
    area: Rect,
    dialog: &WebDialog,
    surfaces: &mut FrameSurfaces,
) {
    // Text that names the natural body width. The box hugs the QR, and longer
    // URLs wrap beneath it rather than stretching the dialog across the screen.
    pub(crate) const MIN_INNER_WIDTH: usize = 40;

    // The QR is the widest single element, so it decides the box width and only
    // shows when the terminal can hold it plus a border and the footer rows.
    let qr_lines: Vec<&str> = dialog
        .qr
        .as_deref()
        .map(|qr| qr.lines().collect())
        .unwrap_or_default();
    let qr_width = qr_lines.iter().map(|line| line.chars().count()).max();
    // Size against the region a modal may occupy so the QR fit and the box width
    // both respect the screen-edge margin that centering will enforce.
    let inner_area = modal_area(area);
    let max_inner = usize::from(inner_area.width).saturating_sub(2);
    let max_qr_height = usize::from(inner_area.height).saturating_sub(6);
    let show_qr =
        matches!(qr_width, Some(width) if width <= max_inner) && qr_lines.len() <= max_qr_height;

    let mut inner_width = MIN_INNER_WIDTH;
    let mut lines = Vec::new();
    if let Some(process) = &dialog.confirm_stop {
        inner_width = 60;
        lines.push(Line::styled(
            "Stop this Mjolnir server?",
            Style::default().fg(theme::palette().warning),
        ));
        lines.push(Line::raw(format!("{} · PID {}", process.name, process.pid)));
        lines.push(Line::raw(process.executable.display().to_string()));
        lines.push(Line::raw(""));
        lines.push(Line::raw("Other viewers and dashboards using that server will be disconnected. Mjolnir will request a graceful stop, then retry this port."));
    } else if dialog.loading {
        lines.push(Line::styled(
            format!("Starting web viewer{}", theme::glyphs().ellipsis),
            Style::default().fg(theme::palette().warning),
        ));
    } else if let Some(message) = &dialog.message {
        inner_width = 60;
        lines.extend(message.lines().map(|line| {
            Line::styled(
                line.to_owned(),
                Style::default().fg(theme::palette().warning),
            )
        }));
        if let Some(address) = dialog.failed_address {
            lines.push(Line::raw(format!("Address: {address}")));
            lines.push(Line::raw(""));
            lines.push(Line::raw("Use another port to get connected now. The new port lasts until the daemon restarts."));
            if dialog.port_conflict {
                lines.push(Line::raw(
                    "Inspect the port to see which process is using it.",
                ));
            }
        }
        if dialog.inspecting {
            lines.push(Line::styled(
                format!("Inspecting listener{}", theme::glyphs().ellipsis),
                Style::default().fg(theme::palette().accent),
            ));
        }
        if let Some(message) = &dialog.inspection_message {
            lines.push(Line::raw(""));
            lines.extend(message.lines().map(|line| Line::raw(line.to_owned())));
        }
        if let Some(process) = dialog.listeners.get(dialog.listener_index) {
            lines.push(Line::raw(""));
            lines.push(Line::raw(format!(
                "Process {} of {}: {} (PID {})",
                dialog.listener_index + 1,
                dialog.listeners.len(),
                process.name,
                process.pid
            )));
            lines.push(Line::raw(process.executable.display().to_string()));
            if let Some(reason) = &process.stop_disabled_reason {
                lines.push(Line::raw(reason.clone()));
            }
        }
    } else {
        if show_qr {
            inner_width = inner_width.max(qr_width.unwrap_or(0));
            lines.extend(
                qr_lines
                    .iter()
                    .map(|line| Line::raw((*line).to_owned()).centered()),
            );
            lines.push(Line::raw(""));
        } else if qr_width.is_some() {
            lines.push(
                Line::styled(
                    "Terminal is too small for a scannable QR code.",
                    Style::default().fg(theme::palette().warning),
                )
                .centered(),
            );
            lines.push(Line::raw(""));
        }
        if let Some(url) = &dialog.viewer_url {
            // The QR encodes this URL; the text is the fallback for hand entry,
            // so it wraps within the box instead of widening it.
            lines.push(Line::from(vec![
                Span::styled("Web: ", Style::default().fg(theme::palette().muted)),
                Span::styled(url.clone(), Style::default().fg(theme::palette().accent)),
            ]));
        }
        if let Some(code) = &dialog.viewer_code {
            lines.push(Line::from(vec![
                Span::styled("Viewer code: ", Style::default().fg(theme::palette().muted)),
                Span::styled(code.clone(), Style::default().fg(theme::palette().accent)),
            ]));
        }
        if let Some(reason) = &dialog.fallback_reason {
            lines.push(Line::styled(
                format!("Local fallback: {reason}"),
                Style::default().fg(theme::palette().warning),
            ));
        }
    }
    lines.push(Line::raw(""));

    let inner_width = inner_width.min(max_inner).max(1);
    let box_width = u16::try_from(inner_width + 2).unwrap_or(u16::MAX);
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let wrapped =
        u16::try_from(paragraph.line_count(box_width.saturating_sub(2))).unwrap_or(u16::MAX);
    let button_rows = dialog.button_rows();
    let footer_height = u16::try_from(button_rows.len()).unwrap_or(u16::MAX);
    let box_height = wrapped
        .saturating_add(2 + footer_height)
        .min(inner_area.height);
    let popup = centered_modal_fixed(frame, surfaces, box_width, box_height, area);
    let inner = popup.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 1,
    });
    if inner.height == 0 {
        clear_dialog_form_geometry(&mut dialog.form.borrow_mut());
        return;
    }
    let body = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner.height.saturating_sub(footer_height),
    );
    frame.render_widget(paragraph, body);
    let mut form = dialog.form.borrow_mut();
    form.begin_frame();
    let title = dismissible_modal_title(&mut form, popup, "Web viewer", theme::title(true), true);
    frame.render_widget(theme::modal().title(title), popup);
    let footer_top = inner.bottom().saturating_sub(footer_height).max(inner.y);
    for (index, buttons) in button_rows.iter().enumerate() {
        let y = footer_top.saturating_add(index as u16);
        let footer = if y < inner.bottom() {
            Rect::new(inner.x, y, inner.width, 1)
        } else {
            Rect::default()
        };
        Dialog::render_actions(frame, footer, buttons, &mut form);
    }
    form.end_frame(dialog.default_control());
}

pub(crate) fn render_qr(data: &str) -> Result<String, String> {
    pub(crate) const QUIET_ZONE: usize = 4;
    let qr = QrCode::with_error_correction_level(data.as_bytes(), EcLevel::L)
        .map_err(|error| format!("encode web login QR: {error}"))?;
    let total = qr.width() + QUIET_ZONE * 2;
    let mut output = String::new();
    for y in (0..total).step_by(2) {
        for x in 0..total {
            let module = |x: usize, y: usize| {
                let Some(x) = x.checked_sub(QUIET_ZONE) else {
                    return false;
                };
                let Some(y) = y.checked_sub(QUIET_ZONE) else {
                    return false;
                };
                x < qr.width() && y < qr.width() && qr[(x, y)] == QrColor::Dark
            };
            output.push(match (module(x, y), module(x, y + 1)) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            });
        }
        output.push('\n');
    }
    Ok(output)
}

pub(crate) fn render_repository_origin(
    frame: &mut Frame,
    area: Rect,
    dialog: &RepositoryOriginDialog,
    surfaces: &mut FrameSurfaces,
) {
    let mut lines = vec![
        Line::raw(format!("Repository: {}", dialog.repository_id)),
        Line::raw(""),
        Line::raw(format!(
            "The configured source does not contain checkpoint base {}.",
            dialog.missing_commit
        )),
        Line::raw(format!("Checkpoint origin: {}", dialog.archived_origin)),
        Line::raw(format!(
            "Configured source checked: {}",
            dialog.configured_origin
        )),
        Line::raw(""),
        Line::raw("Enter a GitHub origin or absolute local path that contains this history:"),
    ];
    if let Some(error) = &dialog.error {
        lines.push(Line::styled(
            error.clone(),
            Style::default().fg(theme::palette().warning),
        ));
    }
    let body_paragraph = Paragraph::new(lines.clone()).wrap(Wrap { trim: false });
    let popup_height = popup_height(&body_paragraph, 76, 14, area);
    let popup = centered_modal(frame, surfaces, 76, popup_height, area);
    let inner = popup.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 1,
    });
    if inner.height == 0 {
        clear_dialog_form_geometry(&mut dialog.form.borrow_mut());
        return;
    }
    let controls_height = 5;
    let text = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner.height.saturating_sub(controls_height),
    );
    frame.render_widget(body_paragraph, text);
    let field_y = inner.y.saturating_add(text.height);
    frame.render_widget(
        Paragraph::new("Source:"),
        Rect::new(inner.x, field_y, 8.min(inner.width), 1),
    );
    let field_x = inner.x.saturating_add(8.min(inner.width));
    let field = Rect::new(field_x, field_y, inner.width.saturating_sub(8), 1);
    let hint_y = inner.bottom().saturating_sub(3);
    frame.render_widget(
        Paragraph::new("Type or paste into Source · Tab moves · Enter checks")
            .style(Style::default().fg(theme::palette().muted)),
        Rect::new(inner.x, hint_y, inner.width, 1),
    );
    let footer = Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1);
    let mut form = dialog.form.borrow_mut();
    form.begin_frame();
    let title = dismissible_modal_title(
        &mut form,
        popup,
        "Repository history is missing",
        theme::title(true),
        true,
    );
    frame.render_widget(theme::modal().title(title), popup);
    PathField::render(
        frame,
        field,
        &dialog.replacement,
        &mut form,
        DialogControl::Field,
    );
    Dialog::render_actions(
        frame,
        footer,
        &[
            (DialogControl::Cancel, "Cancel", true),
            (DialogControl::Primary, "Check origin", true),
        ],
        &mut form,
    );
    form.end_frame(DialogControl::Field);
}

/// The line that says which session a dialog is about: its name when the
/// caller knows one, else its id.
fn session_line(session_id: &str, session_name: Option<&str>) -> Line<'static> {
    let name = session_name
        .filter(|name| !name.is_empty())
        .unwrap_or(session_id);
    Line::raw(format!("Session: {name}"))
}

/// Title and body of one confirmation, without its buttons.
///
/// Split out so the wording a dialog shows can be asserted without
/// rendering a frame and reading cells back.
pub(crate) fn confirmation_body(
    confirmation: &Confirmation,
    session_name: Option<&str>,
) -> (&'static str, Vec<Line<'static>>) {
    match confirmation {
        Confirmation::RepairRepositoryRemotes { repairs, .. } => (
            "Repair Git tracking?",
            repairs
                .iter()
                .flat_map(|repair| {
                    vec![
                        Line::from(repair.path.display().to_string()),
                        Line::from(format!(
                            "Branch {} tracks missing remote {}.",
                            repair.branch, repair.missing_remote
                        )),
                        Line::from(format!(
                            "Set its tracking remote to {}.",
                            repair.replacement_remote
                        )),
                        Line::from(format!("Fetch: {}", repair.fetch_url)),
                        Line::from(format!("Push: {}", repair.push_urls.join(", "))),
                        Line::from(""),
                    ]
                })
                .collect(),
        ),
        Confirmation::ConfigurationRepair { error, .. } => (
            " Configuration repair ",
            vec![
                Line::raw(error.clone()),
                Line::raw(""),
                Line::raw(
                    "Open settings to restore the named entries. The session and its history are retained.",
                ),
                Line::raw("PgUp/PgDn scroll the full details. Esc dismisses."),
            ],
        ),
        Confirmation::LaunchFailed { error, retry, .. } => {
            let mut lines = vec![
                Line::raw("The session could not start. This message stays until you dismiss it."),
                Line::raw(if retry.is_some() {
                    "Resolve the problem below, then Retry launch with the same settings."
                } else {
                    "Dismiss, resolve the problem below, then select the failed session to retry or remove it."
                }),
                Line::raw("PgUp/PgDn scroll the full details. Esc dismisses."),
                Line::raw(""),
            ];
            if error.contains("Operation not permitted") && error.contains("chmod") {
                lines.push(Line::raw("The container user cannot change the uploaded worker's permissions. If using a custom image, try the standard Mjolnir agent image. Include the diagnostic below when reporting this problem."));
                lines.push(Line::raw(""));
            }
            lines.extend(error.lines().map(|line| Line::raw(line.to_owned())));
            (" Launch failed ", lines)
        }
        Confirmation::Dismiss {
            intent: DismissalIntent::DiscardSetup,
            ..
        } => (
            " Discard Settings changes? ",
            vec![
                Line::raw("Settings has unsaved changes."),
                Line::raw("Keep editing to preserve them, or discard the draft."),
            ],
        ),
        Confirmation::Dismiss {
            intent: DismissalIntent::CancelImport,
            ..
        } => (
            " Cancel import? ",
            vec![
                Line::raw("The active import has not finished."),
                Line::raw("Keep importing, or cancel the operation?"),
            ],
        ),
        Confirmation::ConvertRawCheckout { preview, .. } => {
            let mut lines = vec![Line::raw(preview.summary_line()), Line::raw("")];
            for warning in preview.warning_lines() {
                lines.push(Line::styled(
                    warning,
                    Style::default().fg(theme::palette().warning),
                ));
            }
            (" Move this checkout into the target? ", lines)
        }
        Confirmation::DestroyStopped { session_id, .. } => (
            " Destroy suspended session? ",
            vec![
                session_line(session_id, session_name),
                Line::raw(""),
                Line::raw(
                    "Mjolnir will permanently destroy the recovery archive and session record.",
                ),
                Line::raw(
                    "Any Mjolnir-managed worktree will also be removed. Its git branch stays \
                     in the repository unless you choose to delete it.",
                ),
            ],
        ),
        Confirmation::CloseFailed {
            session_id, error, ..
        } => (
            " Suspension could not complete ",
            vec![
                session_line(session_id, session_name),
                Line::raw(""),
                Line::styled(
                    format!("Suspension failed: {error}"),
                    Style::default().fg(theme::palette().warning),
                ),
            ],
        ),
        Confirmation::InterruptWork {
            session_id,
            restart,
        } => (
            if *restart {
                " Restart while working? "
            } else {
                " Suspend while working? "
            },
            vec![
                session_line(session_id, session_name),
                Line::raw(""),
                Line::styled(
                    "The agent is in the middle of a turn.",
                    Style::default().fg(theme::palette().warning),
                ),
                Line::raw(if *restart {
                    "Restarting ends that turn; the workspace and the conversation so far are kept."
                } else {
                    "Suspending ends that turn, saves a recovery copy, and frees the target."
                }),
            ],
        ),
        Confirmation::SuspendSession {
            session_id,
            active_children,
            interrupting,
        } => {
            let mut lines = vec![
                session_line(session_id, session_name),
                Line::raw("Save a recovery copy and release the environment."),
                Line::raw("You can resume this session later."),
            ];
            if *interrupting {
                lines.push(Line::raw("The current turn will be interrupted."));
            }
            if *active_children > 0 {
                lines.push(Line::raw(format!(
                    "This also suspends {active_children} active sub-agent(s) first."
                )));
            }
            (" Suspend session? ", lines)
        }
        Confirmation::DiscardSinceCheckpoint {
            session_id,
            checkpoint,
        } => (
            " Discard changes since checkpoint? ",
            vec![
                session_line(session_id, session_name),
                Line::raw(format!("Recovery copy: {}", checkpoint.created_at)),
                Line::raw("Release the environment using this older recovery copy."),
                Line::raw("All work since that copy may be permanently lost."),
                Line::raw("The recovery copy remains available for Resume."),
            ],
        ),
        Confirmation::RecoverFailed {
            session_id,
            error,
            recoverable,
        } => {
            let mut lines = vec![session_line(session_id, session_name), Line::raw("")];
            match error {
                Some(error) => lines.push(Line::styled(
                    format!("Failed: {error}"),
                    Style::default().fg(theme::palette().warning),
                )),
                None => lines.push(Line::raw("This session failed without a recorded error.")),
            }
            lines.push(Line::raw(""));
            if *recoverable {
                lines.push(Line::raw(
                    "Recover restores the session onto a fresh target from its recovery copy.",
                ));
                lines.push(Line::raw(
                    "Its transcript is readable either way; opening it changes nothing.",
                ));
            } else {
                lines.push(Line::raw(
                    "There is no verified recovery copy, so this session cannot be resumed.",
                ));
                lines.push(Line::raw("Its transcript is still readable."));
            }
            (" Session failed ", lines)
        }
        Confirmation::RecoverMove { operation } => {
            let mut lines = vec![
                Line::raw(format!("Session: {}", operation.selection.session_id)),
                Line::raw(""),
                Line::styled(
                    format!(
                        "Move {} at destination {} / {}.",
                        match operation.phase {
                            MovePhase::Failed => "failed",
                            MovePhase::Cancelled => "was cancelled",
                            _ => "needs recovery",
                        },
                        operation
                            .selection
                            .profile_id
                            .as_deref()
                            .unwrap_or("current profile"),
                        operation
                            .selection
                            .target_template_id
                            .as_deref()
                            .unwrap_or("current target")
                    ),
                    Style::default().fg(theme::palette().warning),
                ),
            ];
            if let Some(error) = &operation.error {
                lines.push(Line::styled(
                    format!("Error: {error}"),
                    Style::default().fg(theme::palette().warning),
                ));
            }
            lines.push(Line::raw(""));
            if operation.queue_admission_started && !operation.queue_admission_finished {
                lines.push(Line::raw(
                    "Some queued work may already have been accepted; only retry on this exact destination.",
                ));
                lines.push(Line::raw(
                    "Resume with previous settings is unavailable until queue admission finishes.",
                ));
            } else {
                lines.push(Line::raw(
                    "Retry move keeps the failed destination and queue choice.",
                ));
                lines.push(Line::raw(
                    "Resume with previous settings restores the source configuration instead.",
                ));
            }
            if operation.queue == ResumeQueueDisposition::Start {
                lines.push(Line::raw("Queued work was selected to run after the move."));
            } else {
                lines.push(Line::raw("Queued work was selected for discard."));
            }
            (" Move recovery ", lines)
        }
        Confirmation::ForceDestroy { session_id } => (
            " Destroy session? ",
            vec![
                session_line(session_id, session_name),
                Line::raw(""),
                Line::raw(
                    "Permanently destroy this session, its environment, and its recovery archive?",
                ),
                Line::raw(
                    "Its managed branch is kept unless explicitly deleted. Work only in the environment is lost.",
                ),
            ],
        ),
    }
}

pub(crate) fn render_confirmation(
    frame: &mut Frame,
    area: Rect,
    dialog: &ConfirmDialog,
    surfaces: &mut FrameSurfaces,
) {
    let confirmation = &dialog.confirmation;
    // Minimum height per dialog; `popup_height` grows it to fit wrapped content.
    let nominal: u16 = match confirmation {
        Confirmation::ConfigurationRepair { .. }
        | Confirmation::LaunchFailed { .. }
        | Confirmation::RepairRepositoryRemotes { .. } => 16,
        Confirmation::Dismiss { .. } => 8,
        Confirmation::ConvertRawCheckout { .. } => 16,
        Confirmation::CloseFailed { .. } => 12,
        Confirmation::DiscardSinceCheckpoint { .. } => 12,
        Confirmation::SuspendSession { .. } => 10,
        Confirmation::InterruptWork { .. } => 10,
        Confirmation::DestroyStopped { .. } => 10,
        Confirmation::RecoverFailed { .. } => 12,
        Confirmation::RecoverMove { .. } => 14,
        Confirmation::ForceDestroy { .. } => 11,
    };
    let (title, mut lines) = confirmation_body(confirmation, dialog.session_name.as_deref());
    let buttons = confirmation_buttons(confirmation);
    lines.push(Line::raw(""));
    lines.push(confirmation_key_line(buttons));
    lines.push(Line::raw(""));
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let inner_width = crate::widgets::centered_rect(72, 1, area)
        .width
        .saturating_sub(2);
    let buttons_width: usize = buttons
        .iter()
        .map(|label| Line::raw(*label).width() + 5)
        .sum();
    let stacked = buttons_width.saturating_sub(1) > usize::from(inner_width);
    let controls_height = if stacked { buttons.len() as u16 } else { 1 };
    let height = popup_height(&paragraph, 72, nominal, area).saturating_add(controls_height + 2);
    let popup = centered_modal(frame, surfaces, 72, height, area);
    let inner = popup.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 1,
    });
    if inner.height == 0 {
        clear_dialog_form_geometry(&mut dialog.form.borrow_mut());
        return;
    }
    let body = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner.height.saturating_sub(controls_height),
    );
    let max_scroll = u16::try_from(paragraph.line_count(body.width.max(1)))
        .unwrap_or(u16::MAX)
        .saturating_sub(body.height);
    dialog.max_scroll.set(max_scroll);
    frame.render_widget(paragraph.scroll((dialog.scroll.min(max_scroll), 0)), body);
    let mut form = dialog.form.borrow_mut();
    form.begin_frame();
    let title_line = dismissible_modal_title(
        &mut form,
        popup,
        title.trim(),
        Style::default()
            .fg(theme::palette().error)
            .add_modifier(Modifier::BOLD),
        true,
    );
    frame.render_widget(
        theme::modal()
            .border_style(Style::default().fg(theme::palette().error))
            .title_style(
                Style::default()
                    .fg(theme::palette().error)
                    .add_modifier(Modifier::BOLD),
            )
            .title(title_line),
        popup,
    );
    let footer = Rect::new(
        inner.x,
        inner.bottom().saturating_sub(controls_height),
        inner.width,
        controls_height,
    );
    let actions = buttons
        .iter()
        .enumerate()
        .map(|(index, label)| (DialogControl::ConfirmButton(index), *label, true))
        .collect::<Vec<_>>();
    if stacked {
        for (row, action) in actions.iter().enumerate() {
            Dialog::render_actions(
                frame,
                Rect::new(footer.x, footer.y + row as u16, footer.width, 1),
                std::slice::from_ref(action),
                &mut form,
            );
        }
    } else {
        Dialog::render_actions(frame, footer, &actions, &mut form);
    }
    form.end_frame(DialogControl::ConfirmButton(initial_confirmation_button(
        confirmation,
        buttons,
    )));
}
