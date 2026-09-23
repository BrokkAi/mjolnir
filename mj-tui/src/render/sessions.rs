use super::*;

/// Session-row rendering results that the caller folds back into the
/// combined surface's mouse hitboxes once the borrow of the session list has
/// ended.
pub(crate) struct SessionRowsRendered {
    pub(crate) session_row_areas: Vec<(usize, Rect)>,
    pub(crate) project_heading_areas: Vec<(String, Rect)>,
}

pub(crate) const PANE_SIZE_CONTROLS_WIDTH: u16 = 11;
pub(crate) const PANE_SIZE_CONTROLS_WIDTH_WITHOUT_MAXIMUM: u16 = 7;
pub(crate) const PANE_SIZE_CONTROL_WIDTH: u16 = 3;

pub(crate) fn pane_size_controls_width(maximize_enabled: bool) -> u16 {
    if maximize_enabled {
        PANE_SIZE_CONTROLS_WIDTH
    } else {
        PANE_SIZE_CONTROLS_WIDTH_WITHOUT_MAXIMUM
    }
}

/// The width left for a pane's left title after preserving the border, a gap,
/// and the currently visible right-aligned size controls.
pub(crate) fn pane_title_content_width(width: u16, maximize_enabled: bool) -> u16 {
    width.saturating_sub(2 + 1 + pane_size_controls_width(maximize_enabled))
}

pub(crate) fn displayed_pane_size(active: PaneSize, maximize_enabled: bool) -> PaneSize {
    if active == PaneSize::Maximized && !maximize_enabled {
        PaneSize::Standard
    } else {
        active
    }
}

/// The title-bar controls. Their padded backgrounds are the buttons; the
/// unstyled cells between them keep inactive controls visually distinct.
pub(crate) fn pane_size_controls(active: PaneSize, maximize_enabled: bool) -> Line<'static> {
    let active = displayed_pane_size(active, maximize_enabled);
    let mut spans = Vec::new();
    let glyphs = theme::glyphs();
    for (index, (size, glyph)) in [
        (PaneSize::Minimized, glyphs.size_minimized),
        (PaneSize::Standard, glyphs.size_standard),
        (PaneSize::Maximized, glyphs.size_maximized),
    ]
    .into_iter()
    .filter(|(size, _)| *size != PaneSize::Maximized || maximize_enabled)
    .enumerate()
    {
        if index > 0 {
            spans.push(Span::raw(" "));
        }
        let style = if size == active {
            theme::active_control()
        } else {
            theme::muted().bg(theme::palette().surface)
        };
        spans.push(Span::styled(format!(" {glyph} "), style));
    }
    Line::from(spans).right_aligned()
}

/// Minimized panes have no right border, but keep its column as horizontal
/// rule so their controls line up with those in a fully bordered pane.
pub(crate) fn minimized_pane_size_controls(
    _focused: bool,
    maximize_enabled: bool,
) -> Line<'static> {
    let mut controls = pane_size_controls(PaneSize::Minimized, maximize_enabled);
    controls.spans.push(Span::raw(theme::glyphs().rule));
    controls
}

/// Screen rectangles occupied by the padded control chips in a bordered
/// pane's right-aligned title.
pub(crate) fn pane_size_control_areas(
    area: Rect,
    pane: SupportPane,
    maximize_enabled: bool,
) -> Vec<(SupportPane, PaneSize, Rect)> {
    let start = area
        .right()
        .saturating_sub(1)
        .saturating_sub(pane_size_controls_width(maximize_enabled));
    [PaneSize::Minimized, PaneSize::Standard, PaneSize::Maximized]
        .into_iter()
        .filter(|size| *size != PaneSize::Maximized || maximize_enabled)
        .enumerate()
        .map(|(index, size)| {
            (
                pane,
                size,
                Rect::new(
                    start.saturating_add(index as u16 * 4),
                    area.y,
                    PANE_SIZE_CONTROL_WIDTH,
                    1,
                ),
            )
        })
        .collect()
}

/// One table row of the Sessions pane, already laid out.
pub(crate) struct DrawnSessionRow {
    /// Index into `ordered_sessions()` for the session this row draws.
    session: Option<usize>,
    /// Project key of the heading this row carries, if it opens a group.
    heading: Option<String>,
    lines: Vec<Line<'static>>,
    /// Blank rows drawn under this one, to separate groups.
    spacing: u16,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct SessionRowsRenderOptions {
    force_expanded: bool,
    summary_only: bool,
    show_project_numbers: bool,
    show_selection: bool,
}

impl SessionRowsRenderOptions {
    pub(crate) const DASHBOARD: Self = Self {
        force_expanded: false,
        summary_only: false,
        show_project_numbers: true,
        show_selection: true,
    };

