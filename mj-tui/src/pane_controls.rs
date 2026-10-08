//! Pin placement and pane menus share the dashboard's component event model.
use super::*;
use crate::actions::CommandId;
use crate::surface_controls::SurfaceControl;
use crate::tile_layout::PaneId;
use mj_chat::components::{ChoiceList, ControlKind, Dialog, Interaction, ListActivation};
use mj_chat::theme;
use ratatui::{
    Frame,
    layout::Direction,
    style::{Modifier, Style},
    text::Line,
    widgets::Paragraph,
};

#[derive(Debug, Clone, PartialEq, Eq)]
enum PaneOperation {
    Split(PaneId, Direction),
    PinSplit(String, Direction),
    PinHere(String, PaneId),
    ChooseEmpty(String),
    Unpin(String),
    Swap(PaneId, PaneId),
    Zoom(PaneId),
    Close(PaneId),
    /// Runs one registry command, exactly as its key or palette row does.
    Command(CommandId),
    /// An informational row in the read-only capacity dropdown.
    Information,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PaneMenu {
    title: String,
    entries: Vec<(String, PaneOperation)>,
    form: RefCell<Dialog<usize>>,
    destinations: bool,
    pressed_destination: Option<usize>,
    /// Where the menu was last drawn. The empty-pane chooser lies inside a
    /// destination pane, so a click on the menu must not count as a click
    /// on the pane beneath it.
    popup: std::cell::Cell<Rect>,
    /// The title a dropdown hangs from. `None` centres the menu.
    anchor: Option<Rect>,
    /// Styled, non-command rows used by the capacity details dropdown.
    display_rows: Option<Vec<Line<'static>>>,
    /// Target whose Disks cell opened this menu, if any.
    capacity_target: Option<String>,
}

impl DashboardState {
    fn show_pane_menu(
        &mut self,
        title: &str,
        entries: Vec<(String, PaneOperation)>,
        destinations: bool,
    ) {
        let mut form = Dialog::default();
        form.register(
            0,
            ControlKind::ChoiceList {
                len: entries.len(),
                selected: 0,
            },
            Rect::default(),
            true,
        );
        form.set_menu(true);
        form.set_list_activation(0, ListActivation::SingleClick);
        form.end_frame(0);
        self.cancel_component_pointer();
        self.pane_menu = Some(PaneMenu {
            title: title.to_owned(),
            entries,
            form: RefCell::new(form),
            destinations,
            pressed_destination: None,
            popup: std::cell::Cell::new(Rect::default()),
            anchor: None,
            display_rows: None,
            capacity_target: None,
        });
    }

    /// Opens the selected capacity row's filesystem details beneath its
    /// Disks cell, using the rendered button geometry as the anchor.
    pub(crate) fn begin_capacity_disks_menu(&mut self, index: usize) {
        let Some(anchor) = self
            .capacity_disks_areas
            .borrow()
            .get(index)
            .copied()
            .flatten()
        else {
            return;
        };
        let Some((target_id, rows)) = self.capacity_details.values().nth(index).map(|detail| {
            (
                detail.target.id.clone(),
                crate::render::capacity_disks_menu_lines(self, detail),
            )
        }) else {
            return;
        };
        if rows.is_empty() {
            return;
        }
        let entries = rows
            .iter()
            .map(|line| (line.to_string(), PaneOperation::Information))
            .collect();
        self.show_pane_menu("Disks", entries, false);
        if let Some(menu) = self.pane_menu.as_mut() {
            menu.anchor = Some(anchor);
            menu.display_rows = Some(rows);
            menu.capacity_target = Some(target_id);
        }
        self.focus = Focus::Targets;
        self.set_session_action_focus(None);
        self.capacity_index = index;
    }

    pub(crate) fn capacity_disks_menu_open(&self, target_id: &str) -> bool {
        self.pane_menu
            .as_ref()
            .is_some_and(|menu| menu.capacity_target.as_deref() == Some(target_id))
    }

    /// Opens the small menu that hangs from a support pane's title: Refresh,
    /// then the Setup pages for what the pane lists. Targets has Runtimes…
    /// and Machines…; Profiles has Settings…, the Agent Profiles page.
    /// Clicking the title opens it, and so does `.` while the pane has the
    /// keyboard. Sessions has its own buttons, so it has no title menu.
    pub(crate) fn begin_support_pane_menu(&mut self, pane: SupportPane) {
        let (index, title, settings): (usize, &str, &[(&str, CommandId)]) = match pane {
            SupportPane::Targets => (
                1,
                "Targets",
                &[
                    ("Runtimes…", CommandId::ManageTargets),
                    ("Machines…", CommandId::ManageMachines),
                    ("CPU by session…", CommandId::SessionCpuReport),
                ],
            ),
            SupportPane::Quota => (2, "Profiles", &[("Settings…", CommandId::ManageProfiles)]),
            SupportPane::Sessions => return,
        };
        let entries = std::iter::once(("Refresh", CommandId::Refresh))
            .chain(settings.iter().copied())
            .map(|(label, command)| (label.to_owned(), PaneOperation::Command(command)))
            .collect();
        self.show_pane_menu(title, entries, false);
        // The title's first cell: one in from the pane's left edge, after the
        // border's corner or the minimized row's rule.
        let anchor = self
            .pane_areas
            .map(|areas| areas[index])
            .filter(|area| area.width > 1 && area.height > 0)
            .map(|area| Rect::new(area.x + 1, area.y, 1, 1));
        if let Some(menu) = self.pane_menu.as_mut() {
            menu.anchor = anchor;
        }
    }

