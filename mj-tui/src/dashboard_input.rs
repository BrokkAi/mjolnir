use super::*;

impl DashboardState {
    /// The conversation pane the pointer is over, if any. A click there
    /// belongs to that pane's chat, whatever has focus.
    pub fn chat_region_contains(
        &self,
        column: u16,
        row: u16,
    ) -> Option<crate::tile_layout::PaneId> {
        self.conversation_pane_areas
            .iter()
            .find(|(_, transcript, prompt)| {
                rect_contains(*transcript, column, row) || rect_contains(*prompt, column, row)
            })
            .map(|(pane, _, _)| *pane)
    }

    /// Where the focused pane's transcript sat on the last frame.
    #[cfg(test)]
    pub(crate) fn focused_transcript_area(&self) -> Option<Rect> {
        self.pane_bands(self.focused_pane()).map(|(area, _)| area)
    }

    /// Where the focused pane's composer sat on the last frame.
    #[cfg(test)]
    pub(crate) fn focused_prompt_area(&self) -> Option<Rect> {
        self.pane_bands(self.focused_pane()).map(|(_, area)| area)
    }

    /// The transcript and composer rectangles one pane drew into.
    #[cfg(test)]
    pub(crate) fn pane_bands(&self, pane: crate::tile_layout::PaneId) -> Option<(Rect, Rect)> {
        self.conversation_pane_areas
            .iter()
            .find(|(id, _, _)| *id == pane)
            .map(|(_, transcript, prompt)| (*transcript, *prompt))
    }

    /// Opens the web-access dialog and asks the controller to load it.
    pub fn open_web_dialog(&mut self) -> DashboardAction {
        self.mode = Mode::Web(WebDialog::loading());
        DashboardAction::LoadWebAccess
    }