    pub(crate) const MINIMIZED: Self = Self {
        force_expanded: false,
        summary_only: true,
        show_project_numbers: true,
        show_selection: true,
    };
}

impl DrawnSessionRow {
    pub(crate) fn content_height(&self) -> u16 {
        u16::try_from(self.lines.len()).unwrap_or(u16::MAX)
    }
}

/// Lays the pane out without drawing it, so the layout can ask how tall it
/// wants to be before it has any rows to give it.
pub(crate) fn drawn_session_rows(dashboard: &DashboardState, width: u16) -> Vec<DrawnSessionRow> {
    drawn_session_rows_with_options(dashboard, width, SessionRowsRenderOptions::DASHBOARD)
}

pub(crate) fn drawn_session_rows_with_options(
    dashboard: &DashboardState,
    width: u16,
    options: SessionRowsRenderOptions,
) -> Vec<DrawnSessionRow> {
    let now_epoch_seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let animation_ms = mj_chat::spinner::elapsed_ms();
    let sessions = dashboard.ordered_sessions();
    let targets = session_display_targets(dashboard, &sessions);
    let flow_rows = dashboard.sessions_rows().into_iter().map(|row| match row {
        SessionsRow::ProjectHeading { key, label, number } => SessionsRow::ProjectHeading {
            key,
            label,
            number: options.show_project_numbers.then_some(number).flatten(),
        },
        SessionsRow::Session { index, expanded } => SessionsRow::Session {
            index,
            expanded: options.force_expanded || expanded,
        },
    });
    let mut rows: Vec<DrawnSessionRow> = Vec::new();
    let mut pending_heading: Option<(String, Line<'static>)> = None;
    for row in flow_rows {
        match row {
            SessionsRow::ProjectHeading { key, label, number } => {
                let hotkey = number.map_or_else(String::new, |number| format!("[{number}] "));
                // A heading is drawn inside the row beneath it, so the table's
                // selection index keeps counting sessions and nothing else.
                if let Some(last) = rows.last_mut() {
                    last.spacing = 1;
                }
                let mut spans = vec![Span::styled(
                    format!("{hotkey}{label}"),
                    Style::default()
                        .fg(theme::palette().secondary)
                        .add_modifier(Modifier::BOLD),
                )];
                // A folded project hides its rows, so the heading says what
                // is waiting inside it. An unfolded one shows every symbol.
                if dashboard.collapsed_project_keys.contains(&key) {
                    spans.extend(attention_badge(dashboard.project_attention_summary(&key)));
                }
                pending_heading = Some((key, Line::from(spans)));
            }
            SessionsRow::Session { index, expanded } => {
                let Some(session) = sessions.get(index) else {
                    continue;
                };
                // Supply a display-only title without changing the durable session record.
                let named_session = dashboard.go.is_some().then(|| {
                    let mut named = (*session).clone();
                    named.session_title_override =
                        Some(dashboard.go_conversation_title(&session.id));
                    named
                });
                let session = named_session.as_ref().unwrap_or(session);
                let detail = dashboard.session_details.get(&session.id);
                let review = dashboard.session_review(&session.id);
                let unreachable = dashboard.unreachable_sessions.contains(&session.id);
                let attention = dashboard.attention_level(&session.id);
                let facts = SessionRowFacts {
                    detail,
                    unreachable,
                    state: session.state,
                    now_epoch_seconds,
                    attention,
                };
                let primary_busy = !unreachable
                    && session.state == SessionState::Running
                    && detail.is_some_and(|detail| {
                        detail.activity.is_working(
                            detail.current_turn_started_at,
                            !detail.pending_elicitations.is_empty(),
                        )
                    });
                // Review work is independent of the session's primary
                // lifecycle. A review can keep animating while the session
                // is stopped or unreachable, so do not gate it on either.
                let busy = primary_busy || review.is_some_and(RuntimeReviewView::is_working);
                let spinner = busy.then(|| {
                    mj_chat::spinner::compact_frame(dashboard.config.spinner, animation_ms)
                });
                let operation = dashboard.session_operations.get(&session.id);
                let target = targets.get(index).cloned().unwrap_or_default();
                let git = dashboard.git_row_text(&session.id);
                let permission = session_permission_badge(session, operation, &dashboard.config);
                // The selection drives which conversation is on screen, so
                // the caret marks it in both forms.
                let selected = options.show_selection
                    && dashboard.selected_session_id.as_deref() == Some(session.id.as_str());
                let glyphs = theme::glyphs();
                let symbol = dashboard
                    .transition_kind(&session.id)
                    .map(|transition| match transition {
                        SessionTransitionKind::Starting => glyphs.starting,
                        SessionTransitionKind::Resuming => glyphs.resuming,
                        SessionTransitionKind::Moving => glyphs.moving,
                        SessionTransitionKind::Suspending => glyphs.stopping,
                        SessionTransitionKind::Destroying => glyphs.destroying,
                    })
                    .unwrap_or_else(|| facts.status_symbol(operation));
                let prefix = format!("{}{symbol} ", if selected { glyphs.selected } else { "  " });
                let (heading_key, heading_line) = match pending_heading.take() {
                    Some((key, line)) => (Some(key), Some(line)),
                    None => (None, None),
                };
                let mut lines = Vec::new();
                lines.extend(heading_line);
                let spacing = u16::from(expanded && !options.summary_only);
                if session.configuration_issue(&dashboard.config).is_some() {
                    lines.push(Line::styled(
                        format!("{prefix}{}", session_name(session)),
                        Style::default().fg(theme::palette().session_error),
                    ));
                    lines.push(Line::styled(
                        "  Needs config repair",
                        Style::default().fg(theme::palette().session_error),
                    ));
                    if expanded && !options.summary_only {
                        lines.push(Line::from(
                            match dashboard.first_key_label(crate::CommandId::OpenConfig) {
                                Some(key) => {
                                    format!("  Enter for repair details · {key} settings")
                                }
                                None => "  Enter for repair details".to_owned(),
                            },
                        ));
                    }
                    rows.push(DrawnSessionRow {
                        session: Some(index),
                        heading: heading_key,
                        lines,
                        spacing,
                    });
                    continue;
                }

                if let Some(transition) = dashboard.transition_kind(&session.id) {
                    lines.push(session_transition_line(
                        &prefix,
                        session,
                        transition,
                        operation,
                        now_epoch_seconds,
                        &target,
                        width,
                        None,
                    ));
                    rows.push(DrawnSessionRow {
                        session: Some(index),
                        heading: heading_key,
                        lines,
                        spacing,
                    });
                    continue;
                }
                if let Some(transition) = dashboard.transition_failure_kind(&session.id) {
                    lines.push(session_transition_line(
                        &prefix,
                        session,
                        transition,
                        None,
                        now_epoch_seconds,
                        &target,
                        width,
                        session.last_error.as_deref(),
                    ));
                    rows.push(DrawnSessionRow {
                        session: Some(index),
                        heading: heading_key,
                        lines,
                        spacing,
                    });
                    continue;
                }
                if expanded && !options.summary_only {
                    expanded_session_lines(
                        &mut lines,
                        session,
                        detail,
                        review,
                        unreachable,
                        attention,
                        operation,
                        now_epoch_seconds,
                        &target,
                        permission,
                        width,
                        &prefix,
                        spinner,
                        dashboard.config.advanced.detailed_activity_clocks,
                        git.as_deref(),
                    );
                } else {
                    compact_session_lines(
                        &mut lines,
                        &prefix,
                        session,
                        detail,
                        review,
                        unreachable,
                        attention,
                        operation,
                        now_epoch_seconds,
                        &target,
                        permission,
                        spinner,
                        width,
                    );
                }
                if selected {
                    for line in lines.iter_mut().skip(usize::from(heading_key.is_some())) {
                        line.style = line.style.patch(theme::raised());
                    }
                }
                rows.push(DrawnSessionRow {
                    session: Some(index),
                    heading: heading_key,
                    lines,
                    spacing,
                });
            }
        }
    }
    // The last group never needs a trailing blank row.
    if let Some(last) = rows.last_mut() {
        last.spacing = 0;
    }
    rows
}

/// The most of the git row text — `⎇ main ↑1 ↓2 ±4` — that fits `free` cells,
/// or `None` when not even a marker and a readable name fit.
///
/// Every session created with a managed worktree gets an `mj/<32 hex>` branch,
/// which is wider than the sidebar on its own. Giving up the whole marker there
/// also gave up the counts, which are the part that says the checkout moved, so
/// the name gives up its middle first: `⎇ mj/d45dc…0580 ±2`. The middle of a
/// generated name is what nobody reads; its start and its last few characters
/// are what tells two branches apart.
fn fit_git_row_text(git: &str, free: usize) -> Option<String> {
    /// Below this many cells an elided name says nothing worth the room.
    const NAME_FLOOR: usize = 9;
    /// Characters of the name's tail that survive the elision.
    const TAIL: usize = 4;

    if Line::raw(git).width() <= free {
        return Some(git.to_owned());
    }
    // The marker and the branch name are the first two words; a branch name
    // never contains a space. Whatever follows is the counts, kept with the
    // space that separates them.
    let (marker, rest) = git.split_once(' ')?;
    let (name, counts) = match rest.find(' ') {
        Some(at) => (&rest[..at], Some(&rest[at..])),
        None => (rest, None),
    };
    let marker_room = Line::raw(marker).width() + 1;
    let elided = |room: usize| -> Option<String> {
        if room < NAME_FLOOR {
            return None;
        }
        if Line::raw(name).width() <= room {
            return Some(name.to_owned());
        }
        // The last characters are what tell two generated names apart, so they
        // are kept and the truncated head carries the ellipsis.
        let tail = name
            .chars()
            .skip(name.chars().count().saturating_sub(TAIL))
            .collect::<String>();
        let head = truncate_to_cells(
            name,
            room.saturating_sub(Line::raw(tail.as_str()).width()),
            Truncate::PLAIN,
        );
        Some(format!("{head}{tail}"))
    };
    // The counts are worth more than the middle of the name, so they are what
    // the elision makes room for.
    if let Some(counts) = counts
        && let Some(name) = elided(free.saturating_sub(marker_room + Line::raw(counts).width()))
    {
        return Some(format!("{marker} {name}{counts}"));
    }
    elided(free.saturating_sub(marker_room)).map(|name| format!("{marker} {name}"))
}

/// The four rows an expanded session draws: name, status and identity, and
/// two wrapped rows of the current output. The output block is always two
/// rows, even with nothing to say, so every expanded session is the same
/// height and the layout can be computed from a count.
#[allow(clippy::too_many_arguments)]
pub(crate) fn expanded_session_lines(
    lines: &mut Vec<Line<'static>>,
    session: &SessionRecord,
    detail: Option<&SessionDetail>,
    review: Option<&RuntimeReviewView>,
    unreachable: bool,
    attention: AttentionLevel,
    operation: Option<&SessionOperationDisplay>,
    now_epoch_seconds: u64,
    target: &str,
    permission: Option<Span<'static>>,
    width: u16,
    prefix: &str,
    spinner: Option<&'static str>,
    detailed_activity_clocks: bool,
    git: Option<&str>,
) {
    let style = Style::default().fg(session_band_color(attention, detail, session.state));
    let name = recovery_warning_name(session, session_name(session).to_owned(), now_epoch_seconds);
    // The ellipsis action occupies the last three cells of the first line.
    // Keep the activity and output lines at the full content width so a
    // running clock and queued count remain readable in a compact pane.
    let title_width = width.saturating_sub(if width < 24 { 3 } else { 5 });
    let name_room = usize::from(title_width).saturating_sub(Line::raw(prefix).width());
    let title = truncate_to_cells(&name, name_room, Truncate::PLAIN);
    let mut title_spans = vec![Span::styled(format!("{prefix}{title}"), style)];
    // The branch follows the name when the line has room. A name is rarely as
    // wide as the sidebar, so this is where the branch costs nothing.
    if let Some(git) = git
        && let Some(text) = fit_git_row_text(
            git,
            name_room.saturating_sub(Line::raw(title.as_str()).width() + 2),
        )
    {
        title_spans.push(Span::styled(format!("  {text}"), theme::muted()));
    }
    lines.push(Line::from(title_spans));
    lines.push(session_activity_line(
        "  ",
        session,
        detail,
        review,
        unreachable,
        attention,
        operation,
        now_epoch_seconds,
        target,
        permission,
        spinner,
        width,
        detailed_activity_clocks,
    ));

    let (label, message, muted) = match detail.and_then(current_agent_excerpt) {
        Some(message) => ("", Some(message), false),
        None => match detail.and_then(|detail| detail.last_user_message.as_deref()) {
            Some(message) => ("You: ", Some(message), true),
            None => ("", None, false),
        },
    };
    let output_width = usize::from(width.saturating_sub(2));
    let mut output = message
        .map(|message| {
            let text = if label.is_empty() {
                message.replace('\n', " ")
            } else {
                format!("{label}{message}")
            };
            render_agent_message_head(&text, output_width, 2)
        })
        .unwrap_or_default();
    if output.is_empty() {
        output.push(Line::raw("No messages yet"));
    }
    output.resize(2, Line::default());
    for mut line in output.into_iter().take(2) {
        let mut spans = vec![Span::raw("  ")];
        spans.append(&mut line.spans);
        let mut line = Line::from(spans);
        if muted {
            line.style = Style::default().fg(theme::palette().muted);
        }
        lines.push(line);
    }
}

/// The second row of a session. Actionable state and queued work come first so
/// a narrow pane cannot hide them behind the target or profile identity.
#[allow(clippy::too_many_arguments)]
pub(crate) fn session_activity_line(
    prefix: &str,
    session: &SessionRecord,
    detail: Option<&SessionDetail>,
    review: Option<&RuntimeReviewView>,
    unreachable: bool,
    attention: AttentionLevel,
    operation: Option<&SessionOperationDisplay>,
    now_epoch_seconds: u64,
    target: &str,
    permission: Option<Span<'static>>,
    spinner: Option<&'static str>,
    width: u16,
    detailed_activity_clocks: bool,
) -> Line<'static> {
    let facts = SessionRowFacts {
        detail,
        unreachable,
        state: session.state,
        now_epoch_seconds,
        attention,
    };
    let mut status = if let Some(operation) = operation {
        let (label, started_at) = operation_status(operation);
        format!(
            "{label} {}",
            mj_client::usage_format::format_clock(now_epoch_seconds.saturating_sub(started_at))
        )
    } else if facts.state == SessionState::Error {
        "Error".to_owned()
    } else if session.state == SessionState::Provisioning {
        let started_at = session_updated_at_epoch_seconds(session).unwrap_or(now_epoch_seconds);
        format!(
            "Launch {}",
            mj_client::usage_format::format_clock(now_epoch_seconds.saturating_sub(started_at))
        )
    } else if facts.unreachable {
        "Unreachable".to_owned()
    } else if detail.is_some_and(|d| {
        matches!(
            d.activity.state().last_known(),
            mj_core::activity::ActivityState::CheckingContinuation
        )
    }) {
        "Checking continuation".to_owned()
    } else if facts.needs_input() {
        "Question".to_owned()
    } else if let Some(label) = review_status_label(review) {
        label.to_owned()
    } else if detail.is_some_and(|detail| {
        matches!(
            detail.activity.state().last_known(),
            mj_core::activity::ActivityState::Expecting { .. }
        )
    }) {
        "expecting the agent to continue".to_owned()
    } else if facts.state.is_active() {
        facts.clock(detailed_activity_clocks)
    } else {
        format!("{:?}", facts.state)
    };
    let queue = detail
        .map(|detail| detail.queued_prompts.len())
        .filter(|count| *count > 0)
        .map(|count| {
            if width <= 24 {
                format!("Q{count}")
            } else {
                format!("[Q {count}]")
            }
        });
    // At the minimum sidebar width, retain both the attention marker and the
    // queue count while using a compact status word.
    if width <= 24 && status == "Unreachable" {
        status = "Offline".to_owned();
    }
    let available = usize::from(width);
    // A minimized cell's second line is reserved for activity and its queue.
    // Keep it two cells in from the pane edge so the summary reads as a
    // continuation of the title line while retaining enough room for status.
    let compact = width <= 24;
    let prefix = if compact { "  " } else { prefix };
    let queue_width = queue
        .as_ref()
        .map_or(0, |queue| Line::raw(queue.as_str()).width() + 1);
    let spinner = spinner.filter(|spinner| {
        Line::raw(prefix).width()
            + Line::raw(*spinner).width()
            + 1
            + Line::raw(status.as_str()).width()
            + queue_width
            <= available
    });
    let spinner_width = spinner.map_or(0, |spinner| Line::raw(spinner).width() + 1);
    let status = truncate_to_cells(
        &status,
        available.saturating_sub(Line::raw(prefix).width() + spinner_width + queue_width),
        Truncate::PLAIN,
    );
    let status_width = Line::raw(status.as_str()).width() + queue_width + 2;
    let identity_width = if compact {
        0
    } else {
        available.saturating_sub(Line::raw(prefix).width() + spinner_width + status_width)
    };
    let badge_text = permission
        .as_ref()
        .map_or_else(String::new, |permission| format!(" {}", permission.content));
    let badge_width = Line::raw(badge_text.as_str()).width();
    let show_permission = badge_width > 0 && identity_width >= badge_width.saturating_add(1);
    let identity_width =
        identity_width.saturating_sub(if show_permission { badge_width } else { 0 });
    let profile = operation
        .and_then(|operation| operation.resume_destination.as_ref())
        .map(|(profile, _)| profile.as_str())
        .unwrap_or(&session.last_profile);
    let (target_width, profile_width) =
        if Line::raw(target).width() + 2 + Line::raw(profile).width() <= identity_width {
            (Line::raw(target).width(), Line::raw(profile).width())
        } else {
            let half = identity_width.saturating_sub(2) / 2;
            (half, identity_width.saturating_sub(2).saturating_sub(half))
        };
    let target = truncate_to_cells(target, target_width, Truncate::PLAIN);
    let profile = truncate_to_cells(profile, profile_width, Truncate::PLAIN);
    let identity = match (target.is_empty(), profile.is_empty()) {
        (true, true) => String::new(),
        (true, false) => profile,
        (false, true) => target,
        (false, false) => format!("{target}  {profile}"),
    };
    let mut spans = vec![Span::raw(prefix.to_owned())];
    if let Some(spinner) = spinner {
        spans.push(Span::styled(format!("{spinner} "), facts.style()));
    }
    let status_separator = if identity.is_empty() && !show_permission {
        ""
    } else {
        "  "
    };
    spans.push(Span::styled(identity, facts.style()));
    if show_permission && let Some(permission) = permission {
        spans.push(Span::raw(" "));
        spans.push(permission);
    }
    spans.push(Span::styled(
        format!("{status_separator}{status}"),
        facts.style().add_modifier(if facts.needs_input() {
            Modifier::BOLD
        } else {
            Modifier::empty()
        }),
    ));
    if let Some(queue) = queue {
        spans.push(Span::styled(
            format!(" {queue}"),
            facts.style().add_modifier(Modifier::BOLD),
        ));
    }
    Line::from(spans)
}

/// Folded and minimized sessions retain a fixed two-line summary: identity on
/// the first line, then status, clock, and queued work on the second.
#[allow(clippy::too_many_arguments)]
pub(crate) fn compact_session_lines(
    lines: &mut Vec<Line<'static>>,
    prefix: &str,
    session: &SessionRecord,
    detail: Option<&SessionDetail>,
    review: Option<&RuntimeReviewView>,
    unreachable: bool,
    attention: AttentionLevel,
    operation: Option<&SessionOperationDisplay>,
    now_epoch_seconds: u64,
    target: &str,
    permission: Option<Span<'static>>,
    spinner: Option<&'static str>,
    width: u16,
) {
    let facts = SessionRowFacts {
        detail,
        unreachable,
        state: session.state,
        now_epoch_seconds,
        attention,
    };
    let style = facts.style();
    let name = recovery_warning_name(session, session_name(session).to_owned(), now_epoch_seconds);
    // The ellipsis action occupies the last three cells of the first line;
    // retain the full width for the status line below it.
    let title_width = width.saturating_sub(if width < 24 { 3 } else { 5 });
    lines.push(Line::styled(
        format!(
            "{prefix}{}",
            truncate_to_cells(
                &name,
                usize::from(title_width).saturating_sub(Line::raw(prefix).width()),
                Truncate::PLAIN,
            )
        ),
        style,
    ));
    lines.push(session_activity_line(
        "  ",
        session,
        detail,
        review,
        unreachable,
        attention,
        operation,
        now_epoch_seconds,
        target,
        permission,
        spinner,
        width,
        false,
    ));
}

/// Shared state used to derive a session's status, activity clock, and colour.
#[derive(Clone, Copy)]
pub(crate) struct SessionRowFacts<'a> {
    detail: Option<&'a SessionDetail>,
    unreachable: bool,
    state: SessionState,
    now_epoch_seconds: u64,
    /// How much this session needs a person, from the one shared ladder. The
    /// symbol and the band colour both read it, so a row cannot say one thing
    /// with its glyph and another with its colour.
    attention: AttentionLevel,
}

