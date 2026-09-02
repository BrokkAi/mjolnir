//! Prompt history: reverse-i-search over the stored prompts and the up/down
//! walk through this session's and this project's earlier prompts.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::hel_text_input::TextInput;
use hel::hel_database::{HistoryScope, PromptHistoryEntry};

use super::ChatState;

#[derive(Debug, Clone)]
pub(super) struct HistorySearch {
    original_input: String,
    original_cursor: usize,
    generation: u64,
    pub(super) query: TextInput,
    pub(super) scope: HistoryScope,
    matches: Vec<PromptHistoryEntry>,
    selected: Option<usize>,
    unavailable: Option<String>,
}

#[derive(Debug, Clone)]
pub(super) struct HistorySearchRequest {
    pub(super) generation: u64,
    session_id: String,
    bundle_id: Option<String>,
    scope: HistoryScope,
    query: String,
    local_history: Vec<String>,
}

impl ChatState {
    pub(super) fn set_project_history(&mut self, entries: Vec<PromptHistoryEntry>) {
        self.project_history_error = None;
        // Entries arrive newest-first; split this session's prompts from the
        // rest of the project so history navigation reaches them first.
        let (session, project): (Vec<_>, Vec<_>) = entries
            .into_iter()
            .partition(|entry| entry.session_id == self.session_id);
        self.session_history = session.into_iter().rev().map(|entry| entry.text).collect();
        self.project_history = project.into_iter().rev().map(|entry| entry.text).collect();
        if let Some(index) = self.history_index
            && index >= self.navigation_history().len()
        {
            self.history_index = None;
            self.history_draft.clear();
        }
    }

    pub(super) fn set_project_history_unavailable(&mut self, error: String) {
        self.project_history_error = Some(error);
    }

    pub(super) fn begin_history_search(&mut self) {
        self.autocomplete = None;
        self.history_search = Some(HistorySearch {
            original_input: self.input.clone(),
            original_cursor: self.input_cursor,
            generation: self.next_history_search_generation,
            query: TextInput::new(),
            scope: HistoryScope::Project,
            matches: Vec::new(),
            selected: None,
            unavailable: None,
        });
        self.pending_history_search = None;
    }

    pub(super) fn refresh_history_search(&mut self) {
        let Some(search) = self.history_search.as_ref() else {
            return;
        };
        let generation = self.next_history_search_generation.wrapping_add(1);
        self.next_history_search_generation = generation;
        let query = search.query.to_string();
        let scope = search.scope;
        if let Some(search) = self.history_search.as_mut() {
            search.generation = generation;
        }
        if query.is_empty() {
            self.pending_history_search = None;
            self.apply_history_search_results(generation, Ok(Vec::new()));
        } else {
            self.pending_history_search = Some(HistorySearchRequest {
                generation,
                session_id: self.session_id.clone(),
                bundle_id: self.bundle_id.clone(),
                scope,
                query,
                local_history: self.prompt_history.clone(),
            });
        }
    }

    pub(super) fn take_history_search_request(&mut self) -> Option<HistorySearchRequest> {
        self.pending_history_search.take()
    }

    pub(super) fn apply_history_search_results(
        &mut self,
        generation: u64,
        result: std::result::Result<Vec<PromptHistoryEntry>, String>,
    ) {
        let Some(search) = self.history_search.as_ref() else {
            return;
        };
        if search.generation != generation {
            return;
        }
        let original = (search.original_input.clone(), search.original_cursor);
        match result {
            Ok(matches) => {
                if let Some(search) = self.history_search.as_mut() {
                    search.matches = matches;
                    search.selected = (!search.matches.is_empty()).then_some(0);
                    search.unavailable = None;
                }
                if let Some(text) = self
                    .history_search
                    .as_ref()
                    .and_then(|search| search.matches.first())
                    .map(|entry| entry.text.clone())
                {
                    self.input_cursor = text.len();
                    self.input = text;
                } else {
                    self.input = original.0;
                    self.input_cursor = original.1;
                }
            }
            Err(error) => {
                self.input = original.0;
                self.input_cursor = original.1;
                self.history_search = None;
                self.notices.set(format!("History unavailable: {error}"));
                self.update_autocomplete();
            }
        }
    }

    fn local_history_search_results(request: &HistorySearchRequest) -> Vec<PromptHistoryEntry> {
        request
            .local_history
            .iter()
            .rev()
            .enumerate()
            .filter(|(_, text)| text.to_lowercase().contains(&request.query.to_lowercase()))
            .map(|(index, text)| PromptHistoryEntry {
                id: -(index as i64) - 1,
                session_id: request.session_id.clone(),
                text: text.clone(),
            })
            .collect()
    }

