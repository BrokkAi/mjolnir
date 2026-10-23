//! The turn reviewer's independently scrollable transcript pane.
use super::rendering::{RowCopy, TranscriptRenderMode};
use super::transcript::{copied_rows_text, materialized_chat_entries_reusing, render_entry_rows};
use super::viewport::RowViewport;
use crate::selection::SelectionRange;
use crate::theme;
use mj_core::elicitation::ElicitationRequest;
use mj_core::state::MaterializedSession;
use mj_core::transcript::ChatEntry;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
/// The reviewer's conversation, as its own scrollable pane.
#[derive(Debug, Default)]
pub(super) struct ReviewerPane {
    /// Projected reviewer session, folded from its relay events.
    session: Option<MaterializedSession>,
    entries: Vec<ChatEntry>,
    /// Wrapped rows for `width`. This is the pane's own row cache: a
    /// selection in it is resolved here and never against the primary's.
    rows: Vec<Line<'static>>,
    /// How copying each of `rows` gives back its text.
    copies: Vec<RowCopy>,
    theme: theme::UiTheme,
    width: u16,
    /// Where this pane is scrolled to.
    viewport: RowViewport,
}

impl ReviewerPane {
    pub(super) fn replace_snapshot(&mut self, session: MaterializedSession) -> bool {
        if self.session.as_ref() == Some(&session) {
            return false;
        }
        self.entries = materialized_chat_entries_reusing(&session, 0, Vec::new());
        self.session = Some(session);
        self.width = 0;
        true
    }

    pub(super) fn pending_elicitations(&self) -> &[ElicitationRequest] {
        self.session
            .as_ref()
            .map_or(&[], |session| session.pending_elicitations.as_slice())
    }

    /// Whether the reviewer has produced anything yet.
    pub(super) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn ensure_rows(&mut self, width: u16) {
        if self.width == width && self.theme == theme::current() && !self.rows.is_empty() {
            return;
        }
        self.width = width;
        self.theme = theme::current();
        self.rows.clear();
        self.copies.clear();
        for entry in &self.entries {
            let (rows, copies) =
                render_entry_rows(entry, usize::from(width), TranscriptRenderMode::Rich);
            self.rows.extend(rows);
            self.copies.extend(copies);
        }
    }

    /// Scrolls by `delta` rows, leaving follow mode on only at the end.
    pub(super) fn scroll_by(&mut self, delta: isize, height: usize) -> bool {
        self.viewport.scroll_by(delta, self.rows.len(), height)
    }

    /// The text a selection in this pane covers, resolved against this pane's
    /// rows so it can never pick up the primary transcript's.
    pub(super) fn selection_text(&self, range: &SelectionRange) -> Option<String> {
        if self.width == 0 {
            return None;
        }
        let end = range.end.row.min(self.rows.len().saturating_sub(1));
        let rows = (range.start.row..=end)
            .filter_map(|row| {
                let line = self.rows.get(row)?;
                let copy = self.copies.get(row).copied().unwrap_or_default();
                Some((line, copy, range.columns_on(row, self.width)))
            })
            .collect::<Vec<_>>();
        (!rows.is_empty()).then(|| copied_rows_text(rows, self.width))
    }
}