    /// Handles one terminal event and reports whether the dashboard consumed
    /// it. The legacy key and mouse wrappers below remain available to callers
    /// that only need the action.
    pub fn handle_event_result(&mut self, event: Event) -> EventResult<DashboardAction> {
        self.last_event_consumed.set(false);
        let action = match event {
            Event::Key(key) => self.handle_key_at(key, Instant::now()),
            Event::Mouse(mouse) => self.handle_mouse(mouse),
            Event::Paste(pasted) => {
                self.handle_paste(&pasted);
                DashboardAction::None
            }
            // The controller owns terminal-size comparison; focus events do
            // not mutate dashboard state by themselves.
            Event::Resize(_, _) | Event::FocusLost => {
                self.cancel_component_pointer();
                DashboardAction::None
            }
            Event::FocusGained => DashboardAction::None,
        };
        let action = (!matches!(&action, DashboardAction::None)).then_some(action);
        EventResult {
            consumed: self.last_event_consumed.get() || action.is_some(),
            action,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DashboardAction {
        self.handle_key_at(key, Instant::now())
    }

    /// Handles one key with an explicit reading of the clock. `now` decides
    /// whether the notice on screen has been readable long enough for this
    /// key press to dismiss it.
    pub fn handle_key_at(&mut self, key: KeyEvent, now: Instant) -> DashboardAction {
        if key.kind != KeyEventKind::Press && key.kind != KeyEventKind::Repeat {
            return DashboardAction::None;
        }
        if key.kind == KeyEventKind::Press {
            self.modal_click_transition = None;
            self.suppress_modal_release = false;
        }
        if self.pane_menu.is_some() {
            return self.handle_pane_menu_event(Event::Key(key));
        }
        if is_paste_shortcut(key) {
            self.record_event_handled();
            return DashboardAction::PasteFromClipboard;
        }
        let text_focused = self.text_input_focused();
        let cancel_shortcut = key.code == KeyCode::Char('c')
            && (key.modifiers.contains(KeyModifiers::CONTROL)
                || dashboard_accelerator(key.modifiers));
        if text_focused && cancel_shortcut && self.component_modal_open() {
            return self.handle_component_event(Event::Key(KeyEvent::new(
                KeyCode::Esc,
                KeyModifiers::NONE,
            )));
        }
        // Ctrl-C belongs to the prompt or a text field. Everywhere else it is
        // intentionally inert, including modal controls that happen to use
        // the letter `c` for another purpose.
        if cancel_shortcut {
            self.record_event_handled();
            return DashboardAction::None;
        }

        // Retire the notice this key press is stepping past, but only once it
        // has been on screen long enough to read: for a background failure
        // this bar is the only report there is.
        self.notices.dismiss(now);
        if self.component_modal_open() {
            return self.handle_component_event(crossterm::event::Event::Key(key));
        }
        if matches!(self.mode, Mode::Help(_)) {
            return self.handle_help_key(key);
        }
        let before = self.selected_session_id.clone();
        let action = self.handle_dashboard_key(key);
        if before != self.selected_session_id {
            self.request_selected_browse();
        }
        action
    }

    pub(crate) fn text_input_focused(&self) -> bool {
        self.active_modal()
            .is_some_and(|modal| modal.text_input_focused())
    }

    pub fn handle_paste(&mut self, pasted: &str) {
        if matches!(self.mode, Mode::Help(_)) {
            self.paste_help(pasted);
            return;
        }
        if self.component_modal_open() {
            self.handle_component_event(crossterm::event::Event::Paste(pasted.to_owned()));
            return;
        }
        if self.focus != Focus::Prompt {
            return;
        }
        let normalized = pasted.replace("\r\n", "\n").replace('\r', "\n");
        if let Some(session_id) = self.standby_prompt_session() {
            let session_id = session_id.to_owned();
            self.standby_prompt_mut(&session_id).paste(&normalized);
        } else if self.launch_standby_capturing()
            && let Some(standby) = self.launch_standby.as_mut()
        {
            standby.paste(&normalized);
        }
    }

    /// The surfaces the last frame registered, for the selection engine.
    pub fn frame_surfaces(&self) -> &FrameSurfaces {
        &self.frame_surfaces
    }

    pub fn handle_mouse(&mut self, mouse: MouseEvent) -> DashboardAction {
        let now = Instant::now();
        if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
            self.suppress_modal_release =
                self.modal_click_transition
                    .take()
                    .is_some_and(|(x, y, at)| {
                        x == mouse.column
                            && y == mouse.row
                            && now.saturating_duration_since(at)
                                <= mj_chat::components::DOUBLE_CLICK_INTERVAL
                    });
            if self.suppress_modal_release {
                return DashboardAction::None;
            }
        }
        if mouse.kind == MouseEventKind::Up(MouseButton::Left) && self.suppress_modal_release {
            self.suppress_modal_release = false;
            return DashboardAction::None;
        }
        let before = self.dialog_layer_key();
        let selected_before = self.selected_session_id.clone();
        let action = self.handle_mouse_inner(mouse);
        if selected_before != self.selected_session_id {
            self.request_selected_browse();
        }
        if mouse.kind == MouseEventKind::Up(MouseButton::Left) && before != self.dialog_layer_key()
        {
            self.modal_click_transition = Some((mouse.column, mouse.row, now));
            self.cancel_component_pointer();
        }
        action
    }

    pub(crate) fn handle_mouse_inner(&mut self, mouse: MouseEvent) -> DashboardAction {
        if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
            self.notices.dismiss(Instant::now());
            // Reaching for the mouse abandons a half-typed chord.
            self.prefix_pending = false;
            self.resize_mode = false;
        }
        if self.pane_menu.is_some() {
            return self.handle_pane_menu_event(Event::Mouse(mouse));
        }
        if mouse.kind == MouseEventKind::Down(MouseButton::Right)
            && matches!(self.mode, Mode::Dashboard)
            && let Some(pane) = self.chat_region_contains(mouse.column, mouse.row)
        {
            self.begin_pane_menu(pane);
            self.record_event_handled();
            return DashboardAction::None;
        }
        if matches!(self.mode, Mode::Help(_)) {
            return self.handle_help_mouse(mouse);
        }
        if self.component_modal_open() {
            return self.handle_component_event(crossterm::event::Event::Mouse(mouse));
        }
        if !matches!(self.mode, Mode::Dashboard) {
            return DashboardAction::None;
        }
        if let Some(action) = self.handle_surface_mouse(mouse) {
            return action;
        }
        if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
            if let Some(action) =
                crate::workspaces::workspace_tab_click(self, mouse.column, mouse.row)
            {
                return action;
            }

            if self
                .workspace_pane_area
                .is_some_and(|area| rect_contains(area, mouse.column, mouse.row))
            {
                self.focus = Focus::Workspaces;
                self.workspace_control_focus = crate::workspaces::WorkspaceControlFocus::Tabs;
                self.set_session_action_focus(None);
                self.record_event_handled();
                return DashboardAction::None;
            }
            if let Some(&(pane, size, _)) = self
                .pane_size_control_areas
                .iter()
                .find(|(_, _, area)| rect_contains(*area, mouse.column, mouse.row))
            {
                if size == PaneSize::Maximized && !self.pane_maximize_enabled(pane) {
                    // Defend against stale geometry if a resize arrives before
                    // the next frame redraws the visible controls.
                    return DashboardAction::None;
                }
                self.set_pane_size(pane, size);
                self.record_event_handled();
                return DashboardAction::None;
            }
            if let Some((project_key, _)) = self
                .project_heading_areas
                .iter()
                .find(|(_, area)| rect_contains(*area, mouse.column, mouse.row))
            {
                let project_key = project_key.clone();
                self.focus_sessions();
                self.toggle_project(&project_key);
                self.record_event_handled();
                return DashboardAction::None;
            }
            if let Some(&(index, _)) = self
                .session_row_areas
                .iter()
                .find(|(_, area)| rect_contains(*area, mouse.column, mouse.row))
            {
                self.record_event_handled();
                return self.handle_row_click(Focus::Sessions, index);
            }
            // The click missed every row; forget any pending double click so
            // a stray click elsewhere can't pair up with the next row click.
            self.last_row_click = None;
        }
        // The workspace tabs are a single horizontal row, so the wheel over
        // that pane switches tabs even when the pane is not focused.
        if matches!(
            mouse.kind,
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
        ) && self
            .workspace_pane_area
            .is_some_and(|area| rect_contains(area, mouse.column, mouse.row))
        {
            let delta = if mouse.kind == MouseEventKind::ScrollUp {
                -1
            } else {
                1
            };
            self.record_event_handled();
            return self.select_adjacent_workspace(delta);
        }
        let hovered = self.pane_areas.and_then(|areas| {
            areas
                .into_iter()
                .position(|area| rect_contains(area, mouse.column, mouse.row))
                .map(|index| match index {
                    0 => Focus::Sessions,
                    1 => Focus::Targets,
                    2 => Focus::Quota,
                    _ => unreachable!("the surface has exactly three support panes"),
                })
        });
        let Some(hovered) = hovered else {
            return DashboardAction::None;
        };
        // Minimized Targets and Quota show no selected row. Their summary can
        // take focus so the pane-size key can restore it, but hidden rows do not move or
        // activate underneath the user.
        let rows_visible = hovered == Focus::Sessions
            || hovered
                .support_pane()
                .is_some_and(|pane| self.pane_size(pane) != PaneSize::Minimized);
        // A wheel or a press inside a pane is the dashboard's answer, even
        // when it produces no action: the frame the next pointer event is
        // hit-tested against has to be rebuilt after it.
        match mouse.kind {
            MouseEventKind::ScrollUp if rows_visible => {
                self.scroll_selection_for(hovered, -1);
                self.record_event_handled();
            }
            MouseEventKind::ScrollDown if rows_visible => {
                self.scroll_selection_for(hovered, 1);
                self.record_event_handled();
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.focus = hovered;
                if hovered != Focus::Sessions {
                    self.set_session_action_focus(None);
                }
                self.clamp_selections();
                self.record_event_handled();
            }
            _ => {}
        }
        DashboardAction::None
    }

    /// Selects the clicked row and, if it's the second click on the same row
    /// within `DOUBLE_CLICK_INTERVAL`, performs the same action Enter would.
    pub(crate) fn handle_row_click(&mut self, focus: Focus, index: usize) -> DashboardAction {
        // Clicking a row selects it wherever the dial has left the pane.
        self.scroll_lookahead.set(None);
        self.focus = focus;
        self.set_session_action_focus(None);
        if focus == Focus::Sessions {
            let clicked = self
                .ordered_sessions()
                .get(index)
                .map(|session| session.id.clone());
            if clicked.is_some() && self.selected_session_id != clicked {
                self.selected_session_id = clicked;
            }
        } else {
            self.set_selection_for(focus, index);
        }
        let now = Instant::now();
        let is_double_click = matches!(
            self.last_row_click,
            Some((last_focus, last_index, last_time))
                if last_focus == focus
                    && last_index == index
                    && now.saturating_duration_since(last_time) <= DOUBLE_CLICK_INTERVAL
        );
        if is_double_click {
            self.last_row_click = None;
            self.open_selected_session()
        } else {
            self.last_row_click = Some((focus, index, now));
            DashboardAction::None
        }
    }

    /// Keys for the combined surface's panes.
    ///
    /// The composer is a separate focus and never reaches here, so the pane
    /// actions are plain letters rather than accelerated ones: no key typed at
    /// a pane can be mistaken for text.
    ///
    /// Everything that runs a named command is looked up in the action
    /// registry ([`crate::actions`]) rather than matched here, so the keys, the
    /// footer, and the help overlay are all reading one table. What stays as
    /// hand-written arms is the input that is not a command: list
    /// navigation, and the two keys whose meaning depends on state.
    pub(crate) fn handle_dashboard_key(&mut self, key: KeyEvent) -> DashboardAction {
        if let Some(action) = self.native_agent_key(key) {
            return action;
        }
        if let Some(action) = self.stopped_subagent_key(key) {
            return action;
        }
        let command = dashboard_accelerator(key.modifiers);
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER);
        if let Some(action) = self.handle_workspace_pane_key(key) {
            return action;
        }
        // The Sessions filter takes the keys it is editing with, and its state
        // letters, before anything else can read them as navigation. The action
        // row normally keeps the plain letters, but a filter that hides every
        // row moves the focus there, and the letters are then the only way back
        // to the sessions — including the Esc the empty pane advertises.
        let filter_owns_letters =
            self.session_action_focus.is_none() || self.sessions_filter.is_some();
        if self.focus == Focus::Sessions
            && self
                .handle_sessions_filter_key(key, plain && filter_owns_letters)
                .is_some()
        {
            self.record_event_handled();
            return DashboardAction::None;
        }
        match (key.code, command) {
            // Shift-Tab is the reverse of the registry's Tab.
            (KeyCode::BackTab, _) => {
                self.cycle_focus(true);
                self.record_event_handled();
                return DashboardAction::None;
            }
            // Escape belongs to the composer and to modals. On a pane it only
            // clears the notice bar: the combined surface is quit with the
            // detach key, and a stray Escape must never take the whole screen
            // away.
            // Inside a session's sub-agents, Escape on their list goes back
            // to the parent, the same as the X on the workspace strip.
            (KeyCode::Esc, _)
                if self.focus == Focus::Sessions && self.subagent_parent_id.is_some() =>
            {
                self.record_event_handled();
                return DashboardAction::ExitSubagentWorkspace;
            }
            (KeyCode::Esc, _) => {
                self.notices.clear();
                self.record_event_handled();
                return DashboardAction::None;
            }
            _ => {}
        }
        // The standby composer answers the same keys the focused prompt
        // would — the full readline set — before list navigation can claim
        // the arrows.
        if let Some(action) = self.handle_standby_prompt_key(key) {
            return action;
        }
        if plain
            && self.focus == Focus::Sessions
            && let Some(action) = self.handle_session_action_key(key)
        {
            self.record_event_handled();
            return action;
        }
        // List navigation, shared by visible lists. It comes before the
        // registry so `j`, `k`, Ctrl-N, and Ctrl-P keep moving the selection.
        if self.focused_rows_visible() {
            match (key.code, command) {
                (KeyCode::Up | KeyCode::Char('k'), false) | (KeyCode::Char('p'), true) => {
                    self.move_selection(-1);
                    self.record_event_handled();
                    return DashboardAction::None;
                }
                (KeyCode::Down | KeyCode::Char('j'), false) | (KeyCode::Char('n'), true) => {
                    self.move_selection(1);
                    self.record_event_handled();
                    return DashboardAction::None;
                }
                // `ctrl+d` and `ctrl+u` page by half a screen, as they do in
                // every other list on the surface.
                (KeyCode::Char('u'), true) => {
                    self.move_selection(-8);
                    self.record_event_handled();
                    return DashboardAction::None;
                }
                (KeyCode::Char('d'), true) => {
                    self.move_selection(8);
                    self.record_event_handled();
                    return DashboardAction::None;
                }
                (KeyCode::Home, _) => {
                    self.set_selection_for(self.focus, 0);
                    self.record_event_handled();
                    return DashboardAction::None;
                }
                (KeyCode::End, _) | (KeyCode::Char('G'), false) => {
                    let len = self.focus_len_for(self.focus);
                    self.set_selection_for(self.focus, len.saturating_sub(1));
                    self.record_event_handled();
                    return DashboardAction::None;
                }
                _ => {}
            }
        }
        // Setup and the session editor never both apply: setup only opens
        // while the config is empty, and an empty config has no sessions. The
        // registry cannot resolve this on the key alone, because `e` is also
        // the Sessions, Targets, and Quota panes' key, so the ambiguity is
        // settled here and `Scope::Setup` is left out of
        // `pane_command_for_key`.
        if plain && key.code == KeyCode::Char('e') && self.config_is_empty() {
            let action = self.dispatch_command(CommandId::OpenConfig);
            self.record_event_handled();
            return action;
        }
        // A digit picks a project by its number, and a registry command
        // carries no argument, so this one stays a hand-written arm.
        if self.focus == Focus::Sessions
            && plain
            && let KeyCode::Char(digit @ '1'..='9') = key.code
        {
            self.toggle_project_number(digit.to_digit(10).unwrap_or(0) as usize);
            self.record_event_handled();
            return DashboardAction::None;
        }
        match crate::actions::pane_command_for_key(key, self.focus) {
            Some(id) => {
                let action = self.dispatch_command(id);
                self.record_event_handled();
                action
            }
            None => DashboardAction::None,
        }
    }

    /// Handles the small action row at the top of Sessions. The row is a
    /// second selection target within the pane: Up from its first session
    /// enters it, Down returns to the first session, and Left/Right skip any
    /// disabled action. `None` means the regular dashboard key handling still
    /// owns the key; `Some` means the key was consumed, including a no-op.
    pub(crate) fn handle_session_action_key(&mut self, key: KeyEvent) -> Option<DashboardAction> {
        let session_count = self.visible_session_indices().len();
        let focused_action = self.session_action_focus;
        match (focused_action, key.code) {
            (Some(id), KeyCode::Left | KeyCode::Right) => {
                if let Some(next) = crate::surface_controls::adjacent_enabled_session_action(
                    self,
                    id,
                    key.code == KeyCode::Right,
                ) {
                    self.session_action_focus = Some(next);
                }
                Some(DashboardAction::None)
            }
            (Some(_), KeyCode::Up) => Some(DashboardAction::None),
            (Some(_), KeyCode::Down) if session_count > 0 => {
                self.set_session_action_focus(None);
                self.set_selection_for(Focus::Sessions, 0);
                Some(DashboardAction::None)
            }
            (Some(_), KeyCode::Down) => Some(DashboardAction::None),
            (Some(id), KeyCode::Enter) => {
                self.set_session_action_focus(None);
                Some(self.run_available_command(id))
            }
            (None, KeyCode::Up)
                if session_count == 0 || self.selected_visible_index() == Some(0) =>
            {
                self.session_action_focus =
                    crate::surface_controls::first_enabled_session_action(self);
                Some(DashboardAction::None)
            }
            (None, KeyCode::Down) if session_count == 0 => {
                self.session_action_focus =
                    crate::surface_controls::first_enabled_session_action(self);
                Some(DashboardAction::None)
            }
            (None, KeyCode::Left | KeyCode::Right) if session_count == 0 => {
                let first = crate::surface_controls::first_enabled_session_action(self);
                self.session_action_focus = if key.code == KeyCode::Right {
                    first
                } else {
                    first.and_then(|id| {
                        crate::surface_controls::adjacent_enabled_session_action(self, id, false)
                            .or(Some(id))
                    })
                };
                Some(DashboardAction::None)
            }
            _ => None,
        }
    }

    /// Opens the selected session's conversation and hands the keyboard to its
    /// composer.
    ///
    /// The conversation already follows the selection, so Enter's job is to
    /// take the user to the prompt for the row they are on. A failed session
    /// is the one exception: it asks first, because reading what it did and
    /// putting it back on a fresh target are both reasonable answers to the
    /// same key. That is a prompt rather than the silent diversion into the
    /// resume wizard this used to do - the row is red, and the dialog says
    /// what failed.
    pub(crate) fn open_selected_session(&mut self) -> DashboardAction {
        let Some(session) = self.selected_session() else {
            return DashboardAction::None;
        };
        // A stopped sub-agent is read, not resumed: its parent owns it.
        if self.is_native_agent(&session.id) || self.is_stopped_subagent(&session.id) {
            return DashboardAction::Open {
                session_id: session.id.clone(),
            };
        }
        if let Some(issue) = session.configuration_issue(&self.config) {
            self.mode = Mode::Confirm(self.confirm_dialog(Confirmation::ConfigurationRepair {
                session_id: session.id.clone(),
                error: issue,
                previous: Box::new(self.mode.clone()),
            }));
            return DashboardAction::None;
        }
        if let Some(operation) = self.session_operations.get(&session.id) {
            let label = operation.kind.label();
            self.notices
                .set(match self.first_key_label(CommandId::CancelOperation) {
                    Some(cancel) => format!("{label} is in progress; press {cancel} to cancel it."),
                    None => format!("{label} is in progress."),
                });
            return DashboardAction::None;
        }
        if let Some(transition) = self.transition_kind(&session.id) {
            self.notices.set(format!(
                "{} is in progress; select another session while it completes.",
                transition.label()
            ));
            return DashboardAction::None;
        }
        if self.transition_failure_kind(&session.id).is_some() {
            let confirmation = Confirmation::RecoverFailed {
                session_id: session.id.clone(),
                error: session.last_error.clone(),
                recoverable: session.checkpoint.is_some(),
            };
            self.mode = Mode::Confirm(
                ConfirmDialog::new(confirmation).naming_session(session.display_title()),
            );
            return DashboardAction::None;
        }
        if let Some(operation) = self
            .move_operations
            .get(&session.id)
            .filter(|operation| {
                matches!(
                    operation.phase,
                    mj_core::state::MovePhase::Failed | mj_core::state::MovePhase::Cancelled
                ) && (operation.checkpoint.is_some()
                    || (operation.queue_admission_started && !operation.queue_admission_finished))
            })
            .cloned()
        {
            self.mode = Mode::Confirm(ConfirmDialog::new(Confirmation::RecoverMove {
                operation: Box::new(operation),
            }));
            return DashboardAction::None;
        }
        // A failed session has two reasonable answers - read what it did, or
        // put it back on a fresh target - and recovery replaces the target, so
        // the surface asks rather than guessing.
        if session.state == SessionState::Error {
            let confirmation = Confirmation::RecoverFailed {
                session_id: session.id.clone(),
                error: session.last_error.clone(),
                recoverable: session.checkpoint.is_some(),
            };
            self.mode = Mode::Confirm(
                ConfirmDialog::new(confirmation).naming_session(session.display_title()),
            );
            return DashboardAction::None;
        }
        let session_id = session.id.clone();
        self.focus_prompt();
        DashboardAction::Open { session_id }
    }
}
