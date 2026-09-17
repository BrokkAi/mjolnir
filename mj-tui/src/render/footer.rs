use super::*;

/// Registry commands shared by pane and composer footers, retaining their identities.
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
            let hint = spec.keys.first()?;
            Some((spec.footer_rank, id, format!("{} {word}", hint.label)))
        })
        .collect::<Vec<_>>();
    hints.sort_by_key(|(rank, _, _)| *rank);
    hints.into_iter().map(|(_, id, text)| (id, text)).collect()
}

/// The hotkey hints for whatever applies right now.
///
/// Built from the action registry ([`crate::actions`]) rather than written out
/// as a string per pane, so a hint can never name a key the surface does not
/// answer, and a command can never be added without the footer knowing.
///
/// The row is three groups separated by a vertical bar, always in this order:
/// what the focused pane answers, the `Alt` chords that answer from anywhere,
/// and the function keys. The reader therefore always looks in the same place
/// for a given kind of key, and the row does not reshuffle itself as the pane
/// changes. Each group's order comes from `footer_group` and `footer_rank` on
/// the spec, not from where the command sits in the table.
///
/// `width` is the row's width in cells. When the hints do not fit, whole
/// segments are dropped from the right — never truncated mid-word, because
/// half a hint names a key that does not exist — first from the pane group,
/// then from the chords. Function keys give way last, with palette and help
/// retained longest so the user can find everything the narrow row leaves out.
///
/// The composer's own hints come from the chat itself, because they depend on
/// what it is doing (a queued prompt, dictation, a history search); this text
/// is only drawn when a pane has the keyboard.
pub(crate) fn combined_footer_text(dashboard: &DashboardState, width: u16) -> String {
    let groups = [
        footer_commands(dashboard, crate::actions::FooterGroup::Pane),
        footer_commands(dashboard, crate::actions::FooterGroup::Chord),
        footer_commands(dashboard, crate::actions::FooterGroup::Function),
    ];
    let groups = theme::fit_footer_items(groups, width, |(_, text)| text.as_str());
    theme::footer_items_text(&groups, |(_, text)| text.as_str())
}

/// Draws the shared footer row.
///
/// A notice replaces the hints while one is showing, the same way the
/// composer's own footer works, so the two are interchangeable and the row
/// costs one line whichever surface drew it.
pub(crate) fn render_footer(frame: &mut Frame, area: Rect, dashboard: &DashboardState) {
    let notice = dashboard.notices.current();
    let groups = [
        footer_commands(dashboard, crate::actions::FooterGroup::Pane),
        footer_commands(dashboard, crate::actions::FooterGroup::Chord),
        footer_commands(dashboard, crate::actions::FooterGroup::Function),
    ];
    let groups = theme::fit_footer_items(groups, area.width, |(_, text)| text.as_str());
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
                x = x.saturating_add(Line::raw(theme::FOOTER_GROUP_SEPARATOR).width() as u16);
            }
            for (index, (id, text)) in group.iter().enumerate() {
                if index > 0 {
                    x = x.saturating_add(Line::raw(theme::FOOTER_SEPARATOR).width() as u16);
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
