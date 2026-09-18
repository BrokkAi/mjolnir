use super::*;

/// Draws a chat across a whole frame, the way the combined surface lays it out
/// when nothing else is competing for the rows.
///
/// Only tests use this: the real surface owns the layout and calls
/// [`render_in`] with the bands it chose.
#[cfg(test)]
pub(crate) fn render_full_frame(
    frame: &mut Frame,
    chat: &mut ChatState,
    transcript_selected: bool,
) {
    let inner = frame.area();
    let prompt_width = prompt_content_width(inner.width);
    let visible_queued = chat.queued_prompts.len().min(3) as u16;
    let input_rows = crate::chat::input::input_visual_rows(&chat.input, prompt_width) as u16;
    let prompt_height = input_rows
        .saturating_add(visible_queued)
        .saturating_add(2)
        .max(4)
        .min(inner.height.saturating_sub(6).max(3));
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(5),
            Constraint::Length(prompt_height),
            Constraint::Length(1),
        ])
        .split(inner);
    render_in(
        frame,
        chat,
        ChatRegions {
            transcript: chunks[0],
            prompt: chunks[1],
            footer: Some(test_footer(chunks[2])),
            overlay: inner,
            title_controls: 0,
            pane_focused: false,
        },
        true,
        transcript_selected,
    );
}

