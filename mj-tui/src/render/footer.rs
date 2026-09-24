use super::*;

/// Registry commands shared by pane and composer footers, retaining their identities.
///
/// The chord group comes back without the prefix: the fitter puts
/// [`chord_prefix`] on the first chord that survives — `ctrl+b then c
/// create` — so the reader always sees which key starts the chord.
pub(crate) fn footer_commands(
    dashboard: &DashboardState,
    group: crate::actions::FooterGroup,
) -> Vec<(crate::CommandId, String)> {
    let mut hints = crate::actions::available(dashboard, None)
        .into_iter()
        .filter_map(|id| {
            let spec = crate::actions::spec(id);
            if spec.footer_group != group {
                return None;
            }
            let word = (spec.footer)(dashboard)?;
            let key = dashboard.footer_key(id)?;
            Some((spec.footer_rank, id, format!("{key} {word}")))
        })
        .collect::<Vec<_>>();
    hints.sort_by_key(|(rank, _, _)| *rank);
    hints.into_iter().map(|(_, id, text)| (id, text)).collect()
}

/// The words that lead the chord group: `ctrl+b then `. No colon follows
/// "then", because the palette's own chord key is `:` and `then: : palette`
/// reads as a doubled colon.
pub(crate) fn chord_prefix(dashboard: &DashboardState) -> String {
    format!("{} then ", dashboard.keybinds().prefix_label())
}

/// The footer hints that survive longest when the row runs out of width: a
/// narrow terminal must still say how to reach everything it left out.
pub(crate) fn protected_hint(id: &crate::CommandId) -> bool {
    matches!(id, crate::CommandId::Palette | crate::CommandId::Help)
}

/// The two groups the dashboard footer draws. The third slot exists because
/// the shared fitting helper takes three groups; the dashboard has no separate
/// function-key group any more.
fn footer_groups(dashboard: &DashboardState) -> [Vec<(crate::CommandId, String)>; 3] {
    [
        footer_commands(dashboard, crate::actions::FooterGroup::Pane),
        footer_commands(dashboard, crate::actions::FooterGroup::Chord),
        Vec::new(),
    ]
}

/// The groups that fit in `width`, with the prefix still named on the first
/// chord that survived (see [`theme::fit_prefixed_footer_items`]).
fn fitted_footer_groups(
    dashboard: &DashboardState,
    width: u16,
) -> [Vec<(crate::CommandId, String)>; 3] {
    theme::fit_prefixed_footer_items(
        footer_groups(dashboard),
        width,
        &chord_prefix(dashboard),
        protected_hint,
    )
}

/// The hotkey hints for whatever applies right now.
///
/// Built from the action registry ([`crate::actions`]) rather than written out
/// as a string per pane, so a hint can never name a key the surface does not
/// answer, and a command can never be added without the footer knowing.
///
/// The row is two groups separated by a vertical bar, always in this order:
/// what the focused pane answers, then the prefix chords that answer from
/// anywhere. The reader therefore always looks in the same place for a given
/// kind of key, and the row does not reshuffle itself as the pane changes.
/// Each group's order comes from `footer_group` and `footer_rank` on the spec,
/// not from where the command sits in the table.
///
/// `width` is the row's width in cells. When the hints do not fit, whole
/// segments are dropped from the right — never truncated mid-word, because
/// half a hint names a key that does not exist — first from the chords, then
/// from the pane group, with palette and help retained longest so the user can
/// find everything the narrow row leaves out. The pane group is what works
/// right here and holds three entries; the chord group holds twelve, and the
/// palette lists all of them, so a narrow row keeps the three and gives up the
/// twelve.
///
/// The composer's own hints come from the chat itself, because they depend on
/// what it is doing (a queued prompt, dictation, a history search); this text
/// is only drawn when a pane has the keyboard.
pub(crate) fn combined_footer_text(dashboard: &DashboardState, width: u16) -> String {
    let groups = fitted_footer_groups(dashboard, width);
    theme::footer_items_text(&groups, |(_, text)| text.as_str())
}

/// Draws the shared footer row.
///
/// A notice replaces the hints while one is showing, the same way the
/// composer's own footer works, so the two are interchangeable and the row
/// costs one line whichever surface drew it.
pub(crate) fn render_footer(frame: &mut Frame, area: Rect, dashboard: &DashboardState) {
    // A half-typed chord owns the row: what the reader needs there is the way
    // out of it, not the hints they are already part-way through.
    if dashboard.prefix_pending() {
        frame.render_widget(
            Paragraph::new(prefix_banner_line(dashboard))
                .style(theme::muted().bg(theme::palette().surface)),
            area,
        );
        return;
    }
    if dashboard.resize_mode_active() {
        frame.render_widget(
            Paragraph::new(resize_banner_line()).style(theme::muted().bg(theme::palette().surface)),
            area,
        );
        return;
    }
    let notice = dashboard.notices.current();
    let groups = fitted_footer_groups(dashboard, area.width);
    let line = match notice.as_deref() {
        Some(notice) => Line::styled(
            notice.to_owned(),
            Style::default().fg(theme::palette().warning),
        ),
        None => theme::hints(&combined_footer_text(dashboard, area.width)),
    };
    frame.render_widget(
        Paragraph::new(line).style(theme::muted().bg(theme::palette().surface)),
        area,
    );
    // The dashboard has no active composer to register these controls for us.
    // Register the same fitted segments that were drawn so a footer click
    // dispatches the command represented by that exact hint.
    if notice.is_none() {
        let mut x = area.x;
        for group in groups.iter().filter(|group| !group.is_empty()) {
            if x > area.x {
                x = x.saturating_add(Line::raw(theme::footer_group_separator()).width() as u16);
            }
            for (index, (id, text)) in group.iter().enumerate() {
                if index > 0 {
                    x = x.saturating_add(Line::raw(theme::footer_separator()).width() as u16);
                }
                let width = Line::raw(text.as_str()).width() as u16;
                crate::surface_controls::render_footer_command(
                    frame,
                    Rect::new(
                        x,
                        area.y,
                        width.min(area.right().saturating_sub(x)),
                        area.height,
                    ),
                    dashboard,
                    *id,
                    text,
                );
                x = x.saturating_add(width);
            }
        }
    }
}

pub(crate) fn resize_banner_line() -> Line<'static> {
    Line::raw("Resize panes: h/j/k/l or arrows · Esc done")
}

/// The `PREFIX` banner with this dashboard's live prefix and help key.
pub(crate) fn prefix_banner_line(dashboard: &DashboardState) -> Line<'static> {
    theme::prefix_banner(
        &dashboard.keybinds().prefix_label(),
        &dashboard
            .footer_key(crate::CommandId::Help)
            .unwrap_or_else(|| "?".to_owned()),
    )
}

pub(crate) fn refresh_age(now: u64, refreshed: u64) -> String {
    if refreshed == 0 {
        return "unknown".into();
    }
    let age = now.saturating_sub(refreshed);
    let (value, unit) = if age < 60 {
        (age, "s")
    } else if age < 3_600 {
        (age / 60, "m")
    } else {
        (age / 3_600, "h")
    };
    format!("{value}{unit} ago")
}
