use super::*;

impl ActiveChat {
    /// Stops any dictation thread. The thread reports `Finished`, which clears
    /// the view's voice state, so this only asks it to stop.
    pub(crate) fn cancel_dictation(&mut self) {
        if let Some(cancel) = self.voice_cancel.take()
            && let Err(error) = cancel.send(crate::speech::VoiceCommand::Cancel)
        {
            tracing::debug!(%error, "dictation worker already stopped");
        }
    }

    /// Draws the transcript and the composer into `regions`, for a host that
    /// owns the rest of the frame.
    ///
    /// `prompt_focused` says whether the composer owns the keyboard; only then
    /// does it draw a cursor and an accent border. `transcript_selected` says
    /// the selection engine still owns a selection on the transcript, so its
    /// row space has to stay frozen for this frame.
    pub fn draw_in(
        &mut self,
        frame: &mut Frame,
        regions: ChatRegions<'_>,
        prompt_focused: bool,
        transcript_selected: bool,
    ) {
        self.select_elicitation_surface(super::super::ElicitationSurface::Conversation);
        render_in(
            frame,
            &mut self.state,
            regions,
            prompt_focused,
            transcript_selected,
        );
        self.dispatch_tool_details();
    }

    pub(super) fn select_elicitation_surface(&mut self, surface: super::super::ElicitationSurface) {
        if let Some(dialog) = self.state.elicitation.as_mut() {
            dialog.select_surface(surface);
        }
    }

    /// Accepted form input and answer delivery retain their result owner.
    pub fn has_pending_elicitation_input(&self) -> bool {
        (self.state.elicitation.is_some() && self.state.composer_runtime.paste_in_flight)
            || self.state.has_pending_elicitation_answers()
    }

    pub fn has_elicitation(&self) -> bool {
        self.state.elicitation.is_some()
    }

    /// Eligibility follows the dialog's rendered interior, across surfaces.
    pub fn elicitation_input_available_in(
        &self,
        surface: super::super::ElicitationSurface,
    ) -> bool {
        self.state
            .elicitation
            .as_ref()
            .is_some_and(|dialog| dialog.was_rendered_in(surface))
    }

    pub fn elicitation_component_handles_mouse_in(
        &self,
        surface: super::super::ElicitationSurface,
        mouse: crossterm::event::MouseEvent,
    ) -> bool {
        self.state.elicitation.as_ref().is_some_and(|dialog| {
            dialog.component_handles_mouse_in(surface, mouse.column, mouse.row)
        })
    }

    /// Release a presentation that navigation stopped displaying, while
    /// leaving capture in the other presentation untouched.
    pub fn cancel_elicitation_pointer_in(&self, surface: super::super::ElicitationSurface) {
        if let Some(dialog) = self.state.elicitation.as_ref() {
            dialog.cancel_pointer_in(surface);
        }
    }

    pub fn elicitation_pointer_surface(&self) -> Option<super::super::ElicitationSurface> {
        self.state
            .elicitation
            .as_ref()
            .and_then(|dialog| dialog.pointer_surface())
    }

    /// The chat, including reviewer routing, owns which form is current.
    pub fn elicitation_height(&self, width: u16) -> Option<u16> {
        self.state
            .elicitation
            .as_ref()
            .map(|dialog| dialog.natural_height(width))
    }

    /// Draw the current form without making another draft or response path.
    pub fn draw_elicitation_in(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        focused: bool,
    ) -> crate::selection::FrameSurfaces {
        self.select_elicitation_surface(super::super::ElicitationSurface::Notification);
        let mut surfaces = crate::selection::FrameSurfaces::new();
        if let Some(dialog) = self.state.elicitation.as_ref() {
            render_elicitation_in(frame, dialog, &mut surfaces, area, focused);
        }
        surfaces
    }

    /// The logical text whose row space this form exposes for selection.
    pub fn elicitation_message_text(&self) -> Option<String> {
        self.state
            .elicitation
            .as_ref()
            .map(|dialog| dialog.prompt_text())
    }

    pub fn elicitation_selection_text_in(
        &mut self,
        surface: super::super::ElicitationSurface,
        range: &crate::selection::SelectionRange,
    ) -> Option<String> {
        self.select_elicitation_surface(surface);
        self.state.elicitation_selection_text(range)
    }

    pub fn autoscroll_elicitation_in(
        &mut self,
        surface: super::super::ElicitationSurface,
        direction: i8,
    ) {
        self.select_elicitation_surface(surface);
        self.autoscroll_selection(SurfaceId::ElicitationMessage, direction);
    }

    pub fn elicitation_minimum_height(&self, width: u16) -> Option<u16> {
        self.state
            .elicitation
            .as_ref()
            .map(|dialog| dialog.minimum_height(width))
    }

    /// Visible host footer commands, indexed through the supplied chords then
    /// functions, with the text drawn in each area. The text can differ from
    /// the supplied hint: the first surviving chord carries the chord prefix.
    pub fn footer_command_areas(&self) -> Vec<(usize, Rect, String)> {
        self.state.footer_command_areas.borrow().clone()
    }

    /// Scrolls the surface a drag is holding against one of its edges.
    /// `direction` is negative for up and positive for down.
    pub fn autoscroll_selection(&mut self, surface: SurfaceId, direction: i8) {
        let rows = isize::try_from(MOUSE_SCROLL_ROWS).unwrap_or(1);
        match surface {
            SurfaceId::Transcript if direction < 0 => {
                self.state.scroll_history_up(MOUSE_SCROLL_ROWS);
            }
            SurfaceId::Transcript => {
                self.state.scroll_history_down(MOUSE_SCROLL_ROWS);
            }
            SurfaceId::ElicitationMessage => self
                .state
                .scroll_elicitation_message(if direction < 0 { -rows } else { rows }),
            _ => {}
        }
    }
}