/// Draws the transcript and the composer into `regions`.
///
/// `prompt_focused` says whether the composer owns the keyboard; only then
/// does it draw a cursor and an accent border. `transcript_selected` says the
/// selection engine still owns a selection on the transcript, so its row
/// space has to stay frozen for this frame.
pub(crate) fn render_in(
    frame: &mut Frame,
    chat: &mut ChatState,
    regions: ChatRegions<'_>,
    prompt_focused: bool,
    transcript_selected: bool,
) {
    chat.footer_command_areas.borrow_mut().clear();
    chat.voice_form.begin_frame();
    if chat.component_modal_open() || chat.second_opinion_split() || chat.turn_review_split() {
        chat.voice_form.cancel_pointer();
    }
    chat.frame_surfaces.clear();
    chat.frame_surfaces_exclusive = false;
    // Modals and the completion popup are centred in the whole frame, not
    // in the band the transcript happens to have been given.
    let inner = regions.overlay;
    let mut transcript_area = regions.transcript;
    let prompt_area = regions.prompt;

    // An open question replaces the composer, but only with the natural
    // height of its current page (up to half of the combined conversation
    // bands). The transcript is rendered into the rows left above it so its
    // viewport, scrollbar, and selection row space describe what is visible.
    // This branch deliberately runs before the ordinary prompt/split drawing:
    // no hidden prompt or autocomplete surface may survive underneath the
    // question, and both visible scrollable surfaces remain registered.
    if let Some(question_height) = chat.elicitation.as_ref().map(|dialog| {
        let combined = Rect::new(
            transcript_area.x,
            transcript_area.y,
            transcript_area.width,
            prompt_area.bottom().saturating_sub(transcript_area.y),
        );
        dialog
            .natural_height(combined.width)
            .min(combined.height / 2)
    }) {
        let combined = Rect::new(
            transcript_area.x,
            transcript_area.y,
            transcript_area.width,
            prompt_area.bottom().saturating_sub(transcript_area.y),
        );
        let transcript_height = combined.height.saturating_sub(question_height);
        let question_area = Rect::new(
            combined.x,
            combined.bottom().saturating_sub(question_height),
            combined.width,
            question_height,
        );
        let upper_transcript = Rect::new(combined.x, combined.y, combined.width, transcript_height);

        chat.voice_button_area = None;
        chat.config_chip_areas.clear();
        chat.task_control_area = None;
        chat.subagent_control_area = None;
        chat.task_dialog_area = None;
        chat.reviewer_area = None;
        chat.split_action_areas.clear();
        chat.turn_review_action_areas.clear();
        chat.voice_form.cancel_pointer();
        chat.voice_form
            .end_frame(crate::chat::VoiceControl::Microphone);

        render_transcript(
            frame,
            upper_transcript,
            chat,
            transcript_selected,
            regions.title_controls,
            regions.pane_focused,
        );
        if question_height > 0
            && let Some(dialog) = chat.elicitation.as_ref()
        {
            render_elicitation_in(
                frame,
                dialog,
                &mut chat.frame_surfaces,
                question_area,
                prompt_focused,
            );
        }
        if let Some(footer) = regions.footer {
            render_chat_footer(frame, footer, chat, prompt_focused);
        }
        return;
    }

    let split = chat.second_opinion_split() || chat.turn_review_split();
    // Review panes replace the composer, so retain their existing activity
    // row. Normal conversations put the spinner in the composer border.
    let activity_area =
        (split && chat.needs_animation() && transcript_area.height > 3).then(|| {
            transcript_area.height -= 1;
            Rect::new(
                transcript_area.x,
                transcript_area.bottom(),
                transcript_area.width,
                1,
            )
        });
    // The button lives on the prompt border, so it is not part of
    // the selectable prompt interior. Clear the hitbox first because a split
    // view or modal may replace the composer for this frame.
    chat.voice_button_area = None;
    chat.config_chip_areas.clear();
    chat.task_control_area = None;
    chat.subagent_control_area = None;
    chat.task_dialog_area = None;
    let (primary_area, reviewer_area) = if split {
        let halves = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(transcript_area);
        (halves[0], Some(halves[1]))
    } else {
        (transcript_area, None)
    };
    render_transcript(
        frame,
        primary_area,
        chat,
        transcript_selected,
        regions.title_controls,
        regions.pane_focused,
    );
    chat.reviewer_area = None;
    if let Some(area) = reviewer_area {
        if chat.turn_review_split() {
            if let Some(review) = chat.turn_review_mut() {
                let (inner, top, total) =
                    crate::chat::turn_review::render_turn_review_pane(frame, area, review);
                // Route the wheel to whichever review tab is selected.
                chat.reviewer_area = Some(area);
                chat.frame_surfaces.push(SurfaceFrame::scrollable(
                    SurfaceId::ReviewerTranscript,
                    inner,
                    top,
                    total,
                ));
            }
        } else {
            let status = match chat.second_opinion() {
                Some(SecondOpinion::Review(review)) => review.status.clone(),
                _ => String::new(),
            };
            if let Some(SecondOpinion::Review(review)) = chat.second_opinion_mut() {
                let (inner, top, total) =
                    render_reviewer(frame, area, &mut review.reviewer, &status);
                chat.reviewer_area = Some(inner);
                chat.frame_surfaces.push(SurfaceFrame::scrollable(
                    SurfaceId::ReviewerTranscript,
                    inner,
                    top,
                    total,
                ));
            }
        }
    }
    if chat.turn_review_split() {
        // The split has no composer: a review is synchronous, so the only
        // input while it is up is which of its actions to take.
        let status = chat
            .turn_review()
            .map(crate::chat::turn_review::TurnReview::status)
            .unwrap_or_default();
        let buttons = match chat.turn_review_mut() {
            Some(review) => crate::chat::turn_review::render_turn_review_actions(
                frame,
                prompt_area,
                review,
                &status,
            ),
            None => Vec::new(),
        };
        chat.turn_review_action_areas = buttons;
        chat.split_action_areas.clear();
    } else if let Some(SecondOpinion::Review(review)) = chat.second_opinion_mut() {
        // The split has no composer: the revised plan is the planner's to
        // write, so the only input here is which of the three actions to take.
        let buttons = render_split_actions(
            frame,
            prompt_area,
            &review.workflow,
            review.action,
            &review.status,
            &mut review.form,
        );
        chat.split_action_areas = buttons;
        chat.turn_review_action_areas.clear();
    } else {
        chat.split_action_areas.clear();
        chat.turn_review_action_areas.clear();
        render_composer_band(frame, prompt_area, chat, prompt_focused, None);
    }
    if let Some(area) = activity_area {
        let mut activity = if area.width >= 48 {
            chat.activity_spinner()
        } else {
            Line::from(crate::spinner::compact_span(
                chat.spinner_style,
                crate::spinner::elapsed_ms(),
            ))
        };
        activity.spans.insert(0, Span::raw("  "));
        let status = chat
            .turn_review()
            .and_then(|review| review.view.activity_label())
            .map_or_else(|| prompt_title(chat), |label| format!(" {label} "));
        activity.spans.push(Span::styled(status, theme::muted()));
        frame.render_widget(
            Paragraph::new(truncate_line_to_width(activity, usize::from(area.width)))
                .style(theme::base()),
            area,
        );
    }
    if let Some(footer) = regions.footer {
        render_chat_footer(frame, footer, chat, prompt_focused);
    }
    // The popup overlays the prompt and whatever sits above it, so it
    // registers last and wins the cells it covers.
    if let Some(popup) = render_autocomplete(frame, prompt_area, chat) {
        chat.frame_surfaces
            .push(SurfaceFrame::fixed(SurfaceId::AutocompletePopup, popup));
    }
    // A new elicitation can arrive while the task list is open.
    // Keep the task dialog state so it can reappear afterwards, but let the
    // question render and receive input on top of it.
    if chat.task_dialog_open() && chat.elicitation.is_none() {
        render_background_task_dialog(frame, inner, chat);
        return;
    }
    if let Some(body) = crate::chat::config_picker::render_config_picker(frame, inner, chat) {
        // The selector owns the frame's interaction, so the chat behind it
        // stops being selectable. An elicitation dialog still draws over it,
        // matching the key routing that lets the dialog win.
        chat.frame_surfaces.clear();
        chat.frame_surfaces
            .push(SurfaceFrame::fixed(SurfaceId::ModalBody, body));
    }
    if let Some(SecondOpinion::Setup {
        captured,
        setup,
        form,
    }) = chat.second_opinion_mut()
    {
        // The waterfall owns the frame's interaction, so the chat behind it
        // stops being selectable while a reviewer is being chosen.
        let area = crate::modal::centered_modal_rect_fixed(frame, 60, 16, inner);
        let body = render_setup(
            frame,
            area,
            &format!(
                "Reviewing a {}-line plan",
                captured.proposal.lines().count()
            ),
            setup,
            form,
        );
        chat.frame_surfaces.clear();
        chat.frame_surfaces
            .push(SurfaceFrame::fixed(SurfaceId::ModalBody, body));
    }
}

