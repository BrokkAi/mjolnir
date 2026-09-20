use super::*;

/// `title_controls` is the number of columns the host reserves at the right
/// of the title row for its own chips; the title stops short of them.
/// `pane_focused` draws the border in the focused style, which is how a host
/// with several panes shows which one has the keyboard.
pub(crate) fn render_transcript(
    frame: &mut Frame,
    area: Rect,
    chat: &mut ChatState,
    gesture_active: bool,
    title_controls: u16,
    pane_focused: bool,
) {
    let render_started = std::time::Instant::now();
    let viewport_height = usize::from(area.height.saturating_sub(2));
    chat.last_viewport_height = viewport_height;
    let block = theme::panel(pane_focused).padding(Padding::horizontal(1));
    let inner = block.inner(area);
    let content_width = inner.width;
    let window = chat.viewport(content_width, viewport_height);
    // The window resolves and clamps the anchor: an anchor inside the last
    // screenful snaps back to following the tail.
    chat.anchor = window.anchor;
    let at_tail = window.anchor == TranscriptAnchor::Bottom;
    let top = window.top;
    let title = transcript_title(chat, mj_core::clock::epoch_seconds());
    let block = block.title(truncate_line_to_width(
        title,
        usize::from(area.width.saturating_sub(2).saturating_sub(title_controls)),
    ));
    frame.render_widget(block, area);
    let visible = window
        .rows
        .into_iter()
        .take(usize::from(inner.height))
        .collect::<Vec<_>>();
    let visible_rows = visible.len();
    frame.render_widget(
        Paragraph::new(visible).style(Style::default().fg(theme::palette().text)),
        inner,
    );
    for (command_id, started) in chat.submission_renders.drain(..) {
        tracing::debug!(target: "mj_chat::latency", %command_id,
            render_ms = render_started.elapsed().as_secs_f64() * 1000.0,
            elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
            "authoritative submission frame prepared");
    }
    chat.rebuild_transcript_tool_click_targets(inner, top, visible_rows);
    chat.register_transcript_surface(inner, top, visible_rows, at_tail, gesture_active);
    let track = Rect::new(
        inner.right(),
        inner.y,
        u16::from(inner.width > 0),
        inner.height,
    );
    if let Some(geometry) =
        chat.update_transcript_scrollbar(track, content_width, viewport_height, top, chat.anchor)
    {
        render_scrollbar(frame, geometry);
    }
}

pub(crate) fn transcript_title(chat: &ChatState, now_epoch_seconds: u64) -> Line<'static> {
    let separator = theme::glyphs().footer_separator;
    let review_activity = chat
        .turn_review()
        .and_then(|review| review.view.activity_label());
    let summary = if chat.header_target.is_empty() || chat.header_profile.is_empty() {
        review_activity.map_or_else(
            || "Conversation".to_owned(),
            |activity| format!("Conversation{separator}{activity}"),
        )
    } else if let Some(activity) = review_activity {
        let mut columns = vec![chat.header_target.clone()];
        if !chat.queued_prompts.is_empty() {
            columns.push(format!("[Q {}]", chat.queued_prompts.len()));
        }
        columns.push(format!("[{activity}]"));
        columns.push(chat.header_profile.clone());
        columns.join("  ")
    } else {
        let mut columns = vec![chat.header_target.clone()];
        if !chat.queued_prompts.is_empty() {
            columns.push(format!("[Q {}]", chat.queued_prompts.len()));
        }
        columns.push(if !chat.activity_reachable {
            "Unreachable".to_owned()
        } else if chat.phase == crate::chat::WorkerPhase::Closed {
            "Closed".to_owned()
        } else if chat.phase == crate::chat::WorkerPhase::Closing {
            "Closing".to_owned()
        } else if !chat.pending_elicitations.is_empty() {
            "Question".to_owned()
        } else {
            chat.session_activity().display_clock(
                now_epoch_seconds,
                chat.turn_started_at_epoch_seconds,
                chat.current_step_started_at_ms,
                chat.detailed_activity_clocks,
            )
        });
        columns.push(chat.header_profile.clone());
        columns.join("  ")
    };
    let style = theme::title(false);
    let mut spans = vec![Span::styled(format!(" {summary}"), style)];
    if !chat.header_title.is_empty() {
        spans.push(Span::styled("  ", style));
        spans.push(Span::styled(
            chat.header_title.clone(),
            style.fg(theme::palette().secondary),
        ));
    }
    let suffix = match (chat.anchor, chat.render_mode) {
        (TranscriptAnchor::Bottom, TranscriptRenderMode::Rich) => " ".to_owned(),
        (TranscriptAnchor::Bottom, TranscriptRenderMode::Raw) => {
            format!("{separator}raw source ")
        }
        (TranscriptAnchor::Row { entry, .. }, _) => format!(
            "{separator}message {} of {}{separator}End to follow ",
            entry.saturating_add(1),
            chat.entries.len()
        ),
    };
    spans.push(Span::styled(suffix, style));
    Line::from(spans)
}