    pub(crate) fn begin_pin_menu(&mut self, session: String) {
        if !self.state.sessions.contains_key(&session) {
            return;
        }
        let mut entries = vec![
            (
                "Pin and browse right".into(),
                PaneOperation::PinSplit(session.clone(), Direction::Horizontal),
            ),
            (
                "Pin and browse down".into(),
                PaneOperation::PinSplit(session.clone(), Direction::Vertical),
            ),
        ];
        if self.empty_pin_panes().next().is_some() {
            entries.push((
                "Pin in empty pane…".into(),
                PaneOperation::ChooseEmpty(session),
            ));
        }
        self.show_pane_menu("Pin session", entries, false);
    }

    /// Whether the pane chrome puts "Pin selected here" on the pane's first
    /// inside row: an empty pane other than Browse. Anything drawn in the
    /// pane keeps clear of that row.
    pub(crate) fn pane_shows_pin_hint(&self, pane: PaneId) -> bool {
        self.pane_session(pane).is_none() && pane != self.browse_pane()
    }

    fn empty_pin_panes(&self) -> impl Iterator<Item = PaneId> + '_ {
        self.navigation
            .layout()
            .pane_ids()
            .into_iter()
            .filter(|pane| *pane != self.browse_pane() && self.pane_session(*pane).is_none())
    }

    pub(crate) fn toggle_session_pin(&mut self, session: String) -> DashboardAction {
        if self.pin_id(&session).is_some() {
            DashboardAction::UnpinSession {
                session_id: session,
            }
        } else {
            self.begin_pin_menu(session);
            DashboardAction::None
        }
    }

    pub(crate) fn begin_pane_menu(&mut self, pane: PaneId) {
        let mut entries = vec![
            (
                "Split right".into(),
                PaneOperation::Split(pane, Direction::Horizontal),
            ),
            (
                "Split down".into(),
                PaneOperation::Split(pane, Direction::Vertical),
            ),
        ];
        let focused = self.focused_pane();
        if focused != pane {
            entries.push((
                "Swap with focused pane".into(),
                PaneOperation::Swap(focused, pane),
            ));
        }
        if pane != self.browse_pane() {
            if let Some(session) = self.pane_session(pane) {
                entries.push(("Unpin".into(), PaneOperation::Unpin(session.to_owned())));
            } else if let Some(session) = self.selected_session_id() {
                entries.push((
                    "Pin selected here".into(),
                    PaneOperation::PinHere(session.to_owned(), pane),
                ));
            }
            entries.push(("Close pane".into(), PaneOperation::Close(pane)));
        }
        entries.push(("Zoom / unzoom".into(), PaneOperation::Zoom(pane)));
        self.show_pane_menu("Pane", entries, false);
    }