/// Draws the one-row footer under the conversation: the reverse-i-search
/// prompt when one is open, else the shared notice, else the hotkey hints
/// for the composer.
pub(crate) fn render_chat_footer(
    frame: &mut Frame,
    footer: ChatFooter<'_>,
    chat: &ChatState,
    prompt_focused: bool,
) {
    // The host only hands the footer to the chat while the composer has
    // focus, so `prompt_focused` is normally true here; the other arm keeps
    // the row honest if it ever is not.
    // The three groups are the dashboard's: what the composer answers, the
    // chords that answer from anywhere, then the function keys. Only the
    // first group changes with what the composer is doing.
    let footer_area = footer.area;
    // A host banner owns the whole row: it is reporting a state the composer's
    // own hints would only bury.
    if let Some(banner) = footer.banner {
        chat.footer_command_areas.borrow_mut().clear();
        frame.render_widget(
            Paragraph::new(banner.clone()).style(theme::base().fg(theme::palette().muted)),
            footer_area,
        );
        return;
    }
    let queued_keys = format!(
        "Up/Ctrl-P edit last queued · Enter send/queue · Ctrl-R history · Shift-Enter newline · {}",
        chat.turn_control_intent().escape_hint(),
    );
    let composer_keys = if !prompt_focused {
        "Tab pane · PgUp/PgDn transcript"
    } else if chat.voice_active {
        "Listening… click the microphone to stop · PgUp/PgDn transcript"
    } else if !chat.queued_prompts.is_empty() {
        &queued_keys
    } else {
        "Tab pane · Ctrl-V paste · Enter send · Ctrl-R history · Shift-Enter newline"
    };
    let groups = theme::fit_footer_items(
        [
            composer_keys
                .split(theme::footer_separator())
                .map(|text| (None, text))
                .collect(),
            footer
                .chords
                .iter()
                .enumerate()
                .map(|(index, text)| (Some(index), *text))
                .collect(),
            footer
                .functions
                .iter()
                .enumerate()
                .map(|(index, text)| (Some(footer.chords.len() + index), *text))
                .collect(),
        ],
        footer_area.width,
        |(_, text)| *text,
        // The host ranks its hints, and puts the two it wants kept longest —
        // the palette and the help key — at the end of the list.
        |(command, _)| {
            command.is_some_and(|index| {
                index + 2 >= footer.chords.len().saturating_add(footer.functions.len())
            })
        },
    );
    let default_footer = theme::footer_items_text(&groups, |(_, text)| *text);
    let search_footer = chat.history_search.as_ref().map(history_search_footer);
    let notice = chat.notices.current();
    let footer = search_footer
        .as_deref()
        .or(notice.as_deref())
        .unwrap_or(&default_footer);
    let mut command_areas = chat.footer_command_areas.borrow_mut();
    command_areas.clear();
    if search_footer.is_none() && notice.is_none() {
        let mut x = footer_area.x;
        for group in groups.iter().filter(|group| !group.is_empty()) {
            if x > footer_area.x {
                x += display_width(theme::footer_group_separator()) as u16;
            }
            for (index, (command, text)) in group.iter().enumerate() {
                if index > 0 {
                    x += display_width(theme::footer_separator()) as u16;
                }
                let width = display_width(text) as u16;
                if let Some(command) = command {
                    command_areas.push((
                        *command,
                        Rect::new(x, footer_area.y, width, footer_area.height),
                    ));
                }
                x += width;
            }
        }
    }
    // Notices keep a warm accent; navigation hints remain quiet.
    let footer_color = if search_footer.is_none() && notice.is_some() {
        theme::palette().warning
    } else {
        theme::palette().muted
    };
    let line = if search_footer.is_none() && notice.is_none() {
        theme::hints(footer)
    } else {
        Line::raw(footer)
    };
    frame.render_widget(
        Paragraph::new(line).style(theme::base().fg(footer_color)),
        footer_area,
    );
    if let Some(search) = chat.history_search.as_ref()
        && chat.elicitation.is_none()
        && footer_area.width > 0
    {
        let prefix = format!("reverse-i-search [{}]: ", history_scope_name(search.scope));
        let column = display_width(&prefix) + display_width(&search.query);
        frame.set_cursor_position((
            footer_area.x + column.min(usize::from(footer_area.width.saturating_sub(1))) as u16,
            footer_area.y,
        ));
    }
}