impl SessionRowFacts<'_> {
    /// One status symbol is used in expanded, compact, and minimized rows so
    /// lifecycle and attention state remains visible at every width.
    pub(crate) fn status_symbol(
        &self,
        operation: Option<&SessionOperationDisplay>,
    ) -> &'static str {
        let glyphs = theme::glyphs();
        if operation.is_some() {
            return glyphs.working;
        }
        // The same scale the attention queue and the badges read, so a row
        // can never show a symbol the queue disagrees with.
        match self.attention {
            AttentionLevel::Failed => return glyphs.failed,
            AttentionLevel::Unreachable => return glyphs.unreachable,
            AttentionLevel::Waiting => return glyphs.waiting,
            AttentionLevel::Unread => return glyphs.unread,
            AttentionLevel::Working => return glyphs.working,
            AttentionLevel::Idle | AttentionLevel::Inactive => {}
        }
        // What is left is a session that wants nothing: name the lifecycle
        // state it is in, or say how idle it is.
        match self.state {
            SessionState::Stopped => glyphs.stopped,
            SessionState::Provisioning => glyphs.starting,
            SessionState::Checkpointing => glyphs.checkpointing,
            SessionState::Closing => glyphs.stopping,
            SessionState::Destroying => glyphs.destroying,
            SessionState::Lost
            | SessionState::Error
            | SessionState::DestroyedWithDataLoss
            | SessionState::Disconnected
            | SessionState::Running => match self.detail {
                Some(detail)
                    if detail.materialized_applied_event_ordinal.is_some()
                        || detail.activity.execution.is_some()
                        || detail.activity.idle_since_ms.is_some() =>
                {
                    glyphs.idle
                }
                _ => glyphs.unknown,
            },
        }
    }

    pub(crate) fn style(&self) -> Style {
        Style::default().fg(session_band_color(self.attention, self.detail, self.state))
    }

    pub(crate) fn clock(&self, detailed: bool) -> String {
        let activity = self.detail.map(|detail| &detail.activity);
        activity.unwrap_or(&*EMPTY_ACTIVITY).display_clock(
            self.now_epoch_seconds,
            self.detail
                .and_then(|detail| detail.current_turn_started_at),
            self.detail
                .and_then(|detail| detail.current_step_started_at_ms),
            detailed,
        )
    }

    pub(crate) fn needs_input(&self) -> bool {
        self.detail
            .is_some_and(|detail| !detail.pending_elicitations.is_empty() || detail.awaiting_input)
    }
}