    pub(crate) fn handle_pane_menu_event(&mut self, event: Event) -> DashboardAction {
        self.last_event_consumed.set(true);
        if let Some(menu) = self.pane_menu.as_ref()
            && menu.capacity_target.is_some()
            && (matches!(
                &event,
                Event::Key(key)
                    if key.kind != KeyEventKind::Release && key.code == KeyCode::Char('d')
            ) || matches!(
                (&event, menu.anchor),
                (Event::Mouse(mouse), Some(anchor))
                    if mouse.kind == MouseEventKind::Down(MouseButton::Left)
                        && anchor.contains((mouse.column, mouse.row).into())
            ))
        {
            self.pane_menu = None;
            return DashboardAction::None;
        }
        let Some(menu) = self.pane_menu.as_mut() else {
            return DashboardAction::None;
        };
        let mut direct = None;
        if menu.destinations
            && let Event::Mouse(mouse) = &event
            && !rect_contains(menu.popup.get(), mouse.column, mouse.row)
        {
            let hit = menu
                .entries
                .iter()
                .position(|(_, operation)| match operation {
                    PaneOperation::PinHere(_, pane) => {
                        self.conversation_pane_areas
                            .iter()
                            .any(|(id, transcript, prompt)| {
                                id == pane
                                    && (rect_contains(*transcript, mouse.column, mouse.row)
                                        || rect_contains(*prompt, mouse.column, mouse.row))
                            })
                    }
                    _ => false,
                });
            if mouse.kind == MouseEventKind::Down(MouseButton::Left) && hit.is_some() {
                menu.pressed_destination = hit;
                return DashboardAction::None;
            }
            if mouse.kind == MouseEventKind::Up(MouseButton::Left)
                && let Some(pressed) = menu.pressed_destination.take()
            {
                if hit != Some(pressed) {
                    return DashboardAction::None;
                }
                direct = Some(pressed);
            }
        }
        if let Event::Key(key) = &event
            && key.kind != KeyEventKind::Release
            && let KeyCode::Char(c @ '1'..='9') = key.code
        {
            direct = Some(c as usize - '1' as usize);
        }
        let interaction = if direct.is_none() {
            menu.form.get_mut().handle(&event).action
        } else {
            None
        };
        if matches!(interaction, Some(Interaction::Cancel)) {
            self.pane_menu = None;
            return DashboardAction::None;
        }
        let selected = direct.or_else(|| {
            matches!(interaction, Some(Interaction::Activate(0)))
                .then(|| menu.form.borrow().selected(0).unwrap_or(0))
        });
        let Some(operation) = selected.and_then(|i| menu.entries.get(i).map(|(_, op)| op.clone()))
        else {
            return DashboardAction::None;
        };
        self.pane_menu = None;
        self.last_row_click = None;
        match operation {
            PaneOperation::Split(pane, direction) => {
                DashboardAction::SplitConversation { pane, direction }
            }
            PaneOperation::PinSplit(session_id, direction) => DashboardAction::OpenSessionInSplit {
                session_id,
                direction,
            },
            PaneOperation::PinHere(session_id, pane) => {
                DashboardAction::PinSession { session_id, pane }
            }
            PaneOperation::Unpin(session_id) => DashboardAction::UnpinSession { session_id },
            PaneOperation::ChooseEmpty(session) => {
                let entries = self
                    .empty_pin_panes()
                    .enumerate()
                    .map(|(i, pane)| {
                        (
                            format!("{}: Pin in empty pane", i + 1),
                            PaneOperation::PinHere(session.clone(), pane),
                        )
                    })
                    .collect();
                self.conversation_zoomed = false;
                self.show_pane_menu("Choose empty pane (click or number)", entries, true);
                DashboardAction::None
            }
            PaneOperation::Swap(source, target) => {
                self.swap_conversation_panes(source, target);
                DashboardAction::ConversationPanesChanged { focus_moved: false }
            }
            PaneOperation::Zoom(pane) => {
                self.focus_pane(pane);
                self.zoom_pane_command()
            }
            PaneOperation::Close(pane) => DashboardAction::ClosePane { pane },
            PaneOperation::Command(id) => self.run_available_command(id),
            PaneOperation::Information => DashboardAction::None,
        }
    }
}

const PANE_CHROME_SUFFIX: &str = "| Conversation ";

/// The pane's own name in its title row: Browse, its pin badge, or Empty.
fn pane_chrome_label(dashboard: &DashboardState, pane: PaneId) -> String {
    let badge = dashboard
        .pane_session(pane)
        .and_then(|id| dashboard.pin_id(id));
    if pane == dashboard.browse_pane() {
        "Browse".to_owned()
    } else if let Some(badge) = badge {
        format!("{} {}", theme::glyphs().pinned, theme::pin_label(badge))
    } else {
        "Empty".to_owned()
    }
}

/// How many columns at the left of a pane's title row `render_pane_chrome`
/// draws over, so the conversation drawn beneath can start its own title
/// after them instead of losing its first words.
pub(crate) fn pane_chrome_width(dashboard: &DashboardState, pane: PaneId, width: u16) -> u16 {
    if width < 12 {
        return 0;
    }
    let label = pane_chrome_label(dashboard, pane);
    let drawn = Line::from(format!(" {label} {PANE_CHROME_SUFFIX}")).width();
    u16::try_from(drawn).unwrap_or(u16::MAX).min(width - 12)
}

/// The title of a pane that shows a child's conversation read-only in place
/// of a chat (a native child, a stopped sub-agent): the child's name first,
/// then as many of its `status` words as fit, dropped from the end. A name
/// too long for the row alone ends in an ellipsis.
///
/// Like the chat title, it starts after the label `render_pane_chrome` draws
/// at the left of the title row and stops short of the chips at the right.
/// The native child's title started under the label, so the row read
/// "Browse | Conversation d · availability unknown" (R12-3).
pub(crate) fn child_pane_title(
    dashboard: &DashboardState,
    pane: PaneId,
    width: u16,
    name: &str,
    status: &[&str],
) -> Line<'static> {
    let lead = pane_chrome_width(dashboard, pane, width);
    let room = usize::from(
        width
            .saturating_sub(2)
            .saturating_sub(lead)
            .saturating_sub(pane_title_reserve(dashboard, width, 0)),
    );
    let fitted = (0..=status.len())
        .rev()
        .map(|shown| {
            let mut title = format!(" {name}");
            for word in &status[..shown] {
                title.push_str(" · ");
                title.push_str(word);
            }
            title.push(' ');
            Line::from(title)
        })
        .find(|title| title.width() <= room)
        .unwrap_or_else(|| {
            mj_chat::chat::truncate_line_to_width(Line::from(format!(" {name} ")), room)
        });
    let mut spans = vec![ratatui::text::Span::raw(" ".repeat(usize::from(lead)))];
    spans.extend(fitted.spans);
    Line::from(spans)
}