#[cfg(test)]
pub(crate) fn test_footer(area: Rect) -> ChatFooter<'static> {
    ChatFooter {
        area,
        chords: &["ctrl+b then: b panes", "q detach", ": palette", "? keys"],
        functions: &[],
        banner: None,
    }
}

/// A remembered configuration value, or `None` when it stands for the
/// harness's own default and nothing should be applied.
pub(crate) fn remembered_value(stored: Option<&str>) -> Option<String> {
    stored
        .filter(|value| *value != mj_core::second_opinion::HARNESS_DEFAULT_VALUE)
        .map(str::to_owned)
}

/// Draws the composer band: the title, borders, queued-prompt previews,
/// input, and cursor. The full chat render calls this for its ordinary
/// prompt, and hosts call it through
/// [`ChatState::draw_prompt_band`] to show the real composer while a session
/// is not attached.
///
/// `note` adds a left-aligned line to the bottom border — the standby
/// prompt's cancel chord; the ordinary render passes `None`.
pub(crate) fn render_composer_band(
    frame: &mut Frame,
    prompt_area: Rect,
    chat: &mut ChatState,
    prompt_focused: bool,
    note: Option<Line<'static>>,
) {
    let prompt_width = prompt_content_width(prompt_area.width);
    let (prompt_title, activity_title, config_chips) = prompt_title_line(chat, prompt_area);
    chat.config_chip_areas = config_chips;
    let mut prompt_block = theme::panel(prompt_focused)
        .padding(Padding::new(2, 1, 0, 0))
        .title(prompt_title);
    if let Some(activity_title) = activity_title {
        prompt_block = prompt_block.title(activity_title.right_aligned());
    }
    chat.task_control_area = None;
    chat.subagent_control_area = None;
    let bottom_width = prompt_area.width.saturating_sub(2);
    let queue_control = prompt_bottom_queue_control(chat);
    let task_label = (chat.background_task_count() > 0)
        .then(|| format!(" View tasks ({}) ", chat.background_task_count()));
    let subagent_label =
        (chat.subagent_count() > 0).then(|| format!(" Sub-agents ({}) ", chat.subagent_count()));
    let command_hints = (prompt_focused && prompt_area.width >= 56).then(|| {
        // A standby composer cannot send, so the hint says what Enter does
        // there instead of advertising a send that would be refused.
        if chat.standby {
            Line::from(vec![
                Span::styled(" Enter ", theme::key_hint()),
                Span::styled(" keeps the draft ", theme::hint_description()),
            ])
            .right_aligned()
        } else {
            Line::from(vec![
                Span::styled(" Enter ", theme::key_hint()),
                Span::styled(" send  ", theme::hint_description()),
                Span::styled(" / ", theme::key_hint()),
                Span::styled(" commands ", theme::hint_description()),
            ])
            .right_aligned()
        }
    });
    let queue_width = queue_control.as_ref().map_or(0, Line::width);
    let task_width = task_label.as_ref().map_or(0, |label| display_width(label));
    let subagent_width = subagent_label
        .as_ref()
        .map_or(0, |label| display_width(label));
    let command_width = command_hints.as_ref().map_or(0, Line::width);
    let task_separator_width = usize::from(queue_control.is_some() && task_label.is_some()) * 2;
    // Fit queue/control text first, then a complete task button, then hints.
    let left_with_task = queue_width + task_separator_width + task_width;
    let show_task = task_label.is_some() && left_with_task <= usize::from(bottom_width);
    let mut left_width = if show_task {
        left_with_task
    } else {
        queue_width
    };
    let subagent_separator_width = usize::from(left_width > 0) * 2;
    let show_subagents = subagent_label.is_some()
        && left_width + subagent_separator_width + subagent_width <= usize::from(bottom_width);
    if show_subagents {
        left_width += subagent_separator_width + subagent_width;
    }
    let show_command_hints = command_hints.is_some()
        && left_width + usize::from(left_width > 0) + command_width <= usize::from(bottom_width);
    let mut bottom_spans = Vec::new();
    let mut bottom_left_width = 0usize;
    if let Some(queue_control) = queue_control {
        bottom_left_width = queue_width;
        bottom_spans.extend(queue_control.spans);
    }
    if show_task {
        let task_start = bottom_left_width + task_separator_width;
        if bottom_left_width > 0 {
            bottom_spans.push(Span::raw(" ·"));
        }
        let task_label = task_label.expect("show_task implies a task label");
        let task_width = u16::try_from(task_width).expect("task label fits in u16");
        chat.task_control_area = Some(Rect::new(
            prompt_area
                .x
                .saturating_add(1)
                .saturating_add(u16::try_from(task_start).unwrap_or(u16::MAX)),
            prompt_area.bottom().saturating_sub(1),
            task_width,
            1,
        ));
        bottom_spans.push(Span::styled(
            task_label,
            if chat.task_control_focused() {
                theme::selection(true)
            } else {
                // The chip is always clickable while its work is running, so
                // keep its blue highlight; focus only adds accent foreground.
                theme::selection(false)
            },
        ));
        bottom_left_width = task_start + usize::from(task_width);
    }
    if show_subagents {
        let separator_width = usize::from(bottom_left_width > 0) * 2;
        let subagent_start = bottom_left_width + separator_width;
        if bottom_left_width > 0 {
            bottom_spans.push(Span::raw(" ·"));
        }
        let label = subagent_label.expect("show_subagents implies a label");
        let width = u16::try_from(subagent_width).expect("subagent label fits in u16");
        chat.subagent_control_area = Some(Rect::new(
            prompt_area
                .x
                .saturating_add(1)
                .saturating_add(u16::try_from(subagent_start).unwrap_or(u16::MAX)),
            prompt_area.bottom().saturating_sub(1),
            width,
            1,
        ));
        bottom_spans.push(Span::styled(
            label,
            if chat.subagent_control_focused() {
                theme::selection(true)
            } else {
                // The chip is always clickable, so keep its blue highlight;
                // keyboard focus only adds the accent foreground.
                theme::selection(false)
            },
        ));
    }
    if !bottom_spans.is_empty() {
        prompt_block = prompt_block.title_bottom(Line::from(bottom_spans).left_aligned());
    }
    if show_command_hints {
        prompt_block = prompt_block.title_bottom(command_hints.expect("presence checked"));
    }
    if let Some(note) = note {
        prompt_block = prompt_block.title_bottom(note);
    }
    let prompt_inner = prompt_block.inner(prompt_area);
    chat.prompt_content_width = prompt_width;
    chat.voice_button_area = voice_button_area(prompt_area);
    let mut prompt_lines = chat
        .queued_prompts
        .iter()
        .rev()
        .take(3)
        .rev()
        .enumerate()
        .map(|(index, queued)| {
            Line::from(Span::styled(
                truncate_to_width(
                    &format!(
                        "{} {}: {}",
                        queued.queue_label(),
                        index + 1,
                        queued_prompt_preview(&queued.text)
                    ),
                    usize::from(prompt_inner.width),
                ),
                Style::default().fg(theme::palette().muted),
            ))
        })
        .collect::<Vec<_>>();
    let queue_rows = prompt_lines.len();
    prompt_lines.extend(if let Some(search) = chat.history_search.as_ref() {
        highlighted_input_lines(&chat.input, &search.query)
    } else if chat.input.is_empty() {
        vec![Line::from(Span::styled(
            if chat.standby {
                "Type a draft · sending opens when the session is live"
            } else if chat.phase == WorkerPhase::Running {
                "Add a follow-up while the agent works…"
            } else if chat.entries.is_empty()
                && chat.unconverted_prefix == 0
                && !chat.transcript_loading
            {
                "What would you like to build?"
            } else {
                ""
            },
            theme::muted(),
        ))]
    } else {
        chat.input
            .split('\n')
            .map(|line| Line::raw(line.to_owned()))
            .collect()
    });
    let cursor_row =
        input_cursor_visual_position(&chat.input, chat.input_cursor, prompt_width).1 + queue_rows;
    let content_height = usize::from(prompt_inner.height).max(1);
    let input_scroll = cursor_row.saturating_add(1).saturating_sub(content_height);
    frame.render_widget(
        Paragraph::new(prompt_lines)
            .style(Style::default().fg(theme::palette().text))
            .wrap(Wrap { trim: false })
            .scroll((input_scroll as u16, 0))
            .block(prompt_block),
        prompt_area,
    );
    if let Some(marker_row) = queue_rows.checked_sub(input_scroll)
        && marker_row < usize::from(prompt_inner.height)
    {
        frame.render_widget(
            Line::styled(">", theme::title(prompt_focused)),
            Rect::new(
                prompt_inner.x.saturating_sub(2),
                prompt_inner.y.saturating_add(marker_row as u16),
                1,
                1,
            ),
        );
    }
    if let Some(button_area) = chat.voice_button_area {
        chat.voice_form.register(
            crate::chat::VoiceControl::Microphone,
            ControlKind::Button,
            button_area,
            chat.voice_available || chat.voice_active,
        );
        frame.render_widget(
            voice_button_line(chat.voice_available, chat.voice_active),
            button_area,
        );
    }
    chat.voice_form
        .end_frame(crate::chat::VoiceControl::Microphone);
    chat.frame_surfaces
        .push(SurfaceFrame::fixed(SurfaceId::PromptInput, prompt_inner));
    // The cursor belongs to whatever has focus, so the composer only shows one
    // while the keyboard is driving it.
    if chat.history_search.is_none() && prompt_focused && chat.elicitation.is_none() {
        set_input_cursor(
            frame,
            prompt_inner,
            &chat.input,
            chat.input_cursor,
            queue_rows,
            input_scroll,
        );
    }
}