    pub(super) fn resolve_history_search_request(
        request: HistorySearchRequest,
    ) -> std::result::Result<Vec<PromptHistoryEntry>, String> {
        if let Some(bundle_id) = request.bundle_id.as_deref() {
            hel::hel_database::search_prompts(
                &request.session_id,
                bundle_id,
                request.scope,
                &request.query,
            )
            .map_err(|error| format!("{error:#}"))
        } else {
            Ok(Self::local_history_search_results(&request))
        }
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
        self.input = search.matches[next].text.clone();
        self.input_cursor = self.input.len();
    }

    fn cycle_history_scope(&mut self) {
        let Some(search) = self.history_search.as_mut() else {
            return;
        };
        search.scope = match search.scope {
            HistoryScope::Project => HistoryScope::Session,
            HistoryScope::Session => HistoryScope::All,
            HistoryScope::All => HistoryScope::Project,
        };
        self.refresh_history_search();
    }

    fn cancel_history_search(&mut self) {
        let Some(search) = self.history_search.take() else {
            return;
        };
        self.input = search.original_input;
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
    }

    fn navigation_history(&self) -> Vec<String> {
        let mut history = self.project_history.clone();
        history.extend(self.session_history.iter().cloned());
        history.extend(self.prompt_history.iter().cloned());
        history
    }

    pub(super) fn move_history(&mut self, delta: isize) {
        let history = self.navigation_history();
        if let Some(error) = self
            .bundle_id
            .as_ref()
            .and(self.project_history_error.as_ref())
        {
            self.notices.set(format!("History unavailable: {error}"));
        }
        if history.is_empty() {
            return;
        }
        let next = match (self.history_index, delta.is_negative()) {
            (None, true) => {
                self.history_draft.clone_from(&self.input);
                Some(history.len() - 1)
            }
            (None, false) => None,
            (Some(index), true) => Some(index.saturating_sub(1)),
            (Some(index), false) if index + 1 < history.len() => Some(index + 1),
            (Some(_), false) => None,
        };
        self.history_index = next;
        let input = next
            .and_then(|index| history.get(index).cloned())
            .unwrap_or_else(|| self.history_draft.clone());
        self.input = input;
        self.input_cursor = self.input.len();
        self.preferred_column = None;
        self.update_autocomplete();
    }
}

pub(super) fn history_scope_name(scope: HistoryScope) -> &'static str {
    match scope {
        HistoryScope::Project => "project",
        HistoryScope::Session => "session",
        HistoryScope::All => "all projects",
    }
}

