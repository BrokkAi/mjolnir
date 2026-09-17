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
        Confirmation::DestroyStopped { .. } => &["No", "Yes"],
        Confirmation::CloseFailed { .. } => &["Cancel", "Force stop", "Retry stop"],
        Confirmation::StopWithSubagents { .. } => &["Cancel", "Stop children and parent"],
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
        Confirmation::ForceDestroy { .. } => &["No", "Yes"],
    }
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
            | Confirmation::StopWithSubagents { .. }
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
        &format!("Session: {}", editor.session_id),
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
                    format!(" Testing {target_id}…"),
                    Style::default().fg(theme::palette().accent),
                ),
                Span::styled(" Alt-X cancels test", theme::muted()),
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
            "Starting web viewer…",
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
                "Inspecting listener…",
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

/// Title and body of one confirmation, without its buttons.
///
/// Split out so the wording a dialog shows can be asserted without
/// rendering a frame and reading cells back.
pub(crate) fn confirmation_body(confirmation: &Confirmation) -> (&'static str, Vec<Line<'static>>) {
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
            " Permanently destroy stopped session? ",
            vec![
                Line::raw(format!("Session: {session_id}")),
                Line::raw(""),
                Line::raw(
                    "Mjolnir will permanently destroy the recovery archive and session record.",
                ),
                Line::raw(
                    "Any Mjolnir-managed worktree and generated branch will also be removed.",
                ),
            ],
        ),
        Confirmation::CloseFailed { session_id, error } => (
            " Stop could not complete ",
            vec![
                Line::raw(format!("Session: {session_id}")),
                Line::raw(""),
                Line::styled(
                    format!("Stop failed: {error}"),
                    Style::default().fg(theme::palette().warning),
                ),
            ],
        ),
        Confirmation::StopWithSubagents { session_id, count } => (
            " Stop parent and sub-agents? ",
            vec![
                Line::raw(format!("Session: {session_id}")),
                Line::raw(""),
                Line::styled(
                    format!("This session has {count} active sub-agent(s)."),
                    Style::default().fg(theme::palette().warning),
                ),
                Line::raw("Mjolnir will stop the children first, then save and stop the parent."),
            ],
        ),
        Confirmation::RecoverFailed {
            session_id,
            error,
            recoverable,
        } => {
            let mut lines = vec![Line::raw(format!("Session: {session_id}")), Line::raw("")];
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
            " Delete session? ",
            vec![
                Line::raw(format!("Session: {session_id}")),
                Line::raw(""),
                Line::raw("Delete this session, its worktree, and its recovery archive?"),
                Line::raw("Y: Yes    N / Esc: No"),
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
        Confirmation::StopWithSubagents { .. } => 10,
        Confirmation::DestroyStopped { .. } => 10,
        Confirmation::RecoverFailed { .. } => 12,
        Confirmation::RecoverMove { .. } => 14,
        Confirmation::ForceDestroy { .. } => 11,
    };
    let (title, mut lines) = confirmation_body(confirmation);
    let buttons = confirmation_buttons(confirmation);
    lines.push(Line::raw(""));
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let extra = 1;
    let height = popup_height(&paragraph, 72, nominal.saturating_add(extra), area);
    let popup = centered_modal(frame, surfaces, 72, height, area);
    let inner = popup.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 1,
    });
    if inner.height == 0 {
        clear_dialog_form_geometry(&mut dialog.form.borrow_mut());
        return;
    }
    let controls_height = 1;
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
    let footer = Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1);
    Dialog::render_actions(
        frame,
        footer,
        &buttons
            .iter()
            .enumerate()
            .map(|(index, label)| (DialogControl::ConfirmButton(index), *label, true))
            .collect::<Vec<_>>(),
        &mut form,
    );
    form.end_frame(DialogControl::ConfirmButton(initial_confirmation_button(
        confirmation,
        buttons,
    )));
}