pub(super) fn render_reviewer_titled(
    frame: &mut ratatui::Frame,
    area: Rect,
    reviewer: &mut ReviewerPane,
    status: &str,
    title: &str,
    strip: Option<Line<'static>>,
) -> (Rect, usize, usize) {
    let block = theme::panel(false)
        .title(title.to_owned())
        .border_style(Style::default().fg(theme::palette().secondary));
    let mut inner = block.inner(area);
    frame.render_widget(block, area);
    if let Some(strip) = strip
        && inner.height > 1
    {
        let strip_area = Rect::new(inner.x, inner.y, inner.width, 1);
        frame.render_widget(Paragraph::new(strip), strip_area);
        inner = Rect::new(inner.x, inner.y + 1, inner.width, inner.height - 1);
    }
    reviewer.ensure_rows(inner.width);
    let height = usize::from(inner.height);
    if reviewer.viewport.follow {
        reviewer.viewport.top_row = reviewer.rows.len().saturating_sub(height);
    }
    let top = reviewer.viewport.top_row;
    let visible = reviewer
        .rows
        .iter()
        .skip(top)
        .take(height)
        .cloned()
        .collect::<Vec<_>>();
    let rows = if visible.is_empty() {
        vec![Line::from(Span::styled(
            status.to_owned(),
            Style::default().fg(theme::palette().muted),
        ))]
    } else {
        visible
    };
    let total = reviewer.rows.len().max(rows.len());
    frame.render_widget(Paragraph::new(rows), inner);
    (inner, top, total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::transcript::row_text;
    use mj_core::transcript::ChatRole;

    fn pane_from_entries(entries: Vec<ChatEntry>) -> ReviewerPane {
        ReviewerPane {
            entries,
            viewport: RowViewport {
                top_row: 0,
                follow: true,
            },
            ..ReviewerPane::default()
        }
    }
    #[test]
    fn reviewer_rows_adopt_the_new_theme_without_new_events() {
        let mut pane = pane_from_entries(vec![ChatEntry::plain(
            1,
            ChatRole::Agent,
            "Review the **changed behavior**.",
        )]);
        theme::with_theme(theme::UiTheme::Midnight, || pane.ensure_rows(60));
        let original = pane.rows.clone();
        theme::with_theme(theme::UiTheme::Light, || {
            pane.ensure_rows(60);
            assert_ne!(pane.rows, original);
            assert_eq!(
                pane.rows.iter().map(row_text).collect::<Vec<_>>(),
                original.iter().map(row_text).collect::<Vec<_>>()
            );
            assert!(
                pane.rows
                    .iter()
                    .flat_map(|line| &line.spans)
                    .any(|span| { span.style.fg == Some(theme::palette().accent) })
            );
        });
    }

    #[test]
    fn the_reviewer_pane_scrolls_and_copies_from_its_own_rows() {
        let entries = (0..40)
            .map(|index| {
                ChatEntry::plain(index + 1, ChatRole::Agent, format!("reviewer line {index}"))
            })
            .collect::<Vec<_>>();
        let mut pane = pane_from_entries(entries);
        pane.ensure_rows(40);
        let total = pane.rows.len();
        assert!(total > 10, "the fixture must not fit on one screen");

        // Scrolling stops at the last full screen rather than running past it.
        pane.scroll_by(1_000, 10);
        assert_eq!(pane.viewport.top_row, total - 10);
        pane.scroll_by(-1_000, 10);
        assert_eq!(pane.viewport.top_row, 0);

        let text = pane
            .selection_text(&SelectionRange {
                start: crate::selection::ContentPos::new(0, 0),
                end: crate::selection::ContentPos::new(1, 39),
            })
            .expect("a selection over this pane's rows resolves here");
        assert!(
            text.contains("reviewer line 0"),
            "the pane resolves its own rows: {text:?}"
        );
    }

    #[test]
    fn the_reviewer_pane_copies_a_wrapped_url_whole() {
        let url = "https://example.com/a/rather/long/path/that/cannot/fit/on/one/row";
        let mut pane = pane_from_entries(vec![ChatEntry::plain(
            1,
            ChatRole::Agent,
            format!("See {url} for details."),
        )]);
        pane.ensure_rows(30);
        let last = pane.rows.len() - 2;

        let text = pane
            .selection_text(&SelectionRange {
                start: crate::selection::ContentPos::new(1, 0),
                end: crate::selection::ContentPos::new(last, 29),
            })
            .expect("a selection over this pane's rows resolves here");
        assert_eq!(text, format!("See {url} for details."));
    }
}
