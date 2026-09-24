//! Read-only view of a Mjolnir sub-agent that has stopped.
//!
//! A closed child has no worker to attach to, so its conversation cannot open
//! the ordinary way: an attach waits out its timeout and leaves the pane
//! empty. Its transcript is durable, so the host loads the stored tail in the
//! background and this draws it, the way a native agent's pane is drawn.

use super::*;

pub(crate) struct StoppedSubagentPane {
    /// `None` until the stored tail arrives.
    transcript: Option<mj_chat::chat::TranscriptSnapshot>,
    empty: bool,
    error: Option<String>,
    scroll: usize,
}

impl DashboardState {
    /// Whether `id` is a Mjolnir sub-agent whose session has stopped, so its
    /// conversation can only be read.
    pub fn is_stopped_subagent(&self, id: &str) -> bool {
        self.state.subagents.contains_key(id) && self.pane_session_is_suspended(id)
    }

    /// Start showing a stopped sub-agent. Returns whether the host has to load
    /// its stored transcript: not when it is loaded or loading already, and
    /// again after a load that failed.
    pub fn begin_stopped_subagent(&mut self, id: &str) -> bool {
        match self.stopped_subagents.get_mut(id) {
            Some(pane) if pane.error.is_none() => false,
            Some(pane) => {
                pane.error = None;
                true
            }
            None => {
                self.stopped_subagents.insert(
                    id.to_owned(),
                    StoppedSubagentPane {
                        transcript: None,
                        empty: false,
                        error: None,
                        scroll: 0,
                    },
                );
                true
            }
        }
    }

    /// The stored tail of a stopped sub-agent's conversation, or why it could
    /// not be read.
    pub fn set_stopped_subagent_transcript(
        &mut self,
        id: &str,
        loaded: std::result::Result<Option<mj_core::state::MaterializedSession>, String>,
    ) {
        let Some(pane) = self.stopped_subagents.get_mut(id) else {
            return;
        };
        match loaded {
            Ok(Some(session)) => {
                pane.empty = session.transcript.is_empty();
                pane.transcript = Some(mj_chat::chat::TranscriptSnapshot::from_materialized(
                    &session,
                ));
            }
            Ok(None) => {
                pane.empty = true;
                pane.transcript = Some(mj_chat::chat::TranscriptSnapshot::from_entries(Vec::new()));
            }
            Err(error) => pane.error = Some(error),
        }
    }

    pub(crate) fn render_stopped_subagent(
        &mut self,
        frame: &mut ratatui::Frame,
        id: &str,
        transcript_area: ratatui::layout::Rect,
        prompt_area: ratatui::layout::Rect,
    ) {
        use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
        let title = self.state.sessions.get(id).map_or_else(
            || id.to_owned(),
            |session| session.display_title().to_owned(),
        );
        let Some(pane) = self.stopped_subagents.get_mut(id) else {
            return;
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" {title} · stopped "));
        let inner = block.inner(transcript_area);
        frame.render_widget(block, transcript_area);
        match (&mut pane.transcript, &pane.error) {
            (_, Some(error)) => frame.render_widget(
                Paragraph::new(format!(
                    "Could not read this sub-agent's transcript: {error}\nOpen it again to retry."
                ))
                .wrap(Wrap { trim: false }),
                inner,
            ),
            (None, None) => frame.render_widget(
                Paragraph::new(format!(
                    "Loading this sub-agent's transcript{}",
                    mj_chat::theme::glyphs().ellipsis
                )),
                inner,
            ),
            (Some(_), None) if pane.empty => {
                frame.render_widget(Paragraph::new("This sub-agent left no transcript."), inner)
            }
            (Some(transcript), None) => {
                let (lines, scroll) =
                    transcript.rich_tail_scrolled(inner.width, inner.height as usize, pane.scroll);
                pane.scroll = scroll;
                frame.render_widget(Paragraph::new(lines), inner);
            }
        }
        frame.render_widget(
            Paragraph::new(
                "This sub-agent has stopped; its conversation is read-only.\n\
                 PgUp/PgDn: scroll · End: latest",
            )
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Stopped sub-agent"),
            ),
            prompt_area,
        );
    }

    /// Scrolling for a stopped sub-agent's read-only conversation. Every other
    /// key keeps its ordinary meaning.
    pub(crate) fn stopped_subagent_key(&mut self, key: KeyEvent) -> Option<DashboardAction> {
        let id = match self.focus {
            Focus::Sessions if self.sessions_filter.is_none() => self.selected_session_id()?,
            Focus::Prompt => self.current_session_id()?,
            _ => return None,
        }
        .to_owned();
        let pane = self.stopped_subagents.get_mut(&id)?;
        match key.code {
            KeyCode::PageUp => pane.scroll = pane.scroll.saturating_add(20),
            KeyCode::PageDown => pane.scroll = pane.scroll.saturating_sub(20),
            KeyCode::End => pane.scroll = 0,
            _ => return None,
        }
        Some(DashboardAction::None)
    }
}
