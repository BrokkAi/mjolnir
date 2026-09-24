use super::*;
use crate::theme;
use mj_core::storage::TranscriptCursor;
use ratatui::widgets::{Paragraph, Wrap};

/// A bounded page, separate from the live feed and the person's draft.
#[derive(Default)]
pub(super) struct EarlierMessages {
    pub generation: u64,
    pub before: Option<TranscriptCursor>,
    pub requested: bool,
    pub loading: bool,
    pub loaded: bool,
    pub lines: Vec<Line<'static>>,
    pub scroll: u16,
    pub error: Option<String>,
}

impl ChatState {
    pub(super) fn open_earlier_messages(&mut self) {
        self.earlier_generation += 1;
        self.earlier = Some(EarlierMessages {
            generation: self.earlier_generation,
            requested: true,
            ..Default::default()
        });
    }

    pub(super) fn earlier_key(&mut self, code: KeyCode) -> ChatAction {
        if code == KeyCode::Esc {
            self.earlier = None;
            return ChatAction::None;
        }
        let reader = self.earlier.as_mut().expect("history reader is open");
        match code {
            KeyCode::Enter if !reader.loading && (!reader.loaded || reader.before.is_some()) => {
                reader.requested = true;
            }
            KeyCode::Up => reader.scroll = reader.scroll.saturating_sub(1),
            KeyCode::PageUp => reader.scroll = reader.scroll.saturating_sub(10),
            KeyCode::Down => reader.scroll = reader.scroll.saturating_add(1),
            KeyCode::PageDown => reader.scroll = reader.scroll.saturating_add(10),
            KeyCode::Home => reader.scroll = 0,
            _ => {}
        }
        ChatAction::None
    }
}

pub(super) fn render_earlier(frame: &mut Frame, area: Rect, reader: &mut EarlierMessages) {
    let status = if reader.loading {
        "Loading earlier messages..."
    } else if let Some(error) = reader.error.as_deref() {
        error
    } else if reader.before.is_some() {
        "Enter: earlier page | Up/Down/PgUp/PgDn: scroll | Esc: live"
    } else {
        "Beginning of conversation | Up/Down/PgUp/PgDn: scroll | Esc: live"
    };
    let block = theme::panel(true)
        .title(" Earlier messages ")
        .title_bottom(status);
    let inner = block.inner(area);
    let paragraph = Paragraph::new(reader.lines.clone()).wrap(Wrap { trim: false });
    let max_scroll = paragraph
        .line_count(inner.width)
        .saturating_sub(inner.height as usize);
    reader.scroll = reader.scroll.min(max_scroll.min(u16::MAX as usize) as u16);
    frame.render_widget(ratatui::widgets::Clear, area);
    frame.render_widget(paragraph.scroll((reader.scroll, 0)).block(block), area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_navigation_retains_the_draft_and_dismisses_without_sending() {
        let mut chat = ChatState::new(
            &WorkerSnapshot::summary("s".into(), WorkerPhase::Idle, 0),
            &[],
        );
        chat.input = "unsent draft".into();
        chat.open_earlier_messages();
        assert!(chat.earlier.as_ref().unwrap().requested);
        chat.earlier.as_mut().unwrap().loading = true;
        chat.earlier.as_mut().unwrap().requested = false;
        assert_eq!(chat.earlier_key(KeyCode::Enter), ChatAction::None);
        assert!(!chat.earlier.as_ref().unwrap().requested);
        chat.earlier_key(KeyCode::Esc);
        assert!(chat.earlier.is_none());
        assert_eq!(chat.input, "unsent draft");
    }
}