/// Return the moving value currently used in a session's activity line.
/// Keeping this decision beside the renderer prevents invalidation from
/// inventing a second precedence order for operation, lifecycle, and activity
/// clocks.
pub(crate) fn session_display_clock(
    dashboard: &DashboardState,
    session: &SessionRecord,
    detail: Option<&SessionDetail>,
    review: Option<&RuntimeReviewView>,
    unreachable: bool,
    operation: Option<&SessionOperationDisplay>,
    now_epoch_seconds: u64,
) -> Option<String> {
    let started_at = if let Some(operation) = operation {
        Some(operation_status(operation).1)
    } else if dashboard.transition_kind(&session.id).is_some()
        || dashboard.transition_failure_kind(&session.id).is_some()
        || session.state == SessionState::Provisioning
    {
        session_updated_at_epoch_seconds(session)
    } else {
        None
    };
    if let Some(started_at) = started_at {
        return Some(mj_client::usage_format::format_clock(
            now_epoch_seconds.saturating_sub(started_at),
        ));
    }

    if session.last_error.is_some()
        || unreachable
        || detail
            .is_some_and(|detail| !detail.pending_elicitations.is_empty() || detail.awaiting_input)
        || review.is_some_and(|review| review.activity_label().is_some())
        || !session.state.is_active()
    {
        return None;
    }
    let detail = detail?;
    if detail.activity.is_idle(detail.current_turn_started_at)
        || matches!(
            detail.activity.execution,
            Some(mj_core::relay::RelayExecutionState::Closing)
                | Some(mj_core::relay::RelayExecutionState::Closed)
        )
    {
        return None;
    }
    let detailed = dashboard.pane_size(SupportPane::Sessions) != PaneSize::Minimized
        && dashboard.project_is_expanded(session)
        && dashboard.config.advanced.detailed_activity_clocks;
    Some(
        SessionRowFacts {
            detail: Some(detail),
            unreachable,
            state: session.state,
            now_epoch_seconds,
            attention: dashboard.attention_level(&session.id),
        }
        .clock(detailed),
    )
}

