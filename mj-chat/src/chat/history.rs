//! Prompt history: reverse-i-search over the stored prompts and the up/down
//! walk through this session's earlier prompts.

use crate::theme;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use crate::text_input::TextInput;
use mj_core::storage::{
    HistoryCursor, HistoryScope, MIN_HISTORY_QUERY_CHARS, PromptHistoryEntry, PromptHistoryPage,
};

use super::{ChatDaemonRequest, ChatState, PromptPayload};

/// How many matches the search keeps fetched beyond the selected one, so
/// stepping to an older match never waits on the store.
const HISTORY_LOOKAHEAD: usize = 8;

#[derive(Debug, Clone)]
pub(super) struct HistorySearch {
    original_input: String,
    original_cursor: usize,
    original_images: Vec<super::PromptImage>,
    generation: u64,
    pub(super) query: TextInput,
    pub(super) scope: HistoryScope,
    /// Every match fetched for this query, in the order the store listed
    /// them, each text once.
    matches: Vec<PromptHistoryEntry>,
    selected: Option<usize>,
    /// Where the next page starts. `None` before the first page arrives and
    /// once the store has no more matches; `fetching` tells the two apart.
    next: Option<HistoryCursor>,
    /// A page request is queued or in flight for this generation.
    fetching: bool,
    unavailable: Option<String>,
}

impl HistorySearch {
    /// Fetched matches not yet stepped to.
    fn ahead(&self) -> usize {
        match self.selected {
            Some(selected) => self.matches.len().saturating_sub(selected + 1),
            None => self.matches.len(),
        }
    }

    /// Whether the store may hold matches the search has not fetched.
    fn more_may_follow(&self) -> bool {
        self.fetching || self.next.is_some()
    }
}

#[derive(Debug, Clone)]
pub(super) struct HistorySearchRequest {
    pub(super) generation: u64,
    bundle_id: Option<String>,
    scope: HistoryScope,
    query: String,
    after: Option<HistoryCursor>,
    limit: usize,
}

impl ChatState {
    /// `entries` are this session's stored prompts, newest first.
    pub(super) fn set_session_history(&mut self, entries: Vec<PromptHistoryEntry>) {
        self.session_history_error = None;
        self.session_history = entries.into_iter().rev().map(|entry| entry.text).collect();
        if let Some(index) = self.history_index
            && index >= self.navigation_history().len()
        {
            self.history_index = None;
            self.history_draft.clear();
            self.history_draft_images.clear();
        }
    }

    pub(super) fn set_session_history_unavailable(&mut self, error: String) {
        if self.session_history_error.as_deref() != Some(error.as_str()) {
            self.session_history_error = Some(error);
        }
    }

    pub(super) fn begin_history_search(&mut self) {
        self.autocomplete = None;
        self.history_search = Some(HistorySearch {
            original_input: self.input.clone(),
            original_cursor: self.input_cursor,
            original_images: self.input_images.clone(),
            generation: self.next_history_search_generation,
            query: TextInput::new(),
            scope: HistoryScope::All,
            matches: Vec::new(),
            selected: None,
            next: None,
            fetching: false,
            unavailable: None,
        });
        self.pending_history_search = None;
    }

    /// Start the search again after its query or scope changed.
    pub(super) fn refresh_history_search(&mut self) {
        let generation = self.next_history_search_generation.wrapping_add(1);
        let Some(search) = self.history_search.as_mut() else {
            return;
        };
        self.next_history_search_generation = generation;
        search.generation = generation;
        search.matches.clear();
        search.selected = None;
        search.next = None;
        search.fetching = false;
        self.pending_history_search = None;
        let searchable = search.query.chars().count() >= MIN_HISTORY_QUERY_CHARS;
        let original = PromptPayload {
            text: search.original_input.clone(),
            images: search.original_images.clone(),
        };
        let original_cursor = search.original_cursor;
        self.set_input_payload(original);
        self.input_cursor = original_cursor;
        if searchable {
            // The first match is shown, so the first page also fills the
            // queue behind it.
            self.queue_history_page(None, HISTORY_LOOKAHEAD + 1);
        }
    }

    fn queue_history_page(&mut self, after: Option<HistoryCursor>, limit: usize) {
        let Some(search) = self.history_search.as_mut() else {
            return;
        };
        search.fetching = true;
        self.pending_history_search = Some(HistorySearchRequest {
            generation: search.generation,
            bundle_id: self.bundle_id.clone(),
            scope: search.scope,
            query: search.query.to_string(),
            after,
            limit,
        });
    }