/// How many columns from the right of a pane's title row the pane chips
/// start at: the pin chip, then the menu chip, then room for the close chip.
const PANE_CHROME_CHIPS_RESERVE: u16 = 10;

/// The columns a pane's own title must leave clear at the right: the host's
/// `reserve` for its close and zoom chips, widened to clear the pin and menu
/// chips `render_pane_chrome` draws whenever the pane is wide enough for
/// them. A title that ran under a chip showed through its unpainted cells.
pub(crate) fn pane_title_reserve(dashboard: &DashboardState, width: u16, reserve: u16) -> u16 {
    if width < 12 {
        return reserve;
    }
    let zoom = if dashboard.conversation_zoomed() {
        3
    } else {
        0
    };
    reserve.max(PANE_CHROME_CHIPS_RESERVE + zoom)
}

pub(crate) fn render_pane_chrome(frame: &mut Frame, dashboard: &DashboardState) {
    for &(pane, transcript, _) in &dashboard.conversation_pane_areas {
        if transcript.width < 12 || transcript.height == 0 {
            continue;
        }
        let session = dashboard.pane_session(pane);
        let badge = session.and_then(|id| dashboard.pin_id(id));
        let label = pane_chrome_label(dashboard, pane);
        let style = badge.map_or_else(theme::muted, |id| Style::default().fg(theme::pin_color(id)));
        let badge_style = if dashboard.focus() == Focus::Sessions
            && session.is_some()
            && session == dashboard.selected_session_id()
        {
            style
                .add_modifier(ratatui::style::Modifier::BOLD | ratatui::style::Modifier::UNDERLINED)
        } else {
            style
        };
        // A label cut short ends in an ellipsis rather than a stray letter.
        let header = mj_chat::chat::truncate_line_to_width(
            Line::from(vec![
                ratatui::text::Span::styled(format!(" {label} "), badge_style),
                ratatui::text::Span::styled(PANE_CHROME_SUFFIX, theme::muted()),
            ]),
            usize::from(transcript.width.saturating_sub(12)),
        );
        frame.render_widget(
            Paragraph::new(header),
            Rect::new(
                transcript.x + 1,
                transcript.y,
                transcript.width.saturating_sub(12),
                1,
            ),
        );
        let mut form = dashboard.surface_form.borrow_mut();
        let zoom_reserve = if dashboard.conversation_zoomed() {
            3
        } else {
            0
        };
        for (control, offset, glyph) in [
            (SurfaceControl::PaneMenu(pane), 7, theme::glyphs().row_menu),
            (
                SurfaceControl::PanePin(pane),
                10,
                if badge.is_some() {
                    theme::glyphs().pinned
                } else {
                    theme::glyphs().pin
                },
            ),
        ] {
            let area = Rect::new(
                transcript.right().saturating_sub(offset + zoom_reserve),
                transcript.y,
                3,
                1,
            );
            form.register(control, ControlKind::Button, area, true);
            // Paint all three cells, so nothing underneath shows through.
            frame.render_widget(Paragraph::new(format!(" {glyph} ")).style(style), area);
        }
        if dashboard.pane_shows_pin_hint(pane) && transcript.height > 2 {
            let area = Rect::new(
                transcript.x + 1,
                transcript.y + 1,
                19.min(transcript.width - 2),
                1,
            );
            let control = SurfaceControl::PinHere(pane);
            form.register(
                control,
                ControlKind::Button,
                area,
                dashboard.selected_session_id().is_some(),
            );
            frame.render_widget(
                Paragraph::new("Pin selected here").style(theme::title(true)),
                area,
            );
        }
    }
}

/// Where a dropdown sits: under the title it hangs from, as wide as its
/// longest line needs. It opens upward when there is no room below, as for a
/// minimized pane, whose one row sits just above the footer.
fn dropdown_popup(
    area: Rect,
    anchor: Rect,
    title: &str,
    entries: &[(String, PaneOperation)],
) -> Rect {
    let content = entries
        .iter()
        .map(|(label, _)| Line::raw(label.as_str()).width())
        // The title sits on the top border between the two corners.
        .chain([Line::raw(title).width() + 2])
        .max()
        .unwrap_or(0);
    let width = u16::try_from(content + 4)
        .unwrap_or(u16::MAX)
        .max(16)
        .min(area.width);
    let wanted_height = (entries.len() as u16 + 2).min(area.height);
    let below = area.bottom().saturating_sub(anchor.bottom());
    let above = anchor.y.saturating_sub(area.y);
    let open_below = below >= wanted_height || below >= above;
    let height = wanted_height
        .min(if open_below { below } else { above })
        .max(1);
    let x = anchor.x.min(area.right().saturating_sub(width)).max(area.x);
    let y = if open_below {
        anchor.bottom()
    } else {
        anchor.y.saturating_sub(height)
    };
    Rect::new(x, y, width, height)
}