/// Select only content authored by the agent for the expanded output rows.
/// The user prompt is a fallback for the compact summary, never an agent
/// excerpt with a misleading prefix.
pub(crate) fn current_agent_excerpt(detail: &SessionDetail) -> Option<&str> {
    if detail.last_agent_message_follows_last_user {
        detail
            .last_agent_message
            .as_deref()
            .or(detail.latest_agent_activity_after_last_user.as_deref())
    } else {
        detail.latest_agent_activity_after_last_user.as_deref()
    }
}

/// The target label shown for each session, in `ordered_sessions()` order.
///
/// A target repeated inside one project is ambiguous on its own, so repeats
/// are numbered `[1]`, `[2]`, … in the order they appear. Every Sessions
/// representation reads from this so labels remain consistent.
pub(crate) fn session_display_targets(
    dashboard: &DashboardState,
    sessions: &[&SessionRecord],
) -> Vec<String> {
    let mut counts = BTreeMap::<(String, String), usize>::new();
    for session in sessions {
        let key = (
            dashboard.project_source(session).key,
            session_target_label(
                &dashboard.state,
                session,
                dashboard.session_operations.get(&session.id),
                &dashboard.config,
            ),
        );
        *counts.entry(key).or_default() += 1;
    }
    let mut occurrences = BTreeMap::<(String, String), usize>::new();
    sessions
        .iter()
        .map(|session| {
            let base = session_target_label(
                &dashboard.state,
                session,
                dashboard.session_operations.get(&session.id),
                &dashboard.config,
            );
            let key = (dashboard.project_source(session).key, base.clone());
            let occurrence = occurrences.entry(key.clone()).or_default();
            *occurrence += 1;
            if counts.get(&key).copied().unwrap_or_default() > 1 {
                format!("{base} [{}]", *occurrence)
            } else {
                base
            }
        })
        .collect()
}