/// The gutter before every transcript body row, in the symbol set in force.
pub(crate) fn role_gutter() -> &'static str {
    theme::glyphs().role_gutter
}
pub(crate) const ROLE_GUTTER_WIDTH: usize = 2;

/// Body rows of an agent message for a summary viewport: the conversation's
/// own rendering, minus the parts that only make sense inside it.
///
/// No header row, blank rows dropped, and no role gutter. The gutter marks
/// continuation rows underneath a role header; a summary row has no header and
/// carries its own prefix, so a gutter there is a second marker for nothing.
/// The wrap width is widened by the gutter it drops, so the caller gets the
/// `width` columns of text it asked for.
pub(crate) fn preview_rows(source: &str, width: usize) -> Vec<Line<'static>> {
    let entry = ChatEntry::plain(0, ChatRole::Agent, source);
    entry_body_rows(
        &entry,
        width.saturating_add(ROLE_GUTTER_WIDTH),
        TranscriptRenderMode::Rich,
    )
    .into_iter()
    .filter(|line| !line_is_empty(line))
    .map(without_role_gutter)
    .collect()
}

/// The last rows of an agent message for a small preview viewport, rendered
/// by the same pipeline as the conversation view.
pub fn render_agent_message_tail(
    source: &str,
    width: usize,
    maximum_lines: usize,
) -> Vec<Line<'static>> {
    if width == 0 || maximum_lines == 0 {
        return Vec::new();
    }
    let lines = preview_rows(source, width);
    let start = lines.len().saturating_sub(maximum_lines);
    lines.into_iter().skip(start).collect()
}

/// First rows of an agent message for session-list summaries. Rich formatting
/// is retained, but the final visible row announces omitted content.
pub fn render_agent_message_head(
    source: &str,
    width: usize,
    maximum_lines: usize,
) -> Vec<Line<'static>> {
    if width == 0 || maximum_lines == 0 {
        return Vec::new();
    }
    let mut lines = preview_rows(source, width);
    let truncated = lines.len() > maximum_lines;
    lines.truncate(maximum_lines);
    if truncated && let Some(last) = lines.last_mut() {
        append_trimmed_ellipsis(last, 0);
    }
    lines
}

/// Render every row of the transcript. Rendering surfaces are all incremental
/// now, so this exists only for tests that assert on the whole projection.
#[cfg(test)]
pub(crate) fn transcript_lines(chat: &mut ChatState, width: u16) -> Vec<Line<'static>> {
    prepare_render_cache(
        &chat.entries,
        &mut chat.render_cache,
        width,
        chat.render_mode,
        &chat.expanded_tool_calls,
    );
    let mut lines = Vec::new();
    for index in 0..chat.entries.len() {
        lines.extend_from_slice(cached_entry_lines(
            &chat.entries,
            &mut chat.render_cache,
            index,
        ));
    }
    for entry in chat.trailing_entries() {
        lines.extend(chat.render_trailing_entry(&entry, usize::from(width)));
    }
    if lines.is_empty() {
        lines.push(empty_transcript_row(chat.transcript_loading));
    }
    lines
}

pub(crate) fn empty_transcript_row(loading: bool) -> Line<'static> {
    Line::from(Span::styled(
        if loading {
            "Loading…"
        } else {
            "No messages yet — send a prompt to begin."
        },
        Style::default()
            .fg(theme::palette().muted)
            .add_modifier(Modifier::ITALIC),
    ))
}

/// One entry's rows, header included, at `width`.
///
/// The reviewer pane draws with this so its conversation looks exactly like
/// the primary's rather than growing a second renderer.
pub(crate) fn render_entry_rows(
    entry: &ChatEntry,
    width: usize,
    mode: TranscriptRenderMode,
) -> Vec<Line<'static>> {
    render_transcript_entry(entry, width, mode)
}

pub(crate) fn render_transcript_entry(
    entry: &ChatEntry,
    width: usize,
    mode: TranscriptRenderMode,
) -> Vec<Line<'static>> {
    render_transcript_entry_with_options(entry, width, mode, false)
}

/// Render a completed tool after the user opens it from a compact Rich row.
/// The header keeps the active transcript mode, while the body uses the full
/// provider title and attached details just like Raw mode.
pub(crate) fn render_transcript_entry_expanded(
    entry: &ChatEntry,
    width: usize,
) -> Vec<Line<'static>> {
    render_transcript_entry_with_options(entry, width, TranscriptRenderMode::Rich, true)
}