    /// Fetch the next page when fewer than [`HISTORY_LOOKAHEAD`] matches lie
    /// ahead of the selected one and the store may have more.
    fn refill_history_search(&mut self) {
        let Some(search) = self.history_search.as_ref() else {
            return;
        };
        let ahead = search.ahead();
        if search.fetching || ahead >= HISTORY_LOOKAHEAD {
            return;
        }
        if let Some(after) = search.next {
            self.queue_history_page(Some(after), HISTORY_LOOKAHEAD - ahead);
        }
    }

    pub(super) fn take_history_search_request(&mut self) -> Option<HistorySearchRequest> {
        self.pending_history_search.take()
    }

    pub(super) fn take_abandoned_prompt_request(&mut self) -> Option<ChatDaemonRequest> {
        self.pending_abandoned_prompt.take()
    }

    pub(super) fn apply_history_search_results(
        &mut self,
        generation: u64,
        result: std::result::Result<PromptHistoryPage, String>,
    ) {
        let Some(search) = self.history_search.as_mut() else {
            return;
        };
        if search.generation != generation {
            return;
        }
        search.fetching = false;
        match result {
            Ok(page) => {
                // A page repeats text an earlier page already listed when the
                // same prompt was sent in several scopes; keep the first.
                for entry in page.entries {
                    if !search.matches.iter().any(|known| known.text == entry.text) {
                        search.matches.push(entry);
                    }
                }
                search.next = page.next;
                search.unavailable = None;
                if search.selected.is_none()
                    && let Some(first) = search.matches.first()
                {
                    let text = first.text.clone();
                    search.selected = Some(0);
                    self.set_input(text);
                }
                self.refill_history_search();
            }
            Err(error) => {
                let original = PromptPayload {
                    text: search.original_input.clone(),
                    images: search.original_images.clone(),
                };
                let original_cursor = search.original_cursor;
                self.set_input_payload(original);
                self.input_cursor = original_cursor;
                self.history_search = None;
                self.set_notice(format!("History unavailable: {error}"));
                self.update_autocomplete();
            }
        }
    }

    pub(super) async fn resolve_history_search_request(
        request: HistorySearchRequest,
        session: &mj_client::session::SessionHandle,
    ) -> std::result::Result<PromptHistoryPage, String> {
        let Some(bundle_id) = request.bundle_id else {
            return Err("this chat has no project to search".to_owned());
        };
        session
            .search_prompts(
                bundle_id,
                request.scope,
                request.query,
                request.after,
                request.limit,
            )
            .await
            .map_err(|error| format!("{error:#}"))
    }

    fn step_history_search(&mut self, direction: isize) {
        let Some(search) = self.history_search.as_mut() else {
            return;
        };
        if search.matches.is_empty() {
            return;
        }
        let current = search.selected.unwrap_or(0);
        let next = if direction.is_negative() {
            current.saturating_sub(1)
        } else {
            (current + 1).min(search.matches.len() - 1)
        };
        search.selected = Some(next);
        let text = search.matches[next].text.clone();
        self.set_input(text);
        self.refill_history_search();
    }

    fn cycle_history_scope(&mut self) {
        let Some(search) = self.history_search.as_mut() else {
            return;
        };
        // Each press narrows the search; the last wraps back to the widest.
        search.scope = match search.scope {
            HistoryScope::All => HistoryScope::Project,
            HistoryScope::Project => HistoryScope::Session,
            HistoryScope::Session => HistoryScope::All,
        };
        self.refresh_history_search();
    }

    fn cancel_history_search(&mut self) {
        let Some(search) = self.history_search.take() else {
            return;
        };
        self.set_input_payload(PromptPayload {
            text: search.original_input,
            images: search.original_images,
        });
        self.input_cursor = search.original_cursor;
        self.update_autocomplete();
    }

    fn accept_history_search(&mut self) {
        if self
            .history_search
            .as_ref()
            .is_some_and(|search| search.selected.is_some())
        {
            self.history_search = None;
            self.history_index = None;
            self.update_autocomplete();
        }
    }