/// Content rows the Sessions pane wants, excluding its border.
pub(crate) fn sessions_content_height(dashboard: &DashboardState, width: u16) -> u16 {
    drawn_session_rows(dashboard, width)
        .iter()
        .map(|row| row.content_height().saturating_add(row.spacing))
        .fold(0, u16::saturating_add)
        .saturating_add(SESSION_ACTIONS_HEIGHT)
}

pub(crate) fn minimized_sessions_content_height(dashboard: &DashboardState, width: u16) -> u16 {
    drawn_session_rows_with_options(dashboard, width, SessionRowsRenderOptions::MINIMIZED)
        .iter()
        .map(|row| row.content_height().saturating_add(row.spacing))
        .fold(0, u16::saturating_add)
}

/// The pane name the title carries when there is room for it beside a label.
const FULL_TITLE_PREFIX: &str = " Sessions · ";

/// Whether a label is short enough for the pane to keep its full name rather
/// than falling back to `S · `. Callers that want to add to the label ask this
/// first, so the pane name never loses its place to something optional.
pub(crate) fn label_keeps_full_prefix(label: &str, width: u16, maximize_enabled: bool) -> bool {
    let budget = usize::from(pane_title_content_width(width, maximize_enabled));
    FULL_TITLE_PREFIX.chars().count() + label.chars().count() < budget
}

/// The Sessions title keeps the workspace ahead of the long pane label when
/// the screen is narrow, while visible size controls retain their cells.
pub(crate) fn sessions_title(
    workspace_name: &str,
    width: u16,
    maximize_enabled: bool,
) -> Line<'static> {
    let budget = usize::from(pane_title_content_width(width, maximize_enabled));
    if workspace_name.is_empty() {
        return Line::raw(truncate_to_cells(" Sessions ", budget, Truncate::SUMMARY));
    }
    let prefix = if label_keeps_full_prefix(workspace_name, width, maximize_enabled) {
        FULL_TITLE_PREFIX
    } else {
        " S · "
    };
    let workspace_room = budget.saturating_sub(prefix.chars().count() + 1);
    Line::from(vec![
        Span::raw(prefix),
        Span::styled(
            truncate_to_cells(workspace_name, workspace_room, Truncate::SUMMARY),
            Style::default().fg(theme::palette().muted),
        ),
        Span::raw(" "),
    ])
}

/// Reserve a title suffix for the sessions that want a person. At narrow
/// widths a compact form keeps that count visible while preserving the
/// Sessions label; the narrowest form is the most urgent level's own glyph.
pub(crate) fn sessions_title_with_attention(
    workspace_name: &str,
    width: u16,
    badge: Option<(AttentionLevel, usize)>,
    maximize_enabled: bool,
) -> Line<'static> {
    let base = sessions_title(workspace_name, width, maximize_enabled);
    let Some((level, count)) = badge else {
        return base;
    };
    let budget = usize::from(pane_title_content_width(width, maximize_enabled));
    let style = Style::default()
        .fg(attention_color(level))
        .add_modifier(Modifier::BOLD);
    let glyph = attention_glyph(level);
    let suffix = Span::styled(format!(" · Attention: {count}"), style);
    if base.width().saturating_add(suffix.width()) <= budget {
        let mut spans = base.spans;
        spans.push(suffix);
        return Line::from(spans);
    }
    let compact = Line::styled(format!(" Sessions [{glyph}{count}]"), style);
    if compact.width() <= budget {
        return compact;
    }
    let tiny = Line::styled(format!(" {glyph}{count}"), style);
    if tiny.width() <= budget {
        return tiny;
    }
    base
}

pub(crate) fn sessions_block(
    focused: bool,
    workspace_name: &str,
    width: u16,
    size: PaneSize,
    badge: Option<(AttentionLevel, usize)>,
    maximize_enabled: bool,
) -> Block<'static> {
    theme::panel(focused)
        .title(sessions_title_with_attention(
            workspace_name,
            width,
            badge.filter(|_| size == PaneSize::Minimized),
            maximize_enabled,
        ))
        .title(pane_size_controls(size, maximize_enabled))
}

/// The glyph that stands for an attention level wherever it is named: a row
/// symbol, a badge, or a pane title.
pub(crate) fn attention_glyph(level: AttentionLevel) -> &'static str {
    let glyphs = theme::glyphs();
    match level {
        AttentionLevel::Failed => glyphs.failed,
        AttentionLevel::Unreachable => glyphs.unreachable,
        AttentionLevel::Waiting => glyphs.waiting,
        AttentionLevel::Unread => glyphs.unread,
        AttentionLevel::Working => glyphs.working,
        AttentionLevel::Idle | AttentionLevel::Inactive => glyphs.idle,
    }
}

/// The colour an attention level carries: red for something broken, the
/// attention colour for something that is only waiting on a person.
pub(crate) fn attention_color(level: AttentionLevel) -> Color {
    match level {
        AttentionLevel::Failed | AttentionLevel::Unreachable => theme::palette().session_error,
        AttentionLevel::Waiting | AttentionLevel::Unread => theme::palette().session_attention,
        AttentionLevel::Working | AttentionLevel::Inactive => theme::palette().session_activity,
        AttentionLevel::Idle => theme::palette().session_idle,
    }
}

/// The ` ×1` or ` ✓3` a folded heading, a workspace tab, or the footer
/// carries: the most urgent unseen level's glyph and its session count.
pub(crate) fn attention_badge(summary: Option<(AttentionLevel, usize)>) -> Option<Span<'static>> {
    let (level, count) = summary?;
    Some(Span::styled(
        format!(" {}{count}", attention_glyph(level)),
        Style::default()
            .fg(attention_color(level))
            .add_modifier(Modifier::BOLD),
    ))
}

