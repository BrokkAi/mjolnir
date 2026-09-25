//! Pin placement and pane menus share the dashboard's component event model.
use super::*;
use crate::actions::CommandId;
use crate::surface_controls::SurfaceControl;
use crate::tile_layout::PaneId;
use mj_chat::components::{ChoiceList, ControlKind, Dialog, Interaction, ListActivation};
use mj_chat::theme;
use ratatui::{Frame, layout::Direction, style::Style, text::Line, widgets::Paragraph};

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
        });
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
        self.conversation_layout
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
    let height = (entries.len() as u16 + 2).min(area.height);
    let x = anchor.x.min(area.right().saturating_sub(width)).max(area.x);
    let y = if anchor.bottom().saturating_add(height) <= area.bottom() {
        anchor.bottom()
    } else {
        anchor.y.saturating_sub(height).max(area.y)
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
    fn mouse_pin_control_opens_placement_then_activates_the_named_session() {
        let mut d = dashboard_with_session(running_session());
        let pane = d.browse_pane();
        d.set_pane_session(pane, Some("session-1"));
        d.conversation_pane_areas =
            vec![(pane, Rect::new(0, 0, 100, 20), Rect::new(0, 20, 100, 5))];
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| {
                d.begin_surface_frame();
                render_pane_chrome(frame, &d);
                d.end_surface_frame();
            })
            .unwrap();
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            d.handle_event_result(Event::Mouse(mouse_at(kind, (91, 0))));
        }
        assert!(d.pane_menu.is_some());
        assert_eq!(d.selected_session_id(), Some("session-1"));
        let action = d.handle_key(key(KeyCode::Enter));
        assert_eq!(
            action,
            DashboardAction::OpenSessionInSplit {
                session_id: "session-1".into(),
                direction: Direction::Horizontal
            }
        );
        assert!(d.pane_menu.is_none());
    }

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

    #[test]
    fn mouse_empty_destination_requires_press_and_release_in_the_same_pane() {
        let mut d = dashboard_with_session(running_session());
        d.conversation_area = Some(Rect::new(0, 0, 120, 40));
        let empty = d.browse_pane();
        d.split_focused_pane(Direction::Horizontal, None).unwrap();
        d.set_pane_session(empty, None);
        d.conversation_pane_areas =
            vec![(empty, Rect::new(60, 0, 60, 30), Rect::new(60, 30, 60, 10))];
        d.begin_pin_menu("session-1".into());
        d.handle_key(key(KeyCode::Char('3')));
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal
            .draw(|frame| render_pane_menu(frame, frame.area(), &d))
            .unwrap();
        assert_eq!(
            d.handle_pane_menu_event(Event::Mouse(mouse_at(
                MouseEventKind::Down(MouseButton::Left),
                (110, 25)
            ))),
            DashboardAction::None
        );
        assert!(d.pane_menu.is_some());
        assert_eq!(
            d.handle_pane_menu_event(Event::Mouse(mouse_at(
                MouseEventKind::Up(MouseButton::Left),
                (110, 25)
            ))),
            DashboardAction::PinSession {
                session_id: "session-1".into(),
                pane: empty
            }
        );
        assert!(d.pane_menu.is_none());
    }

    /// Launch campaign finding D-10: the empty-pane chooser opens in the
    /// empty pane, under its "[1] Pin here" marker, not at the screen's
    /// top-left corner over the Workspaces pane.
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
    fn prompt_commands_target_the_focused_pin_without_moving_the_list_cursor() {
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
        assert_eq!(d.selected_session_id(), Some("session-2"));
    }

    #[test]
    fn unpin_glyph_does_not_select_the_session_or_open_its_composer() {
        let mut d = dashboard_with_session(running_session());
        let pin = d.browse_pane();
        d.set_pane_session(pin, Some("session-1"));
        d.split_focused_pane(Direction::Horizontal, None).unwrap();
        d.selected_session_id = None;
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

    /// Asserts that `d` shows Setup on `page`, the page `command` opens.
    fn assert_setup_page(d: &DashboardState, command: CommandId, page: &str) {
        let mut expected = dashboard_with_session(running_session());
        expected.dispatch_command(command);
        assert!(matches!(d.mode, Mode::Setup(_)), "{page}");
        assert_eq!(d.dialog_layer_key(), expected.dialog_layer_key(), "{page}");
        assert!(
            d.dialog_layer_key().contains(&format!("[{page:?}]")),
            "{page}: {}",
            d.dialog_layer_key()
        );
    }

    /// User request 2026-09-25: the Targets and Profiles titles are
    /// dropdowns. Clicking one opens a small menu hanging under that title,
    /// with Refresh and then Runtimes… and Machines… (Targets) or Settings…
    /// (Profiles), and leaves the keyboard where it was.
    #[test]
    fn clicking_a_support_pane_title_opens_its_menu_under_the_title() {
        for (_, index, name, _, settings) in TITLE_MENUS {
            let mut d = dashboard_with_session(running_session());
            d.focus_prompt();
            let lines = drawn(&mut d, 140, 40);
            let pane = d.pane_areas.expect("pane areas")[index];
            let title = point(&lines, &format!("{name} ▾"));
            assert_eq!(title.1, pane.y, "{name}: the dropdown mark is on the title");

            assert_eq!(click(&mut d, title), DashboardAction::None);
            assert!(d.pane_menu.is_some(), "{name}");
            assert_eq!(d.focus(), Focus::Prompt, "{name}");

            let (lines, popup) = drawn_menu(&mut d);
            assert_eq!(
                (popup.x, popup.y),
                (pane.x + 1, pane.y + 1),
                "{name}: {lines:#?}"
            );
            assert!(lines[usize::from(popup.y)].contains(name), "{lines:#?}");
            let labels: Vec<&str> = d
                .pane_menu
                .as_ref()
                .expect("the menu is open")
                .entries
                .iter()
                .map(|(label, _)| label.as_str())
                .collect();
            let expected: Vec<&str> = std::iter::once("Refresh")
                .chain(settings.iter().map(|(label, _, _)| *label))
                .collect();
            assert_eq!(labels, expected, "{name}");
            for (row, label) in (0..).zip(&expected) {
                let (x, y) = entry(popup, row);
                assert_eq!(
                    cell_text(&lines, x, y, label.chars().count()),
                    *label,
                    "{lines:#?}"
                );
            }
        }
    }

    /// Refresh runs the refresh `prefix+shift+r` runs. Each later entry opens
    /// Setup on the page its command opens: Runtimes… as "Manage runtimes"
    /// and Machines… as "Manage machines" (Targets), Settings… as "Manage
    /// agent profiles" (Profiles).
    #[test]
    fn a_title_menu_refreshes_and_opens_each_setup_page() {
        for (_, _, name, _, settings) in TITLE_MENUS {
            let mut d = dashboard_with_session(running_session());
            let lines = drawn(&mut d, 140, 40);
            let title = point(&lines, &format!("{name} ▾"));
            click(&mut d, title);
            let (_, popup) = drawn_menu(&mut d);
            assert_eq!(
                click(&mut d, entry(popup, 0)),
                DashboardAction::RefreshAll,
                "{name}"
            );
            assert!(d.pane_menu.is_none());
            assert!(matches!(d.mode, Mode::Dashboard));

            for (row, (_, command, page)) in (1..).zip(settings) {
                let mut d = dashboard_with_session(running_session());
                drawn(&mut d, 140, 40);
                click(&mut d, title);
                let (_, popup) = drawn_menu(&mut d);
                assert_eq!(click(&mut d, entry(popup, row)), DashboardAction::None);
                assert!(d.pane_menu.is_none(), "{name}");
                assert_setup_page(&d, *command, page);
            }
        }
    }

    /// The number keys pick a title-menu entry directly: 1 is Refresh, and
    /// 2 and 3 are Runtimes… and Machines… on Targets.
    #[test]
    fn number_keys_pick_a_title_menu_entry() {
        for (_, _, name, focus, settings) in TITLE_MENUS {
            let mut d = dashboard_with_session(running_session());
            drawn(&mut d, 140, 40);
            d.focus = focus;
            d.handle_key(key(KeyCode::Char('.')));
            assert_eq!(
                d.handle_key(key(KeyCode::Char('1'))),
                DashboardAction::RefreshAll,
                "{name}"
            );
            assert!(d.pane_menu.is_none(), "{name}");

            for ((_, command, page), digit) in settings.iter().zip('2'..) {
                let mut d = dashboard_with_session(running_session());
                drawn(&mut d, 140, 40);
                d.focus = focus;
                d.handle_key(key(KeyCode::Char('.')));
                assert_eq!(
                    d.handle_key(key(KeyCode::Char(digit))),
                    DashboardAction::None
                );
                assert!(d.pane_menu.is_none(), "{name}");
                assert_setup_page(&d, *command, page);
            }
        }
    }

    /// The keyboard path: `.` on the focused pane opens the same menu, Enter
    /// runs the entry under the cursor, and Esc closes it without running
    /// anything.
    #[test]
    fn dot_opens_the_focused_pane_menu_and_esc_closes_it() {
        for (_, index, name, focus, settings) in TITLE_MENUS {
            let mut d = dashboard_with_session(running_session());
            drawn(&mut d, 140, 40);
            d.focus = focus;
            assert_eq!(d.handle_key(key(KeyCode::Char('.'))), DashboardAction::None);
            let (lines, popup) = drawn_menu(&mut d);
            let pane = d.pane_areas.expect("pane areas")[index];
            assert_eq!((popup.x, popup.y), (pane.x + 1, pane.y + 1), "{name}");
            assert!(lines[usize::from(popup.y)].contains(name), "{lines:#?}");
            assert_eq!(d.handle_key(key(KeyCode::Esc)), DashboardAction::None);
            assert!(d.pane_menu.is_none(), "{name}");
            assert!(matches!(d.mode, Mode::Dashboard));
            assert_eq!(d.focus(), focus);

            d.handle_key(key(KeyCode::Char('.')));
            assert_eq!(
                d.handle_key(key(KeyCode::Enter)),
                DashboardAction::RefreshAll,
                "{name}"
            );
            d.handle_key(key(KeyCode::Char('.')));
            d.handle_key(key(KeyCode::Down));
            assert_eq!(d.handle_key(key(KeyCode::Enter)), DashboardAction::None);
            let (_, command, page) = settings[0];
            assert_setup_page(&d, command, page);
        }
    }

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
    fn the_size_controls_on_a_menu_title_still_resize_the_pane() {
        for (pane_id, _, name, _, _) in TITLE_MENUS {
            let mut d = dashboard_with_session(running_session());
            drawn(&mut d, 140, 40);
            for size in [crate::PaneSize::Minimized, crate::PaneSize::Standard] {
                let chip = d
                    .pane_size_control_areas
                    .iter()
                    .find(|(pane, chip, _)| *pane == pane_id && *chip == size)
                    .map(|(_, _, area)| *area)
                    .expect("the size chip is drawn");
                click(&mut d, (chip.x + 1, chip.y));
                assert_eq!(d.pane_size(pane_id), size, "{name}");
                assert!(d.pane_menu.is_none(), "{name}");
                drawn(&mut d, 140, 40);
            }
        }
    }
}