pub(crate) fn prompt_title(chat: &ChatState) -> String {
    let title = prompt_title_parts(chat).join(" · ");
    if title.is_empty() {
        String::new()
    } else {
        format!(" {title} ")
    }
}

pub(crate) fn prompt_title_parts(chat: &ChatState) -> Vec<String> {
    let mut parts = [chat.current_model(), chat.current_effort()]
        .into_iter()
        .flatten()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if chat.fast_mode_active() {
        parts.push("Fast".into());
    }
    if chat.plan_mode_active() {
        parts.push("PLAN MODE".into());
    } else {
        match chat.phase {
            WorkerPhase::Idle => {}
            WorkerPhase::Running if chat.pursuing_goal() => parts.push("Pursuing goal".into()),
            // The spinner already indicates an ordinary running turn.
            WorkerPhase::Running => {}
            WorkerPhase::Closing => parts.push("Closing".into()),
            WorkerPhase::Closed => parts.push("Closed".into()),
        }
    }
    // Auto-review changes what happens when this turn ends, so the composer
    // says it is armed rather than surprising the user with a pane.
    let review = chat.review_config();
    if review.enabled && review.reviewer_profile().is_some() {
        parts.push(format!("review {}", review.tier.label()));
    }
    parts
}

/// The microphone chip owns the prompt border's upper-left corner, followed by
/// model, effort, and state. Activity owns the upper-right corner. A full
/// configured spinner is used when both titles fit; the one-column frame keeps
/// narrow prompts readable. The model and effort spans are returned as
/// clickable chips that open their value selector.
pub(crate) fn prompt_title_line(
    chat: &ChatState,
    prompt_area: Rect,
) -> (
    Line<'static>,
    Option<Line<'static>>,
    Vec<(&'static str, Rect)>,
) {
    let parts = prompt_title_parts(chat);
    let model = chat.current_model();
    let effort = chat.current_effort();
    let prefix_count = usize::from(model.is_some()) + usize::from(effort.is_some());
    let suffix = parts[prefix_count.min(parts.len())..].join(" · ");
    let mut spans = vec![Span::raw(format!(" {VOICE_BUTTON_GLYPH} "))];
    // The title begins just inside the border corner; each chip keeps the exact
    // cells of its span, and only a chip that fits inside the border is kept.
    let mut chip_x = usize::from(prompt_area.x.saturating_add(1)) + spans[0].width();
    let chip_limit = usize::from(prompt_area.right().saturating_sub(1));
    let mut chips = Vec::new();
    if let Some(model) = model {
        let text = format!(" {model} ");
        let width = display_width(&text);
        if chip_x.saturating_add(width) <= chip_limit {
            chips.push((
                "model",
                Rect::new(
                    u16::try_from(chip_x).unwrap_or(u16::MAX),
                    prompt_area.y,
                    u16::try_from(width).unwrap_or(u16::MAX),
                    1,
                ),
            ));
        }
        spans.push(Span::styled(text, theme::selection(false)));
        chip_x = chip_x.saturating_add(width);
    }
    if let Some(effort) = effort {
        let separator_width = usize::from(model.is_some()) * display_width("· ");
        let text = if model.is_some() {
            format!("· {effort} ")
        } else {
            format!(" {effort} ")
        };
        let width = display_width(&text);
        let value_width = width.saturating_sub(separator_width);
        let value_x = chip_x.saturating_add(separator_width);
        // The dot is visual punctuation between independent controls, so
        // neither its color nor its hitbox advertises it as clickable.
        if model.is_some() {
            spans.push(Span::styled("· ", theme::muted()));
        }
        if chip_x.saturating_add(width) <= chip_limit {
            chips.push((
                "effort",
                Rect::new(
                    u16::try_from(value_x).unwrap_or(u16::MAX),
                    prompt_area.y,
                    u16::try_from(value_width).unwrap_or(u16::MAX),
                    1,
                ),
            ));
        }
        spans.push(Span::styled(
            if model.is_some() {
                format!("{effort} ")
            } else {
                text
            },
            theme::selection(false),
        ));
    }
    if !suffix.is_empty() {
        spans.push(Span::raw(format!(" {suffix} ")));
    }
    let left_width = spans.iter().map(Span::width).sum::<usize>();
    let activity_title = chat.needs_animation().then(|| {
        let full = chat.activity_spinner();
        let max_title_width = usize::from(prompt_area.width.saturating_sub(2));
        let spinner =
            if left_width.saturating_add(full.width()).saturating_add(3) <= max_title_width {
                full
            } else {
                Line::from(crate::spinner::compact_span(
                    chat.spinner_style,
                    crate::spinner::elapsed_ms(),
                ))
            };
        let mut title = vec![Span::raw(" ")];
        title.extend(spinner.spans);
        title.push(Span::raw(" "));
        Line::from(title)
    });
    (Line::from(spans), activity_title, chips)
}

/// Queue state and the hint for controlling the current turn live together
/// on the prompt's bottom border. This stays independent of background tasks,
/// so a queued prompt remains visible even when there is no task button.
pub(crate) fn prompt_bottom_queue_control(chat: &ChatState) -> Option<Line<'static>> {
    let mut labels = Vec::new();
    if !chat.queued_prompts.is_empty() {
        labels.push(format!("{} queued", chat.queued_prompts.len()));
    }
    if chat.prompt_in_flight() || chat.session_activity.capacity_retry.is_some() {
        labels.push(chat.turn_control_intent().escape_hint().to_owned());
    }
    (!labels.is_empty()).then(|| {
        Line::from(Span::styled(
            format!(" {}", labels.join(" · ")),
            theme::muted(),
        ))
    })
}