/// Draws the Sessions pane and reports the per-row mouse hitboxes.
pub(crate) fn render_sessions(
    frame: &mut Frame,
    area: Rect,
    dashboard: &DashboardState,
) -> SessionRowsRendered {
    let content = area.inner(Margin {
        horizontal: 1,
        vertical: 1,
    });
    // Keep three cells at the right edge clear for the session ellipsis
    // control on each session's title line. The activity and output lines
    // use the full content width; a narrow sidebar must not hide their clock
    // or queued count behind the action button.
    let actions_area = Rect::new(
        content.x,
        content.y,
        content.width,
        content.height.min(SESSION_ACTIONS_HEIGHT),
    );
    let rows_area = Rect::new(
        content.x,
        content.y.saturating_add(SESSION_ACTIONS_HEIGHT),
        content.width,
        content.height.saturating_sub(SESSION_ACTIONS_HEIGHT),
    );
    let width = content.width;
    let drawn = if dashboard.sessions_minimized() {
        drawn_session_rows_with_options(dashboard, width, SessionRowsRenderOptions::MINIMIZED)
    } else {
        drawn_session_rows(dashboard, width)
    };
    let focused = dashboard.focus() == Focus::Sessions;
    let maximize_enabled = dashboard.pane_maximize_enabled(SupportPane::Sessions);
    let filter_label = dashboard.sessions_filter_label();
    // The count is the first thing to give up its place. Below the width that
    // keeps both, the pane name is worth more than the number.
    let filter_label = match dashboard.sessions_hidden_count() {
        0 => filter_label,
        hidden => {
            let counted = format!("{filter_label} · {hidden} hidden");
            if label_keeps_full_prefix(&counted, area.width, maximize_enabled) {
                counted
            } else {
                filter_label
            }
        }
    };
    frame.render_widget(
        sessions_block(
            focused,
            &filter_label,
            area.width,
            dashboard.pane_size(SupportPane::Sessions),
            dashboard.sessions_attention_summary(),
            maximize_enabled,
        ),
        area,
    );
    crate::surface_controls::render_session_buttons(frame, actions_area, dashboard);
    let table = Table::new(
        drawn.iter().map(|row| {
            Row::new([Cell::from(Text::from(row.lines.clone()))])
                .height(row.content_height())
                .bottom_margin(row.spacing)
        }),
        [Constraint::Min(1)],
    );
    let selected = dashboard
        .selected_visible_index()
        .filter(|index| *index < drawn.len());
    let mut offset = dashboard.sessions_scroll.get();
    if let (Some(selected), Some(direction)) =
        (selected, take_scroll_lookahead(dashboard, Focus::Sessions))
    {
        let row_heights = drawn
            .iter()
            .map(|row| usize::from(row.content_height().saturating_add(row.spacing)))
            .collect::<Vec<_>>();
        offset = offset_with_directional_lookahead(
            offset,
            selected,
            direction,
            &row_heights,
            usize::from(rows_area.height),
        );
    }
    let mut state = TableState::default()
        .with_offset(offset)
        .with_selected(selected);
    frame.render_stateful_widget(table, rows_area, &mut state);
    if drawn.is_empty() && dashboard.sessions_filter.is_some() && rows_area.height > 0 {
        frame.render_widget(
            Paragraph::new("No sessions match · Esc clears the filter")
                .style(theme::muted())
                .wrap(ratatui::widgets::Wrap { trim: true }),
            rows_area,
        );
    }
    // The table scrolled only as far as it had to; remember where it settled
    // so the next frame does not scroll back to the top.
    dashboard.sessions_scroll.set(state.offset());

    let offset = state.offset();
    let mut row_y = rows_area.y;
    let mut visible = 0;
    let mut session_row_areas = Vec::new();
    let mut project_heading_areas = Vec::new();
    for row in drawn.iter().skip(offset) {
        if row_y >= rows_area.bottom() {
            break;
        }
        visible += 1;
        let heading_rows = u16::from(row.heading.is_some());
        if let Some(key) = row.heading.clone() {
            project_heading_areas.push((key, Rect::new(rows_area.x, row_y, rows_area.width, 1)));
        }
        if let Some(index) = row.session {
            let session_y = row_y.saturating_add(heading_rows);
            let height = row.content_height().saturating_sub(heading_rows);
            session_row_areas.push((
                index,
                Rect::new(
                    rows_area.x,
                    session_y,
                    rows_area.width,
                    height.min(rows_area.bottom().saturating_sub(session_y)),
                ),
            ));
        }
        row_y = row_y.saturating_add(row.content_height().saturating_add(row.spacing));
    }
    render_session_scrollbar(frame, area, drawn.len(), offset, visible);

    SessionRowsRendered {
        session_row_areas,
        project_heading_areas,
    }
}

/// The single row used while a lifecycle owns a session. It deliberately
/// contains no transcript excerpt: the identity, operation, active stages,
/// and elapsed time remain stable across expanded, collapsed, and minimized
/// layouts while another session can still be selected and used.
#[allow(clippy::too_many_arguments)]
pub(crate) fn session_transition_line(
    prefix: &str,
    session: &SessionRecord,
    transition: SessionTransitionKind,
    operation: Option<&SessionOperationDisplay>,
    now_epoch_seconds: u64,
    target: &str,
    width: u16,
    failure: Option<&str>,
) -> Line<'static> {
    let started_at = operation
        .map(|operation| {
            operation
                .active_stages
                .values()
                .copied()
                .min()
                .unwrap_or(operation.started_at_epoch_seconds)
        })
        .or_else(|| session_updated_at_epoch_seconds(session))
        .unwrap_or(now_epoch_seconds);
    let stages = operation
        .map(|operation| {
            operation
                .active_stages
                .keys()
                .map(|stage| stage.label())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .filter(|stages| !stages.is_empty())
        .unwrap_or_else(|| "waiting".to_owned());
    let elapsed =
        mj_client::usage_format::format_clock(now_epoch_seconds.saturating_sub(started_at));
    let (profile, _) = operation
        .and_then(|operation| operation.resume_destination.clone())
        .unwrap_or_else(|| {
            (
                session.last_profile.clone(),
                session.target_template_id.clone(),
            )
        });
    // Lead with the name, as every other session row does. A transition row
    // is a single line, so anything after it is what truncation drops first;
    // sessions starting together on one target differ only by their names.
    // Keep the name to half the row so the stage and clock always survive.
    let content_width = usize::from(width.saturating_sub(3)).saturating_sub(prefix.chars().count());
    let name = truncate_to_cells(
        session_name(session),
        (content_width / 2).max(12),
        Truncate::PLAIN,
    );
    let line = format!(
        "{prefix}{name}  {} · {stages} · {elapsed}  {target} · {profile}{}",
        transition.label(),
        failure.map_or_else(String::new, |error| format!(" · failed: {error}"))
    );
    Line::styled(
        truncate_to_cells(
            &line,
            usize::from(width.saturating_sub(3)),
            Truncate::SUMMARY,
        ),
        Style::default()
            .fg(if failure.is_some() {
                theme::palette().session_error
            } else {
                theme::palette().session_activity
            })
            .add_modifier(Modifier::BOLD),
    )
}

/// A session the dashboard has heard nothing operational about yet.
pub(crate) static EMPTY_ACTIVITY: std::sync::LazyLock<mj_client::usage_format::SessionActivity> =
    std::sync::LazyLock::new(mj_client::usage_format::SessionActivity::default);

/// The target a session row names, as `<target>/<project>` where the target
/// runs the project directly.
///
/// The target id is the session's own, but the project comes from whichever
/// session owns the project identity: a sub-agent child works in its parent's
/// checkout, so naming it from its own record would label every child with
/// the parent's session id.
pub(crate) fn session_target_label(
    state: &State,
    session: &SessionRecord,
    operation: Option<&SessionOperationDisplay>,
    config: &Config,
) -> String {
    let target_id = operation
        .and_then(|operation| operation.resume_destination.as_ref())
        .map(|(_, target_id)| target_id)
        .unwrap_or(&session.target_template_id);
    state
        .project_identity_session(session)
        .project_target(config, target_id)
}

pub(crate) fn session_permission_badge(
    session: &SessionRecord,
    operation: Option<&SessionOperationDisplay>,
    config: &Config,
) -> Option<Span<'static>> {
    let target_id = operation
        .and_then(|operation| operation.resume_destination.as_ref())
        .map(|(_, target_id)| target_id)
        .unwrap_or(&session.target_template_id);
    config
        .targets
        .get(target_id)
        .and_then(|target| permission_badge(target.permission_mode()))
}