pub(crate) fn render_pane_menu(frame: &mut Frame, area: Rect, dashboard: &DashboardState) {
    let Some(menu) = &dashboard.pane_menu else {
        return;
    };
    let popup = if let Some(anchor) = menu.anchor {
        dropdown_popup(area, anchor, &menu.title, &menu.entries)
    } else if menu.destinations {
        // Open inside the first destination pane, under its "[1] Pin here"
        // marker, so the chooser sits beside the panes it names; without a
        // drawn pane, centre it like any other menu.
        let width = area.width.min(44);
        let height = area.height.min(menu.entries.len() as u16 + 2);
        let anchor = menu.entries.iter().find_map(|(_, op)| match op {
            PaneOperation::PinHere(_, pane) => dashboard
                .conversation_pane_areas
                .iter()
                .find(|(id, _, _)| id == pane)
                .map(|(_, transcript, _)| *transcript),
            _ => None,
        });
        match anchor {
            Some(transcript) => {
                let x = (transcript.x + 1).min(area.right().saturating_sub(width));
                let y = (transcript.y + 3).min(area.bottom().saturating_sub(height));
                Rect::new(x.max(area.x), y.max(area.y), width, height)
            }
            None => Rect::new(
                area.x + (area.width - width) / 2,
                area.y + (area.height - height) / 2,
                width,
                height,
            ),
        }
    } else {
        let width = area.width.min(44);
        let height = area.height.min(menu.entries.len() as u16 + 2);
        Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        )
    };
    if menu.destinations {
        for (index, (_, op)) in menu.entries.iter().enumerate() {
            if let PaneOperation::PinHere(_, pane) = op
                && let Some((_, transcript, _)) = dashboard
                    .conversation_pane_areas
                    .iter()
                    .find(|(id, _, _)| id == pane)
            {
                frame.render_widget(
                    Paragraph::new(format!(" [{}] Pin here ", index + 1))
                        .style(theme::selection(true)),
                    Rect::new(
                        transcript.x + 1,
                        transcript.y + 2,
                        transcript.width.saturating_sub(2),
                        1,
                    ),
                );
            }
        }
    }
    menu.popup.set(popup);
    let mut form = menu.form.borrow_mut();
    let selected = form.selected(0).unwrap_or(0);
    form.begin_frame();
    form.set_bounds(popup);
    let block = theme::modal().title(menu.title.clone());
    let inner = block.inner(popup);
    frame.render_widget(ratatui::widgets::Clear, popup);
    frame.render_widget(block, popup);
    if let Some(items) = &menu.display_rows {
        let selected = selected.min(items.len().saturating_sub(1));
        ChoiceList::render_with_rows(
            frame,
            inner,
            items,
            selected,
            &(0..items.len()).map(Some).collect::<Vec<_>>(),
            &vec![true; items.len()],
            &mut form,
            0,
        );
        form.set_menu(true);
        form.set_list_activation(0, ListActivation::DoubleClick);
        let offset = form.list_offset(0);
        let rows = items
            .iter()
            .cloned()
            .enumerate()
            .map(|(index, mut line)| {
                if index == selected {
                    line.style = line.style.patch(if theme::reverse_video() {
                        Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
                    } else {
                        Style::default()
                            .bg(theme::palette().selection)
                            .add_modifier(Modifier::BOLD)
                    });
                }
                line
            })
            .collect::<Vec<_>>();
        frame.render_widget(
            Paragraph::new(rows).scroll((u16::try_from(offset).unwrap_or(u16::MAX), 0)),
            inner,
        );
    } else {
        let items: Vec<Line> = menu
            .entries
            .iter()
            .map(|(label, _)| Line::raw(label))
            .collect();
        ChoiceList::render_with_rows(
            frame,
            inner,
            &items,
            selected,
            &(0..items.len()).map(Some).collect::<Vec<_>>(),
            &vec![true; items.len()],
            &mut form,
            0,
        );
        form.set_menu(true);
        form.set_list_activation(0, ListActivation::SingleClick);
    }
    form.end_frame(0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        dashboard_with_session, drawn, key, mouse_at, point, running_session,
    };
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn pane_menu_targets_the_clicked_pane_and_swaps_without_moving_browse_role() {
        let mut d = dashboard_with_session(running_session());
        d.conversation_area = Some(Rect::new(0, 0, 120, 40));
        let pin = d.browse_pane();
        d.set_pane_session(pin, Some("session-1"));
        let browse = d.split_focused_pane(Direction::Horizontal, None).unwrap();
        d.begin_pane_menu(pin);
        assert_eq!(
            d.handle_key(key(KeyCode::Enter)),
            DashboardAction::SplitConversation {
                pane: pin,
                direction: Direction::Horizontal
            }
        );
        d.begin_pane_menu(pin);
        d.handle_key(key(KeyCode::Char('3')));
        assert_eq!(d.browse_pane(), browse);
        assert_eq!(d.pane_session(pin), Some("session-1"));
        assert_eq!(d.pin_id("session-1"), Some(0));
    }

    /// Launch campaign finding D-10: the empty-pane chooser opens in the
    /// empty pane, under its "[1] Pin here" marker, not at the screen's
    /// top-left corner over the Workspaces pane.
    // Hard-won: 541cba84d488: the empty-pane chooser covered Workspaces from the screen origin.
    #[test]
    fn the_empty_pane_chooser_opens_inside_the_empty_pane() {
        let mut d = dashboard_with_session(running_session());
        d.conversation_area = Some(Rect::new(0, 0, 120, 40));
        let empty = d.browse_pane();
        d.split_focused_pane(Direction::Horizontal, None).unwrap();
        d.set_pane_session(empty, None);
        d.conversation_pane_areas =
            vec![(empty, Rect::new(46, 0, 74, 30), Rect::new(46, 30, 74, 10))];
        d.begin_pin_menu("session-1".into());
        d.handle_key(key(KeyCode::Char('3')));
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal
            .draw(|frame| render_pane_menu(frame, frame.area(), &d))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rows = (0..40)
            .map(|y| {
                (0..120)
                    .map(|x| buffer[(x, y)].symbol().to_owned())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        let marker = rows
            .iter()
            .position(|row| row.contains("[1] Pin here"))
            .expect("marker");
        let title = rows
            .iter()
            .position(|row| row.contains("Choose empty pane"))
            .expect("chooser");
        assert!(title > marker, "{rows:#?}");
        let column = (0..120u16)
            .find(|&x| buffer[(x, u16::try_from(title).unwrap())].symbol() != " ")
            .map(usize::from)
            .expect("chooser border");
        assert!(column >= 46, "{rows:#?}");
        // A click on the chooser's own entry picks it through the menu even
        // though the chooser lies inside the pane.
        let entry = (
            u16::try_from(column).unwrap() + 3,
            u16::try_from(title).unwrap() + 1,
        );
        let action = d.handle_pane_menu_event(Event::Mouse(mouse_at(
            MouseEventKind::Down(MouseButton::Left),
            entry,
        )));
        assert!(
            d.pane_menu
                .as_ref()
                .is_none_or(|menu| menu.pressed_destination.is_none()),
            "{action:?}"
        );
    }

    #[test]
    fn prompt_commands_and_list_selection_follow_the_active_pin() {
        let mut d = dashboard_with_session(running_session());
        let pin = d.browse_pane();
        d.split_focused_pane(Direction::Horizontal, None).unwrap();
        let mut other = running_session();
        other.id = "session-2".into();
        d.state.sessions.insert(other.id.clone(), other);
        d.set_state(d.state.clone());
        d.select_active_session("session-2");
        d.focus_pane(pin);
        d.focus_prompt();
        assert_eq!(
            d.dispatch_command(crate::actions::CommandId::UnpinSession),
            DashboardAction::UnpinSession {
                session_id: "session-1".into()
            }
        );
        assert_eq!(d.selected_session_id(), Some("session-1"));
    }

    #[test]
    fn unpin_glyph_does_not_select_the_session_or_open_its_composer() {
        let mut d = dashboard_with_session(running_session());
        let pin = d.browse_pane();
        d.set_pane_session(pin, Some("session-1"));
        d.split_focused_pane(Direction::Horizontal, None).unwrap();
        d.set_current_session(None);
        assert_eq!(
            d.toggle_session_pin("session-1".into()),
            DashboardAction::UnpinSession {
                session_id: "session-1".into()
            }
        );
        assert_eq!(d.selected_session_id(), None);
        assert!(d.pane_menu.is_none());
    }

    /// A press and release at one cell, as a click arrives from the terminal.
    fn click(d: &mut DashboardState, at: (u16, u16)) -> DashboardAction {
        d.handle_mouse(mouse_at(MouseEventKind::Down(MouseButton::Left), at));
        d.handle_mouse(mouse_at(MouseEventKind::Up(MouseButton::Left), at))
    }

    /// Where the open dropdown was drawn, after drawing the surface once.
    fn drawn_menu(d: &mut DashboardState) -> (Vec<String>, Rect) {
        let lines = drawn(d, 140, 40);
        let popup = d.pane_menu.as_ref().expect("the menu is open").popup.get();
        (lines, popup)
    }

    fn append_golden_state(
        output: &mut String,
        label: &str,
        width: u16,
        height: u16,
        lines: &[String],
    ) {
        output.push_str(&format!("=== {label} ({width}x{height}) ===\n"));
        output.push_str(&lines.join("\n"));
        output.push('\n');
    }

    fn append_golden_value(output: &mut String, label: &str, value: impl std::fmt::Debug) {
        output.push_str(&format!("{label}: {value:?}\n"));
    }

    /// The cell of a menu entry's first letter: inside the border, one row
    /// per entry.
    fn entry(popup: Rect, index: u16) -> (u16, u16) {
        (popup.x + 1, popup.y + 1 + index)
    }

    /// The text drawn in `width` cells from (`x`, `y`).
    fn cell_text(lines: &[String], x: u16, y: u16, width: usize) -> String {
        lines[usize::from(y)]
            .chars()
            .skip(usize::from(x))
            .take(width)
            .collect()
    }

    /// The Setup entries a title menu lists after Refresh: the label, the
    /// command it runs, and the Setup page (config section) that opens.
    type SetupEntries = &'static [(&'static str, CommandId, &'static str)];

    /// The two panes with a title menu: the pane, its index in `pane_areas`,
    /// the name on its title, the focus that owns it, and its entries after
    /// Refresh.
    const TITLE_MENUS: [(SupportPane, usize, &str, Focus, SetupEntries); 2] = [
        (
            SupportPane::Targets,
            1,
            "Targets",
            Focus::Targets,
            &[
                ("Runtimes…", CommandId::ManageTargets, "targets"),
                ("Machines…", CommandId::ManageMachines, "machines"),
            ],
        ),
        (
            SupportPane::Quota,
            2,
            "Profiles",
            Focus::Quota,
            &[("Settings…", CommandId::ManageProfiles, "profiles")],
        ),
    ];

    /// User request 2026-09-25: the Targets and Profiles titles are
    /// dropdowns. Clicking one opens a small menu hanging under that title,
    /// with Refresh and then Runtimes… and Machines… (Targets) or Settings…
    /// (Profiles), and leaves the keyboard where it was.
    /// Refresh runs the refresh `prefix+shift+r` runs. Each later entry opens
    /// Setup on the page its command opens: Runtimes… as "Manage runtimes"
    /// and Machines… as "Manage machines" (Targets), Settings… as "Manage
    /// agent profiles" (Profiles).
    /// The number keys pick a title-menu entry directly: 1 is Refresh, and
    /// 2 and 3 are Runtimes… and Machines… on Targets.
    /// The keyboard path: `.` on the focused pane opens the same menu, Enter
    /// runs the entry under the cursor, and Esc closes it without running
    /// anything.
    /// Minimized, Targets and Profiles are the last two rows above the
    /// footer, so neither has room for the menu under its title and the
    /// menu opens upward, still starting at the title's first column.
    #[test]
    fn a_minimized_pane_menu_opens_above_its_row() {
        for (_, index, name, focus, _) in TITLE_MENUS {
            let mut d = dashboard_with_session(running_session());
            for pane in [SupportPane::Targets, SupportPane::Quota] {
                d.set_pane_size(pane, crate::PaneSize::Minimized);
            }
            drawn(&mut d, 140, 40);
            d.focus = focus;
            d.handle_key(key(KeyCode::Char('.')));
            let (lines, popup) = drawn_menu(&mut d);
            let pane = d.pane_areas.expect("pane areas")[index];
            assert_eq!(pane.height, 1, "{name}");
            assert!(popup.bottom() <= pane.y, "{name}: {lines:#?}");
            assert_eq!(popup.x, pane.x + 1, "{name}");
            let (x, y) = entry(popup, 0);
            assert_eq!(cell_text(&lines, x, y, 7), "Refresh", "{lines:#?}");
        }
    }

    /// The size chips share the title row with the dropdown and keep their
    /// own clicks: a chip resizes the pane and opens no menu.
    #[test]
    fn golden_support_pane_title_menu() {
        let mut output = String::new();

        for (_, index, name, _, _) in TITLE_MENUS {
            let mut dashboard = dashboard_with_session(running_session());
            dashboard.focus_prompt();
            let lines = drawn(&mut dashboard, 140, 40);
            let pane = dashboard.pane_areas.expect("pane areas")[index];
            let title = point(&lines, &format!("{name} ▾"));
            dashboard.handle_mouse(mouse_at(MouseEventKind::Down(MouseButton::Left), title));
            let action =
                dashboard.handle_mouse(mouse_at(MouseEventKind::Up(MouseButton::Left), title));
            append_golden_value(&mut output, "title click action", action);
            append_golden_value(&mut output, "focus after title click", dashboard.focus());
            let (lines, popup) = drawn_menu(&mut dashboard);
            append_golden_value(
                &mut output,
                "menu anchor",
                (popup.x, popup.y, pane.x + 1, pane.y + 1),
            );
            append_golden_state(&mut output, &format!("{name} title menu"), 140, 40, &lines);
        }

        for (_, _index, name, _, settings) in TITLE_MENUS {
            let mut dashboard = dashboard_with_session(running_session());
            let lines = drawn(&mut dashboard, 140, 40);
            let title = point(&lines, &format!("{name} ▾"));
            click(&mut dashboard, title);
            let (_, popup) = drawn_menu(&mut dashboard);
            let action = click(&mut dashboard, entry(popup, 0));
            append_golden_value(&mut output, &format!("{name} Refresh action"), action);
            let lines = drawn(&mut dashboard, 140, 40);
            append_golden_state(
                &mut output,
                &format!("{name} after Refresh"),
                140,
                40,
                &lines,
            );

            for (row, (label, _command, page)) in (1..).zip(settings) {
                let mut dashboard = dashboard_with_session(running_session());
                let lines = drawn(&mut dashboard, 140, 40);
                let title = point(&lines, &format!("{name} ▾"));
                click(&mut dashboard, title);
                let (_, popup) = drawn_menu(&mut dashboard);
                let action = click(&mut dashboard, entry(popup, row));
                append_golden_value(&mut output, &format!("{name} {label} action"), action);
                let lines = drawn(&mut dashboard, 140, 40);
                append_golden_state(
                    &mut output,
                    &format!("{name} {page} setup page"),
                    140,
                    40,
                    &lines,
                );
            }
        }

        for (_, _, name, focus, settings) in TITLE_MENUS {
            let mut dashboard = dashboard_with_session(running_session());
            let _ = drawn(&mut dashboard, 140, 40);
            dashboard.focus = focus;
            let action = dashboard.handle_key(key(KeyCode::Char('.')));
            append_golden_value(&mut output, &format!("{name} dot action"), action);
            let lines = drawn(&mut dashboard, 140, 40);
            append_golden_state(
                &mut output,
                &format!("{name} opened by dot"),
                140,
                40,
                &lines,
            );
            for (digit, (label, _, page)) in ('2'..).zip(settings) {
                let mut dashboard = dashboard_with_session(running_session());
                let _ = drawn(&mut dashboard, 140, 40);
                dashboard.focus = focus;
                dashboard.handle_key(key(KeyCode::Char('.')));
                let action = dashboard.handle_key(key(KeyCode::Char(digit)));
                append_golden_value(
                    &mut output,
                    &format!("{name} number {digit} action"),
                    action,
                );
                let lines = drawn(&mut dashboard, 140, 40);
                append_golden_state(
                    &mut output,
                    &format!("{name} number {digit}: {label}, {page}"),
                    140,
                    40,
                    &lines,
                );
            }

            let mut dashboard = dashboard_with_session(running_session());
            let _ = drawn(&mut dashboard, 140, 40);
            dashboard.focus = focus;
            dashboard.handle_key(key(KeyCode::Char('.')));
            let open_lines = drawn(&mut dashboard, 140, 40);
            append_golden_state(
                &mut output,
                &format!("{name} keyboard menu"),
                140,
                40,
                &open_lines,
            );
            let action = dashboard.handle_key(key(KeyCode::Esc));
            append_golden_value(&mut output, &format!("{name} Escape action"), action);
            append_golden_value(
                &mut output,
                &format!("{name} focus after Escape"),
                dashboard.focus(),
            );
            append_golden_value(
                &mut output,
                &format!("{name} menu after Escape"),
                dashboard.pane_menu.is_some(),
            );
            let lines = drawn(&mut dashboard, 140, 40);
            append_golden_state(
                &mut output,
                &format!("{name} after Escape"),
                140,
                40,
                &lines,
            );

            dashboard.handle_key(key(KeyCode::Char('.')));
            let refresh = dashboard.handle_key(key(KeyCode::Enter));
            append_golden_value(
                &mut output,
                &format!("{name} Enter Refresh action"),
                refresh,
            );
            let lines = drawn(&mut dashboard, 140, 40);
            append_golden_state(
                &mut output,
                &format!("{name} after Enter Refresh"),
                140,
                40,
                &lines,
            );

            dashboard.handle_key(key(KeyCode::Char('.')));
            dashboard.handle_key(key(KeyCode::Down));
            let open_setup = dashboard.handle_key(key(KeyCode::Enter));
            append_golden_value(
                &mut output,
                &format!("{name} Enter setup action"),
                open_setup,
            );
            let lines = drawn(&mut dashboard, 140, 40);
            append_golden_state(
                &mut output,
                &format!("{name} setup from keyboard"),
                140,
                40,
                &lines,
            );
        }

        for (pane, _, name, _, _) in TITLE_MENUS {
            let mut dashboard = dashboard_with_session(running_session());
            let _ = drawn(&mut dashboard, 140, 40);
            for size in [crate::PaneSize::Minimized, crate::PaneSize::Standard] {
                let chip = dashboard
                    .pane_size_control_areas
                    .iter()
                    .find(|(candidate, candidate_size, _)| {
                        *candidate == pane && *candidate_size == size
                    })
                    .map(|(_, _, area)| *area)
                    .expect("the size chip is drawn");
                let action = click(&mut dashboard, (chip.x + 1, chip.y));
                append_golden_value(&mut output, &format!("{name} {size:?} size action"), action);
                append_golden_value(
                    &mut output,
                    &format!("{name} menu after size click"),
                    dashboard.pane_menu.is_some(),
                );
                let lines = drawn(&mut dashboard, 140, 40);
                append_golden_state(
                    &mut output,
                    &format!("{name} {size:?} pane"),
                    140,
                    40,
                    &lines,
                );
            }
        }

        mj_core::golden::assert_golden(
            env!("CARGO_MANIFEST_DIR"),
            "support-pane-title-menu",
            &output,
        );
    }
}