pub(crate) fn render_transcript_entry_with_options(
    entry: &ChatEntry,
    width: usize,
    mode: TranscriptRenderMode,
    expanded_tool: bool,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let visual = entry_visual(entry);
    let time = match entry.role {
        ChatRole::User | ChatRole::Agent | ChatRole::System => {
            format_event_time(entry.recorded_at_ms)
        }
        _ => None,
    };
    let mut header = vec![
        Span::styled(
            format!("{} ", visual.glyph),
            visual.header_style.add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            visual.label.clone(),
            visual.header_style.add_modifier(Modifier::BOLD),
        ),
    ];
    if let Some(time) = time {
        header.push(Span::styled(
            format!("{}{time}", theme::glyphs().footer_separator),
            theme::muted(),
        ));
    }
    out.extend(wrap_styled_line(
        Line::from(header),
        width,
        ROLE_GUTTER_WIDTH,
    ));
    out.extend(entry_body_rows_with_options(
        entry,
        width,
        mode,
        expanded_tool,
    ));
    out.push(Line::from(""));
    out
}

/// The gutter-prefixed body rows of one entry, shared between the full
/// conversation view and the dashboard preview tail.
pub(crate) fn entry_body_rows(
    entry: &ChatEntry,
    width: usize,
    mode: TranscriptRenderMode,
) -> Vec<Line<'static>> {
    entry_body_rows_with_options(entry, width, mode, false)
}

pub(crate) fn entry_body_rows_with_options(
    entry: &ChatEntry,
    width: usize,
    mode: TranscriptRenderMode,
    expanded_tool: bool,
) -> Vec<Line<'static>> {
    let visual = entry_visual(entry);
    let content_width = width.saturating_sub(ROLE_GUTTER_WIDTH).max(1);
    entry_logical_lines(entry, mode, &visual, content_width, expanded_tool)
        .into_iter()
        .flat_map(|logical| {
            wrap_styled_line(logical.line, content_width, logical.continuation_indent)
        })
        .map(|row| with_role_gutter(row, visual.rail_style))
        .collect()
}

/// Format an optional transcript event timestamp as local 24-hour time.
pub fn format_event_time(recorded_at_ms: Option<i64>) -> Option<String> {
    chrono::DateTime::from_timestamp_millis(recorded_at_ms?).map(|time| {
        time.with_timezone(&chrono::Local)
            .format("%H:%M")
            .to_string()
    })
}

pub(super) fn entry_logical_lines(
    entry: &ChatEntry,
    mode: TranscriptRenderMode,
    visual: &EntryVisual,
    width: usize,
    expanded_tool: bool,
) -> Vec<LogicalLine> {
    if entry.role == ChatRole::Plan {
        return entry
            .plan
            .iter()
            .map(|item| {
                let (glyph, style) = match item.status {
                    PlanStatus::Pending => (
                        theme::glyphs().pending,
                        Style::default().fg(theme::palette().muted),
                    ),
                    PlanStatus::Running => (
                        theme::glyphs().running,
                        Style::default().fg(theme::palette().warning),
                    ),
                    PlanStatus::Completed => (
                        theme::glyphs().check,
                        Style::default().fg(theme::palette().success),
                    ),
                };
                LogicalLine {
                    line: Line::from(vec![
                        Span::styled(format!("{glyph} "), style),
                        Span::styled(item.text.clone(), visual.body_style),
                    ]),
                    continuation_indent: 2,
                }
            })
            .collect();
    }

    let details = entry
        .tool_content
        .iter()
        .chain(&entry.tool_diffstats)
        .chain(&entry.tool_locations)
        .cloned()
        .collect::<Vec<_>>();
    let mut source = match mode {
        TranscriptRenderMode::Rich if expanded_tool && entry.role == ChatRole::Tool => {
            if details.is_empty() {
                entry.text.clone()
            } else {
                format!("{}\n{}", entry.text, details.join("\n"))
            }
        }
        TranscriptRenderMode::Raw if !details.is_empty() => {
            format!("{}\n{}", entry.text, details.join("\n"))
        }
        TranscriptRenderMode::Rich if !entry.tool_diffstats.is_empty() => {
            format!(
                "{}\n{}",
                entry.tool_summary.as_deref().unwrap_or(&entry.text),
                entry.tool_diffstats.join("\n")
            )
        }
        TranscriptRenderMode::Rich if entry.role == ChatRole::Tool => entry
            .tool_summary
            .as_deref()
            .unwrap_or(&entry.text)
            .to_owned(),
        _ => entry.text.clone(),
    };
    if entry.leading_omitted {
        source.insert_str(0, "[… earlier content omitted …]\n");
    }
    match mode {
        TranscriptRenderMode::Rich => {
            markdown_lines(&source, visual.body_style, visual.header_style, width)
        }
        TranscriptRenderMode::Raw => raw_lines(&source, visual.body_style),
    }
}

