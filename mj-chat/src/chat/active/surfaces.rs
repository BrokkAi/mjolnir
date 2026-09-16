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

    /// The surfaces the last frame registered, for the selection engine.
    pub fn frame_surfaces(&self) -> &FrameSurfaces {
        self.state.frame_surfaces()
    }

    /// Keep a scrollbar gesture routed here even outside the chat pane.
    pub fn transcript_scrollbar_dragging(&self) -> bool {
        self.state.transcript_scrollbar_dragging()
    }

    /// Rows the composer wants at `width`: the wrapped input, up to three
    /// queued-prompt previews, and the block's own border rows.
    pub fn desired_prompt_height(&self, width: u16) -> u16 {
        self.state.desired_prompt_height(width)
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
        render_in(
            frame,
            &mut self.state,
            regions,
            prompt_focused,
            transcript_selected,
        );
    }

    /// Visible host footer commands, indexed through the supplied chords then functions.
    pub fn footer_command_areas(&self) -> Vec<(usize, Rect)> {
        self.state.footer_command_areas.borrow().clone()
    }

    /// Whether the last frame's surfaces stand alone, because a modal owned
    /// the frame.
    pub fn frame_surfaces_exclusive(&self) -> bool {
        self.state.frame_surfaces_exclusive()
    }

    /// Clears the screen geometry retained by chat components before a host
    /// redraw. Focus and an in-flight pointer gesture remain owned by chat.
    pub fn reset_component_geometry(&mut self) {
        self.state.reset_component_geometry();
    }

    /// Whether a chat component owns this pointer event before host selection.
    pub fn component_handles_mouse(&self, mouse: crossterm::event::MouseEvent) -> bool {
        self.state.component_handles_mouse(mouse)
    }

    /// Whether a chat modal currently owns the frame.
    pub fn component_modal_open(&self) -> bool {
        self.state.component_modal_open()
    }

    /// Releases any pointer gesture held by a chat component.
    pub fn cancel_component_pointer(&mut self) {
        self.state.cancel_component_pointer();
    }

    /// The transcript text a finished selection covers.
    pub fn transcript_selection_text(&mut self, range: &SelectionRange) -> Option<String> {
        self.state.transcript_selection_text(range)
    }

    /// The message text a selection in the elicitation pane covers.
    pub fn elicitation_selection_text(&self, range: &SelectionRange) -> Option<String> {
        self.state.elicitation_selection_text(range)
    }

    /// The text a selection in the reviewer pane covers. It is resolved
    /// against that pane's own rows, so a drag there can never pick up the
    /// primary transcript's text.
    pub fn reviewer_selection_text(&self, range: &SelectionRange) -> Option<String> {
        self.state.reviewer_selection_text(range)
    }

    /// Whether the transcript's selection row space stopped describing the
    /// rows on screen since the last call.
    pub fn transcript_selection_invalidated(&mut self) -> bool {
        self.state.transcript_selection_invalidated()
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
            SurfaceId::ReviewerTranscript => {
                self.state
                    .scroll_second_opinion(if direction < 0 { -rows } else { rows });
            }
            _ => {}
        }
    }
}