pub(crate) fn permission_badge(mode: Option<PermissionMode>) -> Option<Span<'static>> {
    mode.map(|mode| match mode {
        PermissionMode::Guardian => Span::styled(
            "[G]",
            Style::default()
                .fg(theme::palette().success)
                .add_modifier(Modifier::BOLD),
        ),
        PermissionMode::Yolo => Span::styled(
            "[Y]",
            Style::default()
                .fg(theme::palette().error)
                .add_modifier(Modifier::BOLD),
        ),
    })
}

pub(crate) fn capacity_target_labels(
    target_ids: &[String],
    dashboard: &DashboardState,
) -> Line<'static> {
    let config = &dashboard.config;
    let mut spans = Vec::new();
    for (index, target_id) in target_ids.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw(", "));
        }
        if dashboard.target_known_unavailable(target_id) {
            spans.push(Span::styled(
                format!("{target_id} (unavailable)"),
                Style::default().fg(theme::palette().muted),
            ));
            continue;
        }
        spans.push(Span::raw(target_id.clone()));
        if let Some(badge) = config
            .targets
            .get(target_id)
            .and_then(|target| permission_badge(target.permission_mode()))
        {
            spans.push(Span::raw(" "));
            spans.push(badge);
        }
    }
    Line::from(spans)
}

pub(crate) fn render_session_scrollbar(
    frame: &mut Frame,
    area: Rect,
    content_length: usize,
    position: usize,
    viewport_content_length: usize,
) {
    if area.width == 0 || content_length <= viewport_content_length {
        return;
    }
    let track = Rect::new(
        area.right().saturating_sub(1),
        area.y.saturating_add(1),
        1,
        area.height.saturating_sub(2),
    );
    if let Some(geometry) =
        scrollbar_geometry(track, content_length, position, viewport_content_length)
    {
        render_scrollbar(frame, geometry);
    }
}

pub(crate) fn operation_status(operation: &SessionOperationDisplay) -> (String, u64) {
    if matches!(
        operation.kind,
        SessionOperationKind::Launching
            | SessionOperationKind::Resuming
            | SessionOperationKind::Moving
    ) && !operation.active_stages.is_empty()
    {
        let label = operation
            .active_stages
            .keys()
            .map(|stage| stage.label())
            .collect::<Vec<_>>()
            .join(", ");
        let started_at = operation
            .active_stages
            .values()
            .copied()
            .min()
            .unwrap_or(operation.started_at_epoch_seconds);
        (label, started_at)
    } else {
        (
            operation.kind.label().to_owned(),
            operation.started_at_epoch_seconds,
        )
    }
}

pub(crate) fn session_updated_at_epoch_seconds(session: &SessionRecord) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(&session.updated_at)
        .ok()?
        .timestamp()
        .try_into()
        .ok()
}

pub(crate) fn session_name(session: &SessionRecord) -> &str {
    session.display_title()
}

/// Maps the controller's review projection to the short overlay that fits in
/// every session row. The controller owns the detailed wording and verdict;
/// the TUI only compresses that authoritative view for the list.
pub(crate) fn review_status_label(review: Option<&RuntimeReviewView>) -> Option<&'static str> {
    review.and_then(RuntimeReviewView::activity_label)
}

/// The colour a session's summary rows carry.
///
/// A session that wants a person takes that level's colour, so the band and
/// the row symbol always agree: red for a failure or an unreachable worker,
/// the attention colour for a question or unread output. What is left is a
/// live session: blue when it is genuinely idle, amber while it or its
/// lifecycle is busy.
pub(crate) fn session_band_color(
    level: AttentionLevel,
    detail: Option<&SessionDetail>,
    state: SessionState,
) -> Color {
    if level == AttentionLevel::Idle {
        // Without a detail projection there is nothing to call idle yet.
        return if state == SessionState::Running
            && detail.is_some_and(|detail| detail.activity.is_idle(detail.current_turn_started_at))
        {
            theme::palette().session_idle
        } else {
            theme::palette().session_activity
        };
    }
    attention_color(level)
}

pub(crate) fn checkpoint_age(now_epoch_seconds: u64, checkpointed_at: &str) -> String {
    let Ok(checkpointed_at) = chrono::DateTime::parse_from_rfc3339(checkpointed_at) else {
        return "unknown".into();
    };
    let checkpointed_at = checkpointed_at.timestamp().max(0) as u64;
    let age = now_epoch_seconds.saturating_sub(checkpointed_at);
    if age < 60 {
        format!("{age}s")
    } else if age < 3_600 {
        format!("{}m", age / 60)
    } else if age < 86_400 {
        format!("{}h", age / 3_600)
    } else {
        format!("{}d", age / 86_400)
    }
}

pub(crate) fn recovery_warning_name(
    session: &SessionRecord,
    name: String,
    now_epoch_seconds: u64,
) -> String {
    if session.last_checkpoint_error.is_none() {
        return name;
    }
    match &session.checkpoint {
        Some(checkpoint) => format!(
            "{name}  {} Recovery copy {} old",
            theme::glyphs().warning,
            checkpoint_age(now_epoch_seconds, &checkpoint.created_at)
        ),
        None => format!("{name}  {} Recovery unavailable", theme::glyphs().warning),
    }
}