/// The mark a transcript header draws for one entry, in the symbol set in
/// force. `mj_client::transcript::entry_glyph` keeps the Unicode marks the
/// browser projection sends; a terminal has to be able to fall back to ASCII.
fn role_glyph(entry: &ChatEntry) -> &'static str {
    let glyphs = theme::glyphs();
    match entry.role {
        ChatRole::User => glyphs.role_user,
        ChatRole::Agent => glyphs.role_agent,
        ChatRole::Thought => glyphs.role_thought,
        ChatRole::Plan => glyphs.role_plan,
        ChatRole::PlanProposal => glyphs.role_plan_proposal,
        ChatRole::System => glyphs.rule,
        ChatRole::Tool => tool_presentation(entry.tool_status.unwrap_or(ToolStatus::Pending)).0,
    }
}

pub(super) fn entry_visual(entry: &ChatEntry) -> EntryVisual {
    match entry.role {
        ChatRole::User => {
            let style = Style::default().fg(theme::palette().accent);
            EntryVisual {
                glyph: role_glyph(entry),
                label: user_label(entry).into(),
                header_style: style,
                body_style: Style::default(),
                rail_style: style,
            }
        }
        ChatRole::Agent => {
            let style = Style::default().fg(theme::palette().secondary);
            EntryVisual {
                glyph: role_glyph(entry),
                label: "Agent".into(),
                header_style: style,
                body_style: Style::default(),
                rail_style: Style::default().fg(theme::palette().border),
            }
        }
        ChatRole::Thought => {
            let style = Style::default()
                .fg(theme::palette().muted)
                .add_modifier(Modifier::ITALIC);
            EntryVisual {
                glyph: role_glyph(entry),
                label: "Thinking".into(),
                header_style: style,
                body_style: style,
                rail_style: Style::default().fg(theme::palette().border),
            }
        }
        ChatRole::Tool => {
            let status = entry.tool_status.unwrap_or(ToolStatus::Pending);
            let (_glyph, label, style) = tool_presentation(status);
            let body_style = match status {
                ToolStatus::Pending | ToolStatus::Completed => {
                    Style::default().fg(theme::palette().muted)
                }
                ToolStatus::Running | ToolStatus::Failed => Style::default(),
            };
            EntryVisual {
                glyph: role_glyph(entry),
                label: format!("Tool{}{label}", theme::glyphs().footer_separator),
                header_style: style,
                body_style,
                rail_style: Style::default().fg(theme::palette().border),
            }
        }
        ChatRole::Plan => {
            let style = Style::default().fg(theme::palette().secondary);
            EntryVisual {
                glyph: role_glyph(entry),
                label: "Plan".into(),
                header_style: style,
                body_style: Style::default(),
                rail_style: style,
            }
        }
        ChatRole::PlanProposal => {
            let style = Style::default().fg(theme::palette().secondary);
            EntryVisual {
                glyph: role_glyph(entry),
                label: "Proposed plan".into(),
                header_style: style,
                body_style: Style::default(),
                rail_style: style,
            }
        }
        ChatRole::System => {
            let style = Style::default().fg(theme::palette().muted);
            EntryVisual {
                glyph: role_glyph(entry),
                label: "Mjolnir".into(),
                header_style: style,
                body_style: style,
                rail_style: style,
            }
        }
    }
}

pub(crate) fn tool_presentation(status: ToolStatus) -> (&'static str, &'static str, Style) {
    match status {
        ToolStatus::Pending => (
            theme::glyphs().bullet,
            "waiting",
            Style::default().fg(theme::palette().muted),
        ),
        ToolStatus::Running => (
            theme::glyphs().running,
            "running",
            Style::default().fg(theme::palette().warning),
        ),
        ToolStatus::Completed => (
            theme::glyphs().check,
            "done",
            Style::default().fg(theme::palette().muted),
        ),
        ToolStatus::Failed => (
            theme::glyphs().failed.trim(),
            "failed",
            Style::default().fg(theme::palette().error),
        ),
    }
}

pub(crate) fn with_role_gutter(line: Line<'static>, style: Style) -> Line<'static> {
    let mut spans = Vec::with_capacity(line.spans.len() + 1);
    spans.push(Span::styled(role_gutter(), style));
    spans.extend(line.spans);
    Line::from(spans)
}

/// Drops the leading role gutter from a rendered row, for viewports that do
/// not draw the rail it belongs to.
pub(crate) fn without_role_gutter(mut line: Line<'static>) -> Line<'static> {
    if line
        .spans
        .first()
        .is_some_and(|span| span.content == role_gutter())
    {
        line.spans.remove(0);
    }
    line
}

pub(crate) fn line_is_empty(line: &Line<'_>) -> bool {
    line.spans
        .iter()
        .all(|span| span.content.trim().is_empty() || span.content.as_ref() == role_gutter())
}