    /// Keys while reverse-i-search is open. The search owns every key, so
    /// nothing typed here reaches the composer.
    pub(super) fn handle_history_search_key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        if modifiers.contains(KeyModifiers::ALT) && code == KeyCode::Char('r') {
            self.cycle_history_scope();
            return;
        }
        if modifiers.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('r')
            || code == KeyCode::Up
        {
            self.step_history_search(1);
            return;
        }
        if modifiers.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('s')
            || code == KeyCode::Down
        {
            self.step_history_search(-1);
            return;
        }
        if code == KeyCode::Esc
            || modifiers.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('c')
        {
            self.cancel_history_search();
            return;
        }
        if code == KeyCode::Enter {
            self.accept_history_search();
            return;
        }
        let changed = self.history_search.as_mut().is_some_and(|search| {
            search
                .query
                .handle_key(KeyEvent::new(code, modifiers))
                .changed()
        });
        if changed {
            self.refresh_history_search();
        }
    }

    pub(super) fn record_prompt_history(&mut self, prompt: &str) {
        if self.prompt_history.last().is_none_or(|last| last != prompt) {
            self.prompt_history.push(prompt.to_owned());
        }
        self.history_index = None;
        self.history_draft.clear();
        self.history_draft_images.clear();
    }

    fn navigation_history(&self) -> Vec<String> {
        let mut history = self.session_history.clone();
        history.extend(self.prompt_history.iter().cloned());
        history
    }

    pub(super) fn move_history(&mut self, delta: isize) {
        let history = self.navigation_history();
        if let Some(error) = self
            .bundle_id
            .as_ref()
            .and(self.session_history_error.as_ref())
        {
            self.set_notice(format!("History unavailable: {error}"));
        }
        if history.is_empty() {
            return;
        }
        let next = match (self.history_index, delta.is_negative()) {
            (None, true) => {
                self.history_draft.clone_from(&self.input);
                self.history_draft_images.clone_from(&self.input_images);
                Some(history.len() - 1)
            }
            (None, false) => None,
            (Some(index), true) => Some(index.saturating_sub(1)),
            (Some(index), false) if index + 1 < history.len() => Some(index + 1),
            (Some(_), false) => None,
        };
        if let Some(input) = next.and_then(|index| history.get(index).cloned()) {
            // Stored history is text-only. Clear any tracked image ranges so
            // attachments from the temporary draft cannot follow unrelated
            // text into a submission.
            self.set_input(input);
        } else {
            self.set_input_payload(PromptPayload {
                text: self.history_draft.clone(),
                images: self.history_draft_images.clone(),
            });
        }
        self.history_index = next;
        self.input_cursor = self.input.len();
        self.preferred_column = None;
        self.update_autocomplete();
    }
}

pub(super) fn history_scope_name(scope: HistoryScope) -> &'static str {
    match scope {
        HistoryScope::Session => "session",
        HistoryScope::Project => "project",
        HistoryScope::All => "all projects",
    }
}

pub(super) fn history_search_footer(search: &HistorySearch) -> String {
    let mut footer = format!(
        "reverse-i-search [{}]: {}",
        history_scope_name(search.scope),
        search.query
    );
    let query_chars = search.query.chars().count();
    if let Some(error) = search.unavailable.as_deref() {
        footer.push_str(&format!("  unavailable: {error}"));
    } else if query_chars > 0 && query_chars < MIN_HISTORY_QUERY_CHARS {
        footer.push_str(&format!(
            "  type {MIN_HISTORY_QUERY_CHARS}+ characters · Alt-R scope · Esc cancel"
        ));
    } else if query_chars > 0 && search.matches.is_empty() {
        footer.push_str(if search.fetching {
            "  searching…"
        } else {
            "  no match"
        });
    } else if let Some(selected) = search.selected {
        footer.push_str(&format!(
            "  {}/{}{} · Enter accept · Esc cancel · Alt-R scope",
            selected + 1,
            search.matches.len(),
            if search.more_may_follow() { "+" } else { "" }
        ));
    } else {
        footer.push_str("  type to search · Alt-R scope · Esc cancel");
    }
    footer
}

pub(super) fn highlighted_input_lines(input: &str, query: &str) -> Vec<Line<'static>> {
    let ranges = case_insensitive_match_ranges(input, query);
    let mut lines = Vec::new();
    let mut line_start = 0;
    for line in input.split('\n') {
        let line_end = line_start + line.len();
        let mut spans = Vec::new();
        let mut cursor = line_start;
        for range in ranges
            .iter()
            .filter(|range| range.start < line_end && range.end > line_start)
        {
            let start = range.start.max(line_start);
            let end = range.end.min(line_end);
            if cursor < start {
                spans.push(Span::raw(input[cursor..start].to_owned()));
            }
            spans.push(Span::styled(
                input[start..end].to_owned(),
                theme::filled(theme::palette().warning).add_modifier(Modifier::BOLD),
            ));
            cursor = end;
        }
        if cursor < line_end {
            spans.push(Span::raw(input[cursor..line_end].to_owned()));
        }
        lines.push(Line::from(spans));
        line_start = line_end.saturating_add(1);
    }
    lines
}