pub(crate) fn render_background_task_dialog(frame: &mut Frame, area: Rect, chat: &mut ChatState) {
    let commands = chat.session_activity().background_commands.clone();
    let height = u16::try_from(commands.len().saturating_add(4))
        .unwrap_or(u16::MAX)
        .max(1)
        .min(area.height.max(1));
    let width = area
        .width
        .saturating_sub(4)
        .clamp(32, 96)
        .min(area.width.max(1));
    let popup = crate::modal::centered_modal_rect_fixed(frame, width, height, area);
    let inner = popup.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 1,
    });
    let now = mj_core::clock::epoch_seconds();
    let content_width = usize::from(inner.width.max(1));
    let mut stop_rows = Vec::new();
    let mut lines = Vec::new();
    if commands.is_empty() {
        lines.push(Line::from(Span::styled(
            "No background tasks remain.",
            theme::muted(),
        )));
    } else {
        for (index, command) in commands.iter().enumerate() {
            let started = u64::try_from(command.started_at_ms.max(0) / 1_000).unwrap_or_default();
            let elapsed = mj_client::usage_format::format_clock(now.saturating_sub(started));
            let prefix = format!("{elapsed:>8}  ");
            let prefix_width = display_width(&prefix);
            let label = if chat.background_stop_pending(&command.id) {
                "Stopping…"
            } else {
                "[Stop]"
            };
            let button_width = display_width(&format!("  {label}  "));
            // Keep the acknowledgement state visible even if the provider
            // revokes the affordance before its task-disappeared snapshot.
            let draw_button = (command.can_stop || chat.background_stop_pending(&command.id))
                && content_width >= button_width.saturating_add(prefix_width).saturating_add(2);
            let text_width = if draw_button {
                content_width
                    .saturating_sub(button_width.saturating_add(1))
                    .max(1)
            } else {
                content_width
            };
            let start = lines.len();
            lines.extend(wrap_styled_line(
                Line::from(format!(
                    "{prefix}{}",
                    crate::chat::rendering::sanitize_terminal_text(&command.command)
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ")
                )),
                text_width,
                prefix_width,
            ));
            if draw_button {
                stop_rows.push((
                    BackgroundTaskControl::Stop(index),
                    start,
                    label,
                    !chat.background_stop_pending(&command.id),
                    button_width,
                    command.id.clone(),
                ));
            }
        }
    }
    let visible = usize::from(inner.height).max(1);
    let total_lines = lines.len();
    let max_scroll = total_lines.saturating_sub(visible);
    chat.task_dialog_max_scroll = max_scroll;
    chat.task_dialog_scroll = chat.task_dialog_scroll.min(max_scroll);
    if chat.task_dialog_scroll > 0 {
        lines = lines.into_iter().skip(chat.task_dialog_scroll).collect();
    }
    lines.truncate(visible);
    chat.task_dialog_form.begin_frame();
    let title = crate::modal::dismissible_modal_title(
        &mut chat.task_dialog_form,
        popup,
        "Background tasks",
        theme::title(true),
        true,
    );
    let block = theme::panel(true)
        .title(title)
        .title_bottom(Line::from(Span::styled(" Esc close ", theme::muted())).right_aligned());
    frame.render_widget(
        Paragraph::new(lines)
            .style(theme::base().fg(theme::palette().text))
            .wrap(Wrap { trim: false })
            .block(block),
        popup,
    );
    chat.task_dialog_control_ids.clear();
    for (control, row, label, enabled, button_width, id) in stop_rows {
        if row < chat.task_dialog_scroll || row >= chat.task_dialog_scroll.saturating_add(visible) {
            continue;
        }
        chat.task_dialog_control_ids.push((control, id));
        let y = inner
            .y
            .saturating_add((row - chat.task_dialog_scroll) as u16);
        let button_area = Rect::new(
            inner
                .right()
                .saturating_sub(u16::try_from(button_width).unwrap_or(u16::MAX)),
            y,
            u16::try_from(button_width)
                .unwrap_or(u16::MAX)
                .min(inner.width),
            1,
        );
        Button::render(
            frame,
            button_area,
            label,
            enabled,
            &mut chat.task_dialog_form,
            control,
        );
    }
    if let Some(geometry) = scrollbar_geometry(
        Rect::new(inner.right(), inner.y, 1, inner.height),
        total_lines,
        chat.task_dialog_scroll,
        visible,
    ) {
        render_scrollbar(frame, geometry);
    }
    chat.task_dialog_area = Some(inner);
    chat.frame_surfaces_exclusive = true;
    chat.frame_surfaces.clear();
    chat.frame_surfaces
        .push(SurfaceFrame::fixed(SurfaceId::ModalBody, inner));
    chat.task_dialog_form
        .end_frame(BackgroundTaskControl::Stop(0));
}

/// The composer keeps a `>` gutter on the left and one cell of space on the
/// right.
pub(crate) fn prompt_content_width(width: u16) -> usize {
    usize::from(width.saturating_sub(5)).max(1)
}
