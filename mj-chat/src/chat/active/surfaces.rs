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
        render_in(
            frame,
            &mut self.state,
            regions,
            prompt_focused,
            transcript_selected,
        );
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
            SurfaceId::ReviewerTranscript => {
                self.state
                    .scroll_second_opinion(if direction < 0 { -rows } else { rows });
            }
            _ => {}
        }
    }
}