pub(super) fn history_search_footer(search: &HistorySearch) -> String {
    let mut footer = format!(
        "reverse-i-search [{}]: {}",
        history_scope_name(search.scope),
        search.query
    );
    if let Some(error) = search.unavailable.as_deref() {
        footer.push_str(&format!("  unavailable: {error}"));
    } else if !search.query.is_empty() && search.matches.is_empty() {
        footer.push_str("  no match");
    } else if let Some(selected) = search.selected {
        footer.push_str(&format!(
            "  {}/{} · Enter accept · Esc cancel · Alt-R scope",
            selected + 1,
            search.matches.len()
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
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
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
    use crate::hel_chat::ChatAction;
    use crate::hel_chat::test_support::{alt, ctrl, key, snapshot};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn apply_pending_history_search(chat: &mut ChatState) {
        let request = chat
            .take_history_search_request()
            .expect("history search request was queued");
        let generation = request.generation;
        let result = ChatState::resolve_history_search_request(request);
        chat.apply_history_search_results(generation, result);
    }

    #[test]
    fn control_c_stashes_the_typed_prompt_into_history_and_clears_the_input() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        for character in "draft prompt".chars() {
            chat.handle_key(key(KeyCode::Char(character)));
        }

        assert_eq!(chat.handle_key(ctrl('c')), ChatAction::None);
        assert!(chat.input.is_empty());
        assert_eq!(chat.input_cursor, 0);

        chat.handle_key(key(KeyCode::Up));
        assert_eq!(chat.input, "draft prompt");
    }

    #[test]
    fn reverse_search_previews_steps_accepts_and_restores_draft() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.prompt_history = vec!["fix parser".into(), "fix renderer".into()];
        chat.set_input("unfinished".into());
        chat.handle_key(ctrl('r'));
        chat.handle_key(key(KeyCode::Char('f')));
        apply_pending_history_search(&mut chat);
        chat.handle_key(key(KeyCode::Char('i')));
        apply_pending_history_search(&mut chat);
        chat.handle_key(key(KeyCode::Char('x')));
        apply_pending_history_search(&mut chat);
        assert_eq!(chat.input, "fix renderer");
        chat.handle_key(ctrl('r'));
        assert_eq!(chat.input, "fix parser");
        chat.handle_key(key(KeyCode::Esc));
        assert_eq!(chat.input, "unfinished");

        chat.handle_key(ctrl('r'));
        for character in "renderer".chars() {
            chat.handle_key(key(KeyCode::Char(character)));
            apply_pending_history_search(&mut chat);
        }
        chat.handle_key(key(KeyCode::Enter));
        assert_eq!(chat.input, "fix renderer");
        assert!(chat.history_search.is_none());
    }

    #[test]
    fn stale_history_search_results_are_dropped() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.prompt_history = vec!["fix parser".into(), "fix renderer".into()];
        chat.set_input("draft".into());

        chat.handle_key(ctrl('r'));
        chat.handle_key(key(KeyCode::Char('f')));
        let stale = chat
            .take_history_search_request()
            .expect("first search request");
        chat.handle_key(key(KeyCode::Char('i')));
        let current = chat
            .take_history_search_request()
            .expect("second search request");

        chat.apply_history_search_results(
            stale.generation,
            Ok(vec![PromptHistoryEntry {
                id: 1,
                session_id: "1234567890".into(),
                text: "fix parser".into(),
            }]),
        );
        assert_eq!(chat.input, "draft");

        let generation = current.generation;
        let result = ChatState::resolve_history_search_request(current);
        chat.apply_history_search_results(generation, result);
        assert_eq!(chat.input, "fix renderer");
    }

    #[test]
    fn move_history_uses_prefetched_project_history_and_local_prompts() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_history_context("bundle");
        chat.set_project_history(vec![
            PromptHistoryEntry {
                id: 2,
                session_id: "other".into(),
                text: "project newest".into(),
            },
            PromptHistoryEntry {
                id: 1,
                session_id: "other".into(),
                text: "project oldest".into(),
            },
        ]);
        chat.record_prompt_history("local prompt");

        chat.handle_key(key(KeyCode::Up));
        assert_eq!(chat.input, "local prompt");
        chat.handle_key(key(KeyCode::Up));
        assert_eq!(chat.input, "project newest");
        chat.handle_key(key(KeyCode::Up));
        assert_eq!(chat.input, "project oldest");
    }

    #[test]
    fn move_history_walks_this_session_before_the_rest_of_the_project() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_history_context("bundle");
        // Newest-first, with this session's prompts interleaved with another's.
        chat.set_project_history(vec![
            PromptHistoryEntry {
                id: 4,
                session_id: "other".into(),
                text: "other newest".into(),
            },
            PromptHistoryEntry {
                id: 3,
                session_id: "1234567890".into(),
                text: "mine newest".into(),
            },
            PromptHistoryEntry {
                id: 2,
                session_id: "other".into(),
                text: "other oldest".into(),
            },
            PromptHistoryEntry {
                id: 1,
                session_id: "1234567890".into(),
                text: "mine oldest".into(),
            },
        ]);
        chat.record_prompt_history("this visit");

        let mut recalled = Vec::new();
        for _ in 0..5 {
            chat.handle_key(key(KeyCode::Up));
            recalled.push(chat.input.clone());
        }
        assert_eq!(
            recalled,
            vec![
                "this visit",
                "mine newest",
                "mine oldest",
                "other newest",
                "other oldest",
            ]
        );
    }

    /// Ctrl-R opens the reverse search, as readline does; once it is open
    /// Alt-R keeps its older job of cycling which history the search reads.
    #[test]
    fn ctrl_r_opens_history_search_and_alt_r_inside_it_cycles_scope() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        // Alt-R does not open one: it belongs to the open search's scope.
        chat.handle_key(alt('r'));
        assert!(chat.history_search.is_none());

        chat.handle_key(ctrl('r'));
        assert!(chat.history_search.is_some());
        chat.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::ALT));
        assert_eq!(
            chat.history_search.as_ref().unwrap().scope,
            HistoryScope::Session
        );
        chat.handle_key(ctrl('c'));
        assert!(chat.history_search.is_none());
    }
}