fn case_insensitive_match_ranges(text: &str, query: &str) -> Vec<std::ops::Range<usize>> {
    if query.is_empty() {
        return Vec::new();
    }
    let query = query
        .chars()
        .flat_map(char::to_lowercase)
        .collect::<String>();
    let mut folded = String::new();
    let mut spans = Vec::new();
    for (start, character) in text.char_indices() {
        let original = start..start + character.len_utf8();
        for lower in character.to_lowercase() {
            let folded_start = folded.len();
            folded.push(lower);
            spans.push((folded_start..folded.len(), original.clone()));
        }
    }
    let mut ranges = Vec::new();
    let mut from = 0;
    while from <= folded.len()
        && let Some(relative) = folded[from..].find(&query)
    {
        let start = from + relative;
        let end = start + query.len();
        let first = spans
            .iter()
            .find(|(range, _)| range.end > start && range.start < end)
            .map(|(_, original)| original.start);
        let last = spans
            .iter()
            .rev()
            .find(|(range, _)| range.end > start && range.start < end)
            .map(|(_, original)| original.end);
        if let (Some(first), Some(last)) = (first, last) {
            ranges.push(first..last);
        }
        from = end;
    }
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::ChatAction;
    use crate::chat::test_support::{ctrl, key, snapshot};
    use crate::clipboard::{ClipboardContent, ClipboardImage};
    use crossterm::event::KeyCode;

    fn entry(id: i64, text: &str) -> PromptHistoryEntry {
        PromptHistoryEntry {
            id,
            session_id: "1234567890".into(),
            text: text.into(),
        }
    }

    fn type_query(chat: &mut ChatState, query: &str) {
        for character in query.chars() {
            chat.handle_key(key(KeyCode::Char(character)));
        }
    }

    /// Answers the queued page request with `texts` and `next`, returning the
    /// request it answered.
    fn answer_history_page(
        chat: &mut ChatState,
        texts: &[&str],
        next: Option<HistoryCursor>,
    ) -> HistorySearchRequest {
        let request = chat
            .take_history_search_request()
            .expect("history search request was queued");
        let entries = texts
            .iter()
            .enumerate()
            .map(|(index, text)| entry(index as i64, text))
            .collect();
        chat.apply_history_search_results(
            request.generation,
            Ok(PromptHistoryPage { entries, next }),
        );
        request
    }

    fn image_draft(chat: &mut ChatState) -> Vec<super::super::PromptImage> {
        chat.set_prompt_images_supported(true);
        chat.set_input("unfinished".into());
        chat.handle_clipboard_content(ClipboardContent::Image(ClipboardImage {
            data_base64: "synthetic-png".into(),
            mime_type: "image/png".into(),
            reference: None,
        }));
        assert!(!chat.input_images.is_empty());
        chat.input_images.clone()
    }

    // Hard-won: e557e89: shipped Ctrl-C did nothing, forcing users to retype abandoned prompts
    #[test]
    fn control_c_stashes_the_typed_prompt_into_history_and_clears_the_input() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        for character in "draft prompt".chars() {
            chat.handle_key(key(KeyCode::Char(character)));
        }

        assert_eq!(chat.handle_key(ctrl('c')), ChatAction::None);
        assert!(chat.input.is_empty());
        assert_eq!(chat.input_cursor, 0);

        // Repeated cancellation on the empty composer keeps the draft recoverable.
        assert_eq!(chat.handle_key(ctrl('c')), ChatAction::None);
        assert_eq!(chat.handle_key(ctrl('c')), ChatAction::None);
        assert!(chat.input.is_empty());

        chat.handle_key(key(KeyCode::Up));
        assert_eq!(chat.input, "draft prompt");
    }

    #[test]
    fn stale_history_search_results_are_dropped() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_history_context("bundle");
        chat.set_input("draft".into());

        chat.handle_key(ctrl('r'));
        type_query(&mut chat, "fix");
        let stale = chat
            .take_history_search_request()
            .expect("first search request");
        chat.handle_key(key(KeyCode::Char(' ')));

        chat.apply_history_search_results(
            stale.generation,
            Ok(PromptHistoryPage {
                entries: vec![entry(1, "fix parser")],
                next: None,
            }),
        );
        assert_eq!(chat.input, "draft");

        answer_history_page(&mut chat, &["fix renderer"], None);
        assert_eq!(chat.input, "fix renderer");
    }

    #[test]
    fn history_search_keeps_eight_matches_queued_and_refills_from_the_cursor() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_history_context("bundle");
        chat.handle_key(ctrl('r'));
        type_query(&mut chat, "fi");
        assert!(
            chat.take_history_search_request().is_none(),
            "two characters are too short for the index"
        );
        assert!(history_search_footer(chat.history_search.as_ref().unwrap()).contains("type 3+"));
        chat.handle_key(key(KeyCode::Char('x')));

        // The first page fills the shown match and the eight behind it.
        let first_page: Vec<String> = (0..9).map(|index| format!("fix {index}")).collect();
        let first_page: Vec<&str> = first_page.iter().map(String::as_str).collect();
        let cursor = HistoryCursor {
            tier: 1,
            before: 40,
        };
        let request = answer_history_page(&mut chat, &first_page, Some(cursor));
        assert_eq!((request.after, request.limit), (None, 9));
        assert_eq!(chat.input, "fix 0");
        assert!(
            chat.take_history_search_request().is_none(),
            "eight matches are already queued"
        );
        assert!(history_search_footer(chat.history_search.as_ref().unwrap()).contains("1/9+"));

        // Stepping leaves seven ahead, so the next page resumes at the cursor.
        chat.handle_key(ctrl('r'));
        assert_eq!(chat.input, "fix 1");
        let later = HistoryCursor { tier: 2, before: 7 };
        // A repeat of a listed prompt, as a wider tier returns it, is dropped
        // and the shortfall is requested again at once.
        let request = answer_history_page(&mut chat, &["fix 3"], Some(later));
        assert_eq!((request.after, request.limit), (Some(cursor), 1));
        let request = answer_history_page(&mut chat, &["fix elsewhere"], None);
        assert_eq!((request.after, request.limit), (Some(later), 1));

        for _ in 0..20 {
            chat.handle_key(ctrl('r'));
        }
        assert_eq!(chat.input, "fix elsewhere");
        assert!(
            chat.take_history_search_request().is_none(),
            "the store reported no more matches"
        );
        assert!(history_search_footer(chat.history_search.as_ref().unwrap()).contains("10/10 "));
    }

    #[test]
    fn cancelling_reverse_search_restores_inline_image_ranges() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        let images = image_draft(&mut chat);
        let draft = chat.input.clone();
        chat.set_history_context("bundle");

        chat.handle_key(ctrl('r'));
        type_query(&mut chat, "his");
        answer_history_page(&mut chat, &["old history prompt"], None);
        assert_eq!(chat.input, "old history prompt");
        assert!(chat.input_images.is_empty());

        chat.handle_key(key(KeyCode::Esc));
        assert_eq!(chat.input, draft);
        assert_eq!(chat.input_images, images);
    }

    #[test]
    fn history_navigation_restores_unsent_image_draft_and_clears_it_for_history() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        let images = image_draft(&mut chat);
        let draft = chat.input.clone();
        chat.prompt_history = vec!["unrelated history prompt".into()];

        chat.move_history(-1);
        assert_eq!(chat.input, "unrelated history prompt");
        assert!(chat.input_images.is_empty());

        chat.move_history(1);
        assert_eq!(chat.input, draft);
        assert_eq!(chat.input_images, images);

        chat.move_history(-1);
        assert_eq!(chat.input, "unrelated history prompt");
        assert!(chat.input_images.is_empty());
    }

    // Hard-won: b6b9787: this session's prompts appeared interleaved with other sessions' history
    #[test]
    fn move_history_walks_only_this_sessions_prompts() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_history_context("bundle");
        chat.set_session_history(vec![
            PromptHistoryEntry {
                id: 3,
                session_id: "1234567890".into(),
                text: "mine newest".into(),
            },
            PromptHistoryEntry {
                id: 1,
                session_id: "1234567890".into(),
                text: "mine oldest".into(),
            },
        ]);
        chat.record_prompt_history("this visit");

        let mut recalled = Vec::new();
        for _ in 0..4 {
            chat.handle_key(key(KeyCode::Up));
            recalled.push(chat.input.clone());
        }
        assert_eq!(
            recalled,
            vec!["this visit", "mine newest", "mine oldest", "mine oldest"]
        );
    }
}
