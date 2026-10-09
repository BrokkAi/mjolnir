use super::*;
use crate::chat::ChatAction;
use crate::chat::test_support::{
    agent_message_item, drawn_transcript, drawn_transcript_selecting, key, line_text, mouse_in,
    queued, snapshot, transcript_text,
};
use crate::selection::SelectionState;
use agent_client_protocol::schema::v1::ToolCallStatus;
use agent_client_protocol::schema::v1::ToolKind;
use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use mj_client::web::BrowserTranscriptEntry;
use mj_core::acp::RuntimeEvent;
use mj_core::relay::{SequencedEvent, WorkerEvent};
use mj_transcript::transcript::{TerminalOutputRecord, tool_content_details};

#[test]
fn background_conversion_reuses_unchanged_entries_and_refreshes_changed_text() {
    let mut session = MaterializedSession::empty("incremental");
    session.applied_event_ordinal = 2;
    session.transcript = vec![
        user_transcript_item(1, "first"),
        user_transcript_item(2, "second"),
    ];
    let first = TranscriptSnapshot::from_materialized(&session);
    let previous = first.converted_entries();
    session.applied_event_ordinal = 3;
    session.transcript[1] = user_transcript_item(2, "edited");
    session.transcript.push(user_transcript_item(3, "third"));
    let reused = TranscriptSnapshot::from_materialized_reusing(
        &session,
        &BTreeMap::new(),
        &previous,
        &BTreeMap::new(),
    );
    assert_eq!(reused.entries[0].revision, previous[0].revision);
    assert_eq!(reused.entries[1].text, "edited");
    assert_eq!(
        reused.browser_tail(100),
        TranscriptSnapshot::from_materialized(&session).browser_tail(100)
    );
}

#[test]
fn background_conversion_refreshes_added_and_removed_diffstats() {
    let mut session = MaterializedSession::empty("diffstats");
    session.applied_event_ordinal = 1;
    session.transcript = vec![fixture_tool_item(1)];
    let empty = BTreeMap::new();
    let stats = BTreeMap::from([("tool:1".to_owned(), vec!["src/file-1.rs +3 -1".to_owned()])]);
    let original = TranscriptSnapshot::from_materialized(&session);
    let added =
        TranscriptSnapshot::from_materialized_reusing(&session, &stats, &original.entries, &empty);
    assert_eq!(added.entries[0].tool_diffstats, stats["tool:1"]);
    let removed =
        TranscriptSnapshot::from_materialized_reusing(&session, &empty, &added.entries, &stats);
    assert_eq!(
        removed.entries[0].tool_diffstats,
        original.entries[0].tool_diffstats
    );
}

fn completed_tool(seq: u64, title: &str) -> ChatEntry {
    let mut entry = ChatEntry::tool(seq, title, None, ToolStatus::Completed);
    entry.tool_summary = Some(title.to_owned());
    entry
}

fn completed_execute_tool(seq: u64, title: &str) -> ChatEntry {
    let call = ToolCall::new(format!("call-{seq}"), title)
        .kind(ToolKind::Execute)
        .status(ToolCallStatus::Completed);
    let presentation = tool_call_presentation(&call);
    let mut entry = ChatEntry::tool(seq, title, None, ToolStatus::Completed);
    entry.tool_summary = Some(presentation.summary.clone());
    entry.tool_presentation = Some(presentation);
    entry
}

fn session_restart(seq: u64) -> ChatEntry {
    ChatEntry::plain(
        seq,
        ChatRole::System,
        mj_core::transcript::SESSION_RESTART_TEXT,
    )
}

fn keypad_key(code: KeyCode) -> KeyEvent {
    KeyEvent::new_with_kind_and_state(
        code,
        KeyModifiers::NONE,
        KeyEventKind::Press,
        KeyEventState::KEYPAD,
    )
}

/// A wheel event clear of the conversations pane, which is the hitbox hover
/// routing checks before it hands the wheel to the transcript.
fn wheel(kind: MouseEventKind) -> MouseEvent {
    mouse_in(kind, Rect::new(0, 10, 40, 1))
}

#[test]
fn adjacent_session_restart_markers_keep_only_the_latest_presentation() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    let mut second_restart = session_restart(2);
    second_restart.recorded_at_ms = Some(2_000);
    chat.entries = vec![
        session_restart(1),
        second_restart,
        ChatEntry::plain(3, ChatRole::User, "continue"),
        session_restart(4),
        session_restart(5),
    ];
    chat.latest_seq = 5;

    let rich = transcript_text(&mut chat, 80);
    assert_eq!(
        rich.iter()
            .filter(|line| line.contains(mj_core::transcript::SESSION_RESTART_TEXT))
            .count(),
        2,
        "one marker survives each adjacent run: {rich:?}"
    );
    assert_eq!(
        chat.entries.len(),
        5,
        "presentation collapse does not rewrite the projected transcript"
    );
    chat.render_mode = TranscriptRenderMode::Raw;
    let raw = transcript_text(&mut chat, 80);
    assert_eq!(
        raw.iter()
            .filter(|line| line.contains(mj_core::transcript::SESSION_RESTART_TEXT))
            .count(),
        2,
        "raw mode coalesces the markers while keeping its visible source rows"
    );

    let mut raw_only = ChatEntry::plain(10, ChatRole::System, "captured terminal output");
    raw_only.raw_only = true;
    let mut raw_chat = ChatState::new(&snapshot(), &[]);
    raw_chat.entries = vec![session_restart(8), raw_only, session_restart(12)];
    raw_chat.render_mode = TranscriptRenderMode::Raw;
    let raw_with_visible_detail = transcript_text(&mut raw_chat, 80);
    assert_eq!(
        raw_with_visible_detail
            .iter()
            .filter(|line| line.contains(mj_core::transcript::SESSION_RESTART_TEXT))
            .count(),
        2,
        "a Raw-visible detail separates restart markers"
    );

    raw_chat.render_mode = TranscriptRenderMode::Rich;
    let rich_with_omitted_detail = transcript_text(&mut raw_chat, 80);
    assert_eq!(
        rich_with_omitted_detail
            .iter()
            .filter(|line| line.contains(mj_core::transcript::SESSION_RESTART_TEXT))
            .count(),
        1,
        "a Rich-omitted detail does not separate restart markers"
    );

    let browser = chat.transcript_snapshot().browser_transcript(None);
    assert_eq!(
        browser
            .entries
            .iter()
            .map(|entry| entry.id)
            .collect::<Vec<_>>(),
        [2, 3, 5],
        "the browser receives the newest marker from each run"
    );
    assert_eq!(browser.entries[0].recorded_at_ms, Some(2_000));
    assert_eq!(browser.latest_seq, 5);
    assert_eq!(browser.window_start_seq, 5);

    // A viewer that had the older marker must be told to replace its feed;
    // otherwise the append-only browser DOM would retain the hidden entry.
    let delta = chat.transcript_snapshot().browser_transcript(Some(1));
    assert!(delta.reset);
    assert_eq!(
        delta
            .entries
            .iter()
            .map(|entry| entry.id)
            .collect::<Vec<_>>(),
        [2, 3, 5]
    );
}

#[test]
fn browser_restart_collapse_resets_when_a_delivered_marker_is_replaced() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.entries = vec![
        ChatEntry::plain(1, ChatRole::User, "before restart"),
        session_restart(2),
    ];
    chat.latest_seq = 2;
    let opened = chat.transcript_snapshot().browser_transcript(None);
    assert_eq!(
        opened
            .entries
            .iter()
            .map(|entry| entry.id)
            .collect::<Vec<_>>(),
        [1, 2]
    );
    assert_eq!(opened.window_start_seq, 1);

    chat.entries.push(session_restart(3));
    chat.latest_seq = 3;
    let delta = chat.transcript_snapshot().browser_transcript(Some(2));
    assert!(delta.reset, "the old marker was already delivered");
    assert_eq!(delta.window_start_seq, 3);
    assert_eq!(
        delta
            .entries
            .iter()
            .map(|entry| entry.id)
            .collect::<Vec<_>>(),
        [1, 3]
    );

    let settled = chat.transcript_snapshot().browser_transcript(Some(3));
    assert!(!settled.reset);
    assert!(settled.entries.is_empty());
}

// Hard-won: 0333efb9: review-generated prompts rendered as if the user typed them.
#[test]
fn a_generated_review_prompt_renders_as_hels_own_line() {
    let item = std::sync::Arc::new(TranscriptItem {
        stable_id: "user:1".into(),
        position: 1,
        latest_content_event_ordinal: None,
        created_at_ms: 0,
        last_changed_at_ms: 0,
        body: TranscriptBody::User {
            content: vec![serde_json::json!({
                "type": "text",
                "text": mj_core::second_opinion::PRIMARY_CONTEXT_REQUEST,
            })],
        },
    });
    let typed = std::sync::Arc::new(TranscriptItem {
        stable_id: "user:2".into(),
        position: 2,
        latest_content_event_ordinal: None,
        created_at_ms: 0,
        last_changed_at_ms: 0,
        body: TranscriptBody::User {
            content: vec![serde_json::json!({"type": "text", "text": "fix the parser"})],
        },
    });
    let session = MaterializedSession {
        transcript: vec![item, typed],
        applied_event_ordinal: 2,
        ..MaterializedSession::empty("session-1")
    };

    let entries = materialized_chat_entries_reusing(&session, 0, Vec::new());
    assert_eq!(entries[0].role, ChatRole::System);
    assert_eq!(entries[1].role, ChatRole::User);

    // Rebuilding reuses the entries rather than flipping their roles.
    let again = materialized_chat_entries_reusing(&session, 0, entries.clone());
    assert_eq!(again[0].role, ChatRole::System);
    assert_eq!(again[1].role, ChatRole::User);
    assert_eq!(again[0].text, entries[0].text);
}

#[test]
fn an_empty_conversation_identifies_initial_relay_loading() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.set_transcript_loading(true);

    assert_eq!(transcript_text(&mut chat, 80), ["Loading…"]);

    chat.set_transcript_loading(false);
    assert_eq!(
        transcript_text(&mut chat, 80),
        ["No messages yet — send a prompt to begin."]
    );
}

#[test]
fn conversation_title_includes_the_session_name_after_the_dashboard_summary() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.set_header_summary("precision-3260/bifrost-fuzz", "kimi", "Fix the build");
    chat.turn_started_at_epoch_seconds = Some(7_847);
    chat.set_current_step_start(Some(20_000_000));

    assert_eq!(
        transcript_title(&chat, 20_000).to_string(),
        " precision-3260/bifrost-fuzz  Working 3h22m  kimi  Fix the build "
    );
    chat.set_detailed_activity_clocks(true);
    assert_eq!(
        transcript_title(&chat, 20_000).to_string(),
        " precision-3260/bifrost-fuzz  T 3h22m S 0s  kimi  Fix the build "
    );
    chat.set_detailed_activity_clocks(false);

    chat.render_mode = TranscriptRenderMode::Raw;
    assert_eq!(
        transcript_title(&chat, 20_000).to_string(),
        " precision-3260/bifrost-fuzz  Working 3h22m  kimi  Fix the build · raw source "
    );

    // An idle session that left a command running names it in the same place
    // the turn clock goes.
    chat.render_mode = TranscriptRenderMode::Rich;
    chat.turn_started_at_epoch_seconds = None;
    chat.set_session_activity(mj_client::usage_format::SessionActivity {
        pursuing_goal: Default::default(),
        checking_response: false,
        quota_recovery: None,
        capacity_retry: None,
        activity_turn_started_at_ms: None,
        prompt_in_flight: false,
        idle_since_ms: None,
        execution: None,
        harness_turn_started_at_ms: None,
        state: None,
        foreground_tool_started_at_ms: None,
        background_commands: vec![mj_core::relay::BackgroundCommand {
            id: "test-background".into(),
            started_at_ms: 17_384_000,
            command: "cargo test".into(),
            can_stop: false,
        }],
        active_user_shells: Vec::new(),
    });
    assert_eq!(
        transcript_title(&chat, 20_000).to_string(),
        " precision-3260/bifrost-fuzz  1 task 43m36s  kimi  Fix the build "
    );
    chat.set_detailed_activity_clocks(true);
    assert_eq!(
        transcript_title(&chat, 20_000).to_string(),
        " precision-3260/bifrost-fuzz  BG 43m36s  kimi  Fix the build "
    );
    chat.set_detailed_activity_clocks(false);

    let previous_activity = chat.session_activity().clone();
    chat.set_session_activity(mj_client::usage_format::SessionActivity {
        pursuing_goal: Default::default(),
        checking_response: false,
        foreground_tool_started_at_ms: Some(19_988_000),
        background_commands: Vec::new(),
        ..previous_activity
    });
    assert_eq!(
        transcript_title(&chat, 20_000).to_string(),
        " precision-3260/bifrost-fuzz  Working 12s  kimi  Fix the build "
    );
}

/// A host that draws its own chips at the right of the title row tells the
/// chat how many columns to stay clear of, and the title stops short of them.
#[test]
fn a_long_title_stops_short_of_the_columns_the_host_reserved() {
    use ratatui::{Terminal, backend::TestBackend};

    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.set_header_summary("podman", "profile", "t".repeat(200));
    let reserve = 4;
    let mut terminal = Terminal::new(TestBackend::new(60, 10)).expect("terminal");
    terminal
        .draw(|frame| {
            render_transcript(frame, frame.area(), &mut chat, false, reserve, 0, false);
        })
        .expect("render conversation");
    let buffer = terminal.backend().buffer();
    let header = (0..60).map(|x| buffer[(x, 0)].symbol()).collect::<String>();
    let title_end = header
        .char_indices()
        .rfind(|(_, glyph)| *glyph == 't')
        .map(|(index, _)| header[..index].chars().count() as u16 + 1)
        .expect("the title is drawn");
    assert!(
        title_end <= 60 - 1 - reserve,
        "the title has to stop short of the reserved chip columns: {header:?}"
    );
}

#[test]
fn long_conversation_titles_use_the_header_width_while_working() {
    use ratatui::{Terminal, backend::TestBackend};

    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.set_header_summary("podman", "profile", "界".repeat(80));
    chat.mark_prompt_submitted("continue");
    for width in [32, 48, 80] {
        let mut terminal = Terminal::new(TestBackend::new(width, 10)).expect("terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_transcript(frame, area, &mut chat, false, 0, 0, false);
            })
            .expect("render conversation");
        let buffer = terminal.backend().buffer();
        assert!((width - 3..width - 1).any(|x| buffer[(x, 0)].symbol() == "…"));
    }
}

#[test]
fn real_tool_claim_suppresses_only_the_matching_terminal_incarnation() {
    let claimed_at_ms = mj_core::clock::epoch_millis();
    let mut session = MaterializedSession::empty("session-terminal-claim");
    session.applied_event_ordinal = 1;
    session.applied_event_digest = "a".repeat(64);
    session.transcript = vec![Arc::new(TranscriptItem {
        stable_id: "tool:shell".into(),
        position: 1,
        latest_content_event_ordinal: None,
        created_at_ms: claimed_at_ms,
        last_changed_at_ms: claimed_at_ms,
        body: TranscriptBody::Tool {
            call: serde_json::json!({
                "toolCallId": "shell",
                "title": "Shell",
                "status": "in_progress",
                "content": [{"type": "terminal", "terminalId": "term-1"}]
            }),
            terminal_outputs: Vec::new(),
            terminal_refs: vec!["term-1".into()],
            presentation: None,
        },
    })];
    let mut chat = ChatState::from_materialized(&session, &[], &[]);
    chat.set_active_agent_terminals(
        &[mj_core::relay::ActiveAgentTerminal {
            terminal_id: "term-1".into(),
            command: "hidden fallback command".into(),
            started_at_ms: claimed_at_ms,
        }],
        &session,
    );

    let claimed = transcript_text(&mut chat, 80);
    assert!(
        !claimed
            .iter()
            .any(|line| line.contains("hidden fallback command")),
        "the ACP tool card owns its live terminal: {claimed:?}"
    );

    chat.set_active_agent_terminals(
        &[mj_core::relay::ActiveAgentTerminal {
            terminal_id: "term-1".into(),
            command: "new bridge command".into(),
            started_at_ms: claimed_at_ms + 1,
        }],
        &session,
    );
    let reused = transcript_text(&mut chat, 80);
    assert!(
        reused
            .iter()
            .any(|line| line.contains("new bridge command")),
        "an old claim cannot hide a reused id after restart: {reused:?}"
    );
}

fn thought(seq: u64, text: &str) -> ChatEntry {
    ChatEntry::plain(seq, ChatRole::Thought, text)
}

/// A chat with `count` single-line user messages, each naming its index so
/// scroll assertions can name the row they expect to see.
fn numbered_chat(count: usize) -> ChatState {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.entries = (0..count)
        .map(|index| ChatEntry::plain(index as u64, ChatRole::User, format!("message {index}")))
        .collect();
    chat
}

fn user_transcript_item(position: u64, text: &str) -> Arc<TranscriptItem> {
    Arc::new(TranscriptItem {
        stable_id: format!("user:{position}"),
        position,
        latest_content_event_ordinal: None,
        created_at_ms: position as i64 * 10,
        last_changed_at_ms: position as i64 * 10,
        body: TranscriptBody::User {
            content: vec![serde_json::json!(text)],
        },
    })
}

/// Transcript items for the tail-first tests. Every item carries the same
/// timestamps, so entries share a revision and a row cached at one position
/// would be served at any other position the cache still believes in.
const FIXTURE_MS: i64 = 7;

fn fixture_item(position: u64, stable_id: String, body: TranscriptBody) -> Arc<TranscriptItem> {
    Arc::new(TranscriptItem {
        stable_id,
        position,
        // The projection requires an agent message to carry the ordinal of
        // its latest content chunk, and carries none for anything else.
        latest_content_event_ordinal: matches!(body, TranscriptBody::Agent { .. })
            .then_some(position),
        created_at_ms: FIXTURE_MS,
        last_changed_at_ms: FIXTURE_MS,
        body,
    })
}

fn fixture_user_item(position: u64) -> Arc<TranscriptItem> {
    fixture_item(
        position,
        format!("user:{position}"),
        TranscriptBody::User {
            content: vec![serde_json::json!(format!("question {position}"))],
        },
    )
}

fn fixture_agent_item(position: u64) -> Arc<TranscriptItem> {
    fixture_item(
        position,
        format!("agent:{position}"),
        TranscriptBody::Agent {
            // Multi-kilobyte, so the conversion cost is realistic.
            chunks: (0..8)
                .map(|chunk| {
                    serde_json::json!({
                        "content": {
                            "type": "text",
                            "text": format!("answer {position}.{chunk} ").repeat(40)
                        }
                    })
                })
                .collect(),
            streaming: false,
        },
    )
}

fn fixture_thought_item(position: u64) -> Arc<TranscriptItem> {
    fixture_item(
        position,
        format!("thought:{position}"),
        TranscriptBody::Thought {
            chunks: vec![serde_json::json!({
                "content": {"type": "text", "text": format!("thinking about {position}")}
            })],
            streaming: false,
        },
    )
}

fn fixture_tool_item(position: u64) -> Arc<TranscriptItem> {
    fixture_item(
        position,
        format!("tool:{position}"),
        TranscriptBody::Tool {
            call: serde_json::json!({
                "toolCallId": format!("call-{position}"),
                "title": format!("read file-{position}"),
                "status": "completed",
                "content": [{
                    "type": "content",
                    "content": {"type": "text", "text": "output ".repeat(600)}
                }],
                "locations": [{"path": format!("src/file-{position}.rs"), "line": 3}]
            }),
            terminal_outputs: Vec::new(),
            terminal_refs: Vec::new(),
            presentation: None,
        },
    )
}

fn fixture_plan_item(position: u64) -> Arc<TranscriptItem> {
    fixture_item(
        position,
        format!("plan:{position}"),
        TranscriptBody::Plan {
            plan: serde_json::json!({
                "entries": [{
                    "content": format!("step {position}"),
                    "priority": "medium",
                    "status": "in_progress"
                }]
            }),
        },
    )
}

fn fixture_system_item(position: u64) -> Arc<TranscriptItem> {
    fixture_item(
        position,
        format!("system:{position}"),
        TranscriptBody::System {
            text: format!("notice {position}"),
        },
    )
}

/// A conversation with the mix of bodies a real session carries, its first
/// item at `first_position`. A compaction rewrite replaces the history in
/// place, so it produces the same shape of transcript at fresh ordinals.
fn materialized_session_from(first_position: u64, items: u64) -> MaterializedSession {
    let mut session = MaterializedSession::empty("session-long");
    session.transcript = (first_position..first_position + items)
        .map(|position| match position % 6 {
            0 => fixture_tool_item(position),
            1 => fixture_user_item(position),
            2 => fixture_agent_item(position),
            3 => fixture_thought_item(position),
            4 => fixture_plan_item(position),
            _ => fixture_system_item(position),
        })
        .collect();
    session.applied_event_ordinal = first_position + items;
    session
}

/// A conversation with the mix of bodies a real session carries.
fn long_materialized_session(items: u64) -> MaterializedSession {
    materialized_session_from(1, items)
}

fn entry_texts(entries: &[ChatEntry]) -> Vec<&str> {
    entries.iter().map(|entry| entry.text.as_str()).collect()
}

fn converted_prefix(session: &MaterializedSession, chat: &ChatState) -> Vec<ChatEntry> {
    materialized_prefix_entries(
        &session.transcript[..chat.unconverted_prefix()],
        session.applied_event_ordinal,
    )
}

// Hard-won: 2b229f70: opening a large session blocked the dashboard for seconds.
#[test]
fn opening_a_long_session_converts_only_the_tail() {
    let items = TAIL_SEED_ITEMS as u64 + 400;
    let session = long_materialized_session(items);

    let chat = ChatState::from_materialized_tail(&session, &[], &[]);

    assert_eq!(chat.entries.len(), TAIL_SEED_ITEMS);
    assert_eq!(chat.unconverted_prefix(), 400);
    let eager = materialized_chat_entries(&session);
    assert_eq!(chat.entries, eager[400..]);
    let short_session = long_materialized_session(TAIL_SEED_ITEMS as u64);
    let short_chat = ChatState::from_materialized_tail(&short_session, &[], &[]);
    assert_eq!(short_chat.unconverted_prefix(), 0);
    assert_eq!(
        short_chat.entries,
        materialized_chat_entries(&short_session),
        "the threshold keeps shorter conversations fully available"
    );
}

#[test]
fn an_update_while_the_prefix_is_pending_keeps_the_tail_and_still_splices() {
    let mut session = long_materialized_session(TAIL_SEED_ITEMS as u64 + 300);
    let mut chat = ChatState::from_materialized_tail(&session, &[], &[]);
    let prefix = converted_prefix(&session, &chat);
    let pending = chat.unconverted_prefix();

    let appended = session.transcript.len() as u64 + 1;
    session.transcript.push(fixture_user_item(appended));
    session.transcript.push(fixture_agent_item(appended + 1));
    session.applied_event_ordinal = appended + 2;
    chat.apply_materialized(&session, &[], &[]);

    assert_eq!(chat.unconverted_prefix(), pending);
    assert_eq!(chat.entries.len(), session.transcript.len() - pending);
    assert_eq!(
        entry_texts(&chat.entries),
        entry_texts(&materialized_chat_entries(&session)[pending..])
    );

    assert!(chat.splice_transcript_prefix(prefix));
    assert_eq!(
        entry_texts(&chat.entries),
        entry_texts(&materialized_chat_entries(&session))
    );
    assert!(
        chat.entries
            .windows(2)
            .all(|pair| pair[0].start_seq < pair[1].start_seq)
    );
}

#[test]
fn splicing_the_prefix_drops_render_rows_cached_at_the_old_positions() {
    let session = long_materialized_session(TAIL_SEED_ITEMS as u64 + 120);
    let mut chat = ChatState::from_materialized_tail(&session, &[], &[]);
    let prefix = converted_prefix(&session, &chat);
    // Fill the cache while the entries still stand for the tail only.
    chat.anchor = TranscriptAnchor::Row { entry: 0, row: 0 };
    let tail_top = drawn_transcript(&mut chat, 60, 24);
    assert!(shows(&tail_top, "question 121"));

    assert!(chat.splice_transcript_prefix(prefix));
    chat.anchor = TranscriptAnchor::Row { entry: 0, row: 0 };
    let spliced_top = drawn_transcript(&mut chat, 60, 24);

    let mut eager = ChatState::from_materialized(&session, &[], &[]);
    eager.anchor = TranscriptAnchor::Row { entry: 0, row: 0 };
    assert_eq!(spliced_top, drawn_transcript(&mut eager, 60, 24));
    assert!(shows(&spliced_top, "question 1"));
    assert!(!shows(&spliced_top, "question 121"));
}

#[test]
fn a_prefix_that_no_longer_meets_the_tail_is_refused() {
    let session = long_materialized_session(TAIL_SEED_ITEMS as u64 + 60);
    let mut chat = ChatState::from_materialized_tail(&session, &[], &[]);
    let pending = chat.unconverted_prefix();
    // History from a compacted transcript: the right length, but it runs
    // past the first entry the tail holds.
    let stale = materialized_prefix_entries(
        &session.transcript[session.transcript.len() - pending..],
        session.applied_event_ordinal,
    );

    assert!(!chat.splice_transcript_prefix(stale));

    assert_eq!(chat.unconverted_prefix(), pending);
    assert_eq!(chat.entries.len(), TAIL_SEED_ITEMS);
}

// Hard-won: 93fa5d9a: a same-size compaction rewrite let a stale prefix match current history.
#[test]
fn a_prefix_from_replaced_history_is_refused_when_the_rewrite_keeps_the_length() {
    let session = long_materialized_session(TAIL_SEED_ITEMS as u64 + 60);
    let mut chat = ChatState::from_materialized_tail(&session, &[], &[]);
    let stale = converted_prefix(&session, &chat);
    let pending = chat.unconverted_prefix();
    assert_eq!(stale.len(), pending);

    // Compaction rewrites the whole conversation at fresh ordinals and
    // leaves it exactly as long, so counting alone still lines up.
    let rewritten = materialized_session_from(1_000, session.transcript.len() as u64);
    chat.apply_materialized(&rewritten, &[], &[]);
    assert_eq!(chat.unconverted_prefix(), pending);
    assert!(
        stale.last().unwrap().start_seq < chat.entries[0].start_seq,
        "the replaced history still sorts in front of the rewritten tail"
    );

    assert!(!chat.splice_transcript_prefix(stale));

    assert_eq!(chat.unconverted_prefix(), pending);
    assert_eq!(
        entry_texts(&chat.entries),
        entry_texts(&materialized_chat_entries(&rewritten)[pending..])
    );
}

#[test]
fn compaction_below_the_pending_prefix_reseats_the_tail() {
    let session = long_materialized_session(TAIL_SEED_ITEMS as u64 + 500);
    let mut chat = ChatState::from_materialized_tail(&session, &[], &[]);
    let prefix = converted_prefix(&session, &chat);

    // Compaction leaves a transcript shorter than the pending prefix.
    let mut compacted = long_materialized_session(TAIL_SEED_ITEMS as u64 + 100);
    compacted.applied_event_ordinal = session.applied_event_ordinal + 1;
    chat.apply_materialized(&compacted, &[], &[]);

    assert_eq!(chat.unconverted_prefix(), 100);
    assert_eq!(chat.entries.len(), TAIL_SEED_ITEMS);
    assert_eq!(
        entry_texts(&chat.entries),
        entry_texts(&materialized_chat_entries(&compacted)[100..])
    );
    // The history built against the old transcript no longer fits.
    assert!(!chat.splice_transcript_prefix(prefix));
}

fn shows(rows: &[String], needle: &str) -> bool {
    rows.iter().any(|row| row.contains(needle))
}

/// The message bodies on screen, ignoring the title and composer chrome.
fn visible_messages(rows: &[String]) -> Vec<String> {
    rows.iter()
        .filter(|row| row.contains("│ message "))
        .cloned()
        .collect()
}

fn browser_tail_label(entry: &BrowserTranscriptEntry) -> String {
    format!("{}: {}", entry.label, entry.lines[0])
}

#[test]
fn reset_interaction_preserves_projected_transcript_and_render_cache() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.entries
        .push(ChatEntry::plain(1, ChatRole::Agent, "cached response"));
    let _ = transcript_text(&mut chat, 80);
    chat.set_input("draft".into());
    chat.prompt_history.push("previous".into());
    chat.queued_prompts.push_back(queued("queued-1", "queued"));
    chat.anchor = TranscriptAnchor::Row { entry: 0, row: 4 };
    chat.set_notice("temporary");
    chat.voice_active = true;

    chat.reset_interaction();

    assert_eq!(chat.entries.len(), 1);
    assert!(chat.render_cache.entries[0].is_some());
    assert_eq!(chat.input, "draft");
    assert_eq!(chat.input_cursor, "draft".len());
    assert!(chat.prompt_history.is_empty());
    assert!(chat.queued_prompts.is_empty());
    assert_eq!(chat.anchor, TranscriptAnchor::Bottom);
    assert!(chat.notice().is_none());
    assert!(!chat.voice_active);
}

#[test]
fn user_and_agent_headers_show_first_event_time_as_local_hours_and_minutes() {
    let expected = format_event_time(Some(0)).unwrap();
    let runtime = |text| RuntimeEvent::SessionUpdate {
        update: serde_json::json!({
            "sessionUpdate": "agent_message_chunk",
            "messageId": "message-1",
            "content": {"type": "text", "text": text}
        }),
    };
    let events = vec![
        SequencedEvent {
            seq: 1,
            recorded_at_ms: Some(0),
            request_id: Some("p".into()),
            event: WorkerEvent::PromptAccepted {
                request_id: "p".into(),
                text: "work".into(),
                attachments: vec![],
            },
        },
        SequencedEvent {
            seq: 2,
            recorded_at_ms: Some(0),
            request_id: None,
            event: WorkerEvent::Adapter {
                kind: "session_update".into(),
                payload: serde_json::to_value(runtime("do")).unwrap(),
            },
        },
        SequencedEvent {
            seq: 3,
            recorded_at_ms: Some(60_000),
            request_id: None,
            event: WorkerEvent::Adapter {
                kind: "session_update".into(),
                payload: serde_json::to_value(runtime("ne")).unwrap(),
            },
        },
    ];
    let mut initial = snapshot();
    initial.latest_seq = 3;
    let mut chat = ChatState::new(&initial, &events);
    let lines = transcript_text(&mut chat, 80);

    assert!(lines.contains(&format!("❯ You · {expected}")));
    assert!(lines.contains(&format!("● Agent · {expected}")));
    assert_eq!(chat.entries[1].text, "done");
    assert_eq!(chat.entries[1].recorded_at_ms, Some(0));
}

#[test]
fn live_acp_diffs_render_paths_without_counting_lines_on_the_event_loop() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.apply_session_update(
        1,
        &serde_json::json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "edit-lib",
            "title": "Edit src/lib.rs",
            "status": "in_progress",
            "content": [{
                "type": "diff",
                "path": "/workspace/src/lib.rs",
                "oldText": "alpha\n",
                "newText": "alpha\nbeta\n"
            }]
        }),
    );

    assert_eq!(
        transcript_text(&mut chat, 80),
        ["● Tool · running", "│ Edit", "│ /workspace/src/lib.rs", ""]
    );

    chat.apply_session_update(
        2,
        &serde_json::json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "edit-lib",
            "status": "completed",
            "content": [{
                "type": "diff",
                "path": "/workspace/src/lib.rs",
                "oldText": "alpha\n",
                "newText": "gamma\n"
            }]
        }),
    );

    assert_eq!(chat.entries[0].tool_diffstats, ["/workspace/src/lib.rs"]);
    assert_eq!(
        transcript_text(&mut chat, 80),
        ["✓ Tool · done", "│ Edit", "│ /workspace/src/lib.rs", ""]
    );
}

#[test]
fn completed_tool_run_collapses_to_single_summary_cell() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.entries.push(completed_tool(1, "grep -rn alpha src"));
    chat.entries.push(completed_tool(2, "grep -rn beta src"));
    chat.entries.push(completed_tool(3, "cat notes.md"));

    let text = transcript_text(&mut chat, 80);

    assert_eq!(
        text,
        [
            "✓ Tool · done",
            "│ grep -rn alpha src, grep -rn beta src, cat notes.md",
            "",
        ]
    );
}

#[test]
fn clicking_a_collapsed_member_expands_only_that_call_and_splits_the_run() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.entries.extend([
        completed_tool(1, "first command"),
        completed_tool(2, "second command"),
        completed_tool(3, "third command"),
        completed_tool(4, "fourth command"),
    ]);

    drawn_transcript(&mut chat, 80, 24);
    assert!(
        chat.transcript_tool_click_targets
            .iter()
            .any(|target| target.start_seq == 2)
    );
    let target = chat
        .transcript_tool_click_targets
        .iter()
        .find(|target| target.start_seq == 2)
        .copied()
        .expect("the second summary segment is clickable");
    let transcript_inner = Rect::new(1, 1, 78, 22);
    assert!(chat.transcript_tool_click_targets.iter().all(|target| {
        transcript_inner.contains(Position::new(target.rect.x, target.rect.y))
            && transcript_inner.contains(Position::new(
                target.rect.right().saturating_sub(1),
                target.rect.bottom().saturating_sub(1),
            ))
    }));

    chat.handle_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: target.rect.x,
        row: target.rect.y,
        modifiers: KeyModifiers::NONE,
    });
    drawn_transcript(&mut chat, 80, 24);

    assert!(chat.expanded_tool_calls.contains(&2));
    assert!(matches!(
        chat.render_cache.collapse[1],
        EntryCollapse::Expanded
    ));
    assert!(matches!(
        chat.render_cache.collapse[2],
        EntryCollapse::Summary { end: 4, .. }
    ));

    let expanded = chat
        .transcript_tool_click_targets
        .iter()
        .find(|target| target.start_seq == 2)
        .copied()
        .expect("the expanded call remains clickable");
    chat.handle_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: expanded.rect.x,
        row: expanded.rect.y,
        modifiers: KeyModifiers::NONE,
    });
    drawn_transcript(&mut chat, 80, 24);

    assert!(!chat.expanded_tool_calls.contains(&2));
    assert!(matches!(
        chat.render_cache.collapse[0],
        EntryCollapse::Summary { end: 4, .. }
    ));
}

fn click_rendered_text(chat: &mut ChatState, rows: &[String], text: &str) {
    let (row, line, offset) = rows
        .iter()
        .enumerate()
        .find_map(|(row, line)| line.find(text).map(|offset| (row, line, offset)))
        .unwrap_or_else(|| panic!("missing {text:?} in {rows:#?}"));
    chat.handle_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: u16::try_from(display_width(&line[..offset])).unwrap(),
        row: u16::try_from(row).unwrap(),
        modifiers: KeyModifiers::NONE,
    });
}

#[test]
fn partially_scrolled_repeated_tool_names_expand_the_visible_call() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    for seq in 1..=4 {
        let mut tool = completed_tool(seq, &format!("provider title {seq}"));
        tool.tool_summary = Some("repeat-command".to_owned());
        tool.tool_content = vec![format!("details for call {seq}")];
        chat.entries.push(tool);
    }
    chat.entries.push(ChatEntry::plain(
        5,
        ChatRole::Agent,
        "later response\n".repeat(30),
    ));
    drawn_transcript(&mut chat, 24, 16);
    // The first tool's summary row is above the viewport; the second and
    // third rows have identical text, including their trailing comma.
    chat.anchor = TranscriptAnchor::Row { entry: 0, row: 2 };
    let rows = drawn_transcript(&mut chat, 24, 16);
    assert!(rows[1].contains("repeat-command,"), "{rows:#?}");
    click_rendered_text(&mut chat, &rows, "repeat-command");
    assert_eq!(chat.expanded_tool_calls, BTreeSet::from([2]));
    let rendered = transcript_text(&mut chat, 24);
    assert!(shows(&rendered, "provider title 2"));
    assert!(shows(&rendered, "details for call 2"));
    assert!(!shows(&rendered, "details for call 1"));
}

#[test]
fn kimi_shell_tool_run_collapses_to_command_names() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.entries.push(completed_execute_tool(
        1,
        "Running: rg -n project_memory src",
    ));
    chat.entries
        .push(completed_execute_tool(2, "Running: cargo test --lib"));
    chat.entries.push(completed_execute_tool(
        3,
        "Starting background: npm run preview",
    ));
    chat.entries
        .push(ChatEntry::plain(4, ChatRole::User, "continue"));

    assert_eq!(
        transcript_text(&mut chat, 80),
        [
            "✓ Tool · done",
            "│ rg, cargo test, npm run",
            "",
            "❯ You",
            "│ continue",
            "",
        ]
    );
}

/// A parser-derived summary starts at the executable even when a harness
/// decorates the command title.
// Hard-won: 585c3402: leading command punctuation made a collapsed command title render incorrectly.
#[test]
fn a_collapsed_tool_label_starts_at_the_command_name() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.entries
        .push(completed_execute_tool(1, "Running: sed -n 1,10p notes.md"));
    chat.entries
        .push(completed_execute_tool(2, "./build.sh --release"));
    chat.entries
        .push(completed_execute_tool(3, "Running: ls -la"));
    chat.entries
        .push(ChatEntry::plain(4, ChatRole::User, "continue"));

    assert_eq!(
        transcript_text(&mut chat, 80),
        [
            "✓ Tool · done",
            "│ sed, ./build.sh, ls",
            "",
            "❯ You",
            "│ continue",
            "",
        ]
    );
}

#[test]
fn interleaved_tools_and_thoughts_render_latest_thinking_then_tool_cdl() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.entries.extend([
        completed_tool(1, "sed -n 1,260p .agents/PLANS.md"),
        thought(2, "Planning coverage analysis with cargo llvm-cov"),
        completed_tool(3, "cargo llvm-cov nextest --help"),
        thought(4, "Requesting full main help information"),
        completed_tool(5, "cargo llvm-cov --help"),
        thought(6, "Planning durable coverage storage"),
        completed_tool(7, "cargo llvm-cov report --help"),
        thought(8, "Planning optimized coverage reporting"),
        completed_tool(9, "Editing files"),
        thought(10, "Preparing coverage environment cleanup"),
    ]);

    assert_eq!(
        transcript_text(&mut chat, 80),
        [
            "○ Thinking",
            "│ Preparing coverage environment cleanup",
            "",
            "✓ Tool · done",
            "│ sed -n 1,260p .agents/PLANS.md, cargo llvm-cov nextest --help, cargo llvm-cov",
            "│ --help, cargo llvm-cov report --help, Editing files",
            "",
        ]
    );
}

#[test]
fn thought_only_streak_keeps_only_the_most_recent_block() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.entries.extend([
        thought(1, "first approach"),
        thought(2, "second approach"),
        thought(3, "final approach"),
    ]);

    assert_eq!(
        transcript_text(&mut chat, 80),
        ["○ Thinking", "│ final approach", ""]
    );
}

#[test]
fn visible_nonmembers_break_tool_thought_streaks() {
    let separators = [
        ChatEntry::plain(3, ChatRole::User, "user boundary"),
        ChatEntry::plain(3, ChatRole::Agent, "agent boundary"),
        ChatEntry::plan(3, Vec::new()),
        ChatEntry::plain(3, ChatRole::System, "system boundary"),
        ChatEntry::tool(3, "waiting tool", None, ToolStatus::Pending),
        ChatEntry::tool(3, "running tool", None, ToolStatus::Running),
        ChatEntry::tool(3, "failed tool", None, ToolStatus::Failed),
    ];

    for separator in separators {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.entries.extend([
            thought(1, "thought before boundary"),
            completed_tool(2, "grep -rn alpha src"),
            separator,
            thought(4, "thought after boundary"),
            completed_tool(5, "cat notes.md"),
            ChatEntry::plain(6, ChatRole::User, "release trailing tool"),
        ]);

        let rendered = transcript_text(&mut chat, 80);
        assert!(rendered.contains(&"│ thought before boundary".to_owned()));
        assert!(rendered.contains(&"│ thought after boundary".to_owned()));
        assert!(rendered.contains(&"│ grep -rn alpha src".to_owned()));
        assert!(rendered.contains(&"│ cat notes.md".to_owned()));
        assert!(!rendered.contains(&"│ grep, cat".to_owned()));
    }
}

#[test]
fn trailing_tool_summary_does_not_change_when_a_later_thought_appears() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.entries.extend([
        completed_tool(1, "grep -rn alpha src"),
        thought(2, "checking the first result"),
        completed_tool(3, "cat notes.md"),
    ]);

    assert_eq!(
        transcript_text(&mut chat, 80),
        [
            "○ Thinking",
            "│ checking the first result",
            "",
            "✓ Tool · done",
            "│ grep -rn alpha src, cat notes.md",
            "",
        ]
    );

    chat.entries
        .push(thought(4, "checking the combined result"));

    assert_eq!(
        transcript_text(&mut chat, 80),
        [
            "○ Thinking",
            "│ checking the combined result",
            "",
            "✓ Tool · done",
            "│ grep -rn alpha src, cat notes.md",
            "",
        ]
    );
}

#[test]
fn updating_the_latest_collapsed_thought_invalidates_the_summary_cache() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.entries.extend([
        completed_tool(1, "grep -rn alpha src"),
        thought(2, "old thought"),
        completed_tool(3, "cat notes.md"),
        thought(4, "latest thought"),
    ]);
    assert!(transcript_text(&mut chat, 80).contains(&"│ latest thought".to_owned()));

    chat.entries[3].text = "revised latest thought".into();
    chat.entries[3].touch(5);

    let rendered = transcript_text(&mut chat, 80);
    assert!(rendered.contains(&"│ revised latest thought".to_owned()));
    assert!(!rendered.contains(&"│ latest thought".to_owned()));
}

#[test]
fn completed_tool_run_collapses_fully_once_a_new_request_starts() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.entries.push(completed_tool(1, "grep -rn alpha src"));
    chat.entries.push(completed_tool(2, "grep -rn beta src"));
    chat.entries.push(completed_tool(3, "cat notes.md"));
    chat.entries
        .push(ChatEntry::plain(4, ChatRole::User, "now ship it"));

    let text = transcript_text(&mut chat, 80);

    assert_eq!(
        text,
        [
            "✓ Tool · done",
            "│ grep -rn alpha src, grep -rn beta src, cat notes.md",
            "",
            "❯ You",
            "│ now ship it",
            "",
        ]
    );
}

#[test]
fn newest_completed_tool_joins_its_predecessor_immediately() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.entries.push(completed_tool(1, "grep -rn alpha src"));
    chat.entries.push(completed_tool(2, "cat notes.md"));

    let text = transcript_text(&mut chat, 80);

    assert_eq!(
        text,
        ["✓ Tool · done", "│ grep -rn alpha src, cat notes.md", "",]
    );
}

#[test]
fn a_later_completed_tool_collapses_the_earlier_run_entirely() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.entries.push(completed_tool(1, "grep -rn alpha src"));
    chat.entries.push(completed_tool(2, "grep -rn beta src"));
    chat.entries.push(completed_tool(3, "cat notes.md"));
    chat.entries
        .push(ChatEntry::plain(4, ChatRole::Agent, "found it"));
    chat.entries.push(completed_tool(5, "rg gamma src"));

    let text = transcript_text(&mut chat, 80);

    assert_eq!(
        text,
        [
            "✓ Tool · done",
            "│ grep -rn alpha src, grep -rn beta src, cat notes.md",
            "",
            "● Agent",
            "│ found it",
            "",
            "✓ Tool · done",
            "│ rg gamma src",
            "",
        ]
    );
}

#[test]
fn agent_message_between_completed_tools_prevents_collapsing() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.entries.push(completed_tool(1, "grep -rn alpha src"));
    chat.entries
        .push(ChatEntry::plain(2, ChatRole::Agent, "found it"));
    chat.entries.push(completed_tool(3, "cat notes.md"));

    let text = transcript_text(&mut chat, 80);

    assert_eq!(
        text,
        [
            "✓ Tool · done",
            "│ grep -rn alpha src",
            "",
            "● Agent",
            "│ found it",
            "",
            "✓ Tool · done",
            "│ cat notes.md",
            "",
        ]
    );
}

#[test]
fn failed_tool_renders_alone_and_breaks_the_collapsed_run() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.entries.push(completed_tool(1, "grep -rn alpha src"));
    chat.entries.push(completed_tool(2, "grep -rn beta src"));
    chat.entries.push(ChatEntry::tool(
        3,
        "cat missing.md",
        None,
        ToolStatus::Failed,
    ));
    chat.entries.push(completed_tool(4, "rg gamma src"));
    chat.entries.push(completed_tool(5, "rg delta src"));

    let text = transcript_text(&mut chat, 80);

    assert_eq!(
        text,
        [
            "✓ Tool · done",
            "│ grep -rn alpha src, grep -rn beta src",
            "",
            "× Tool · failed",
            "│ cat missing.md",
            "",
            "✓ Tool · done",
            "│ rg gamma src, rg delta src",
            "",
        ]
    );

    chat.entries
        .push(ChatEntry::plain(6, ChatRole::User, "now ship it"));

    assert_eq!(
        transcript_text(&mut chat, 80),
        [
            "✓ Tool · done",
            "│ grep -rn alpha src, grep -rn beta src",
            "",
            "× Tool · failed",
            "│ cat missing.md",
            "",
            "✓ Tool · done",
            "│ rg gamma src, rg delta src",
            "",
            "❯ You",
            "│ now ship it",
            "",
        ]
    );
}

#[test]
fn raw_mode_renders_every_completed_tool_in_full() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.render_mode = TranscriptRenderMode::Raw;
    chat.entries.push(completed_tool(1, "grep -rn alpha src"));
    chat.entries.push(completed_tool(2, "grep -rn beta src"));
    chat.entries.push(completed_tool(3, "cat notes.md"));

    let text = transcript_text(&mut chat, 80);

    assert_eq!(
        text,
        [
            "✓ Tool · done",
            "│ grep -rn alpha src",
            "",
            "✓ Tool · done",
            "│ grep -rn beta src",
            "",
            "✓ Tool · done",
            "│ cat notes.md",
            "",
        ]
    );
}

#[test]
fn raw_mode_preserves_interleaved_tools_and_thoughts_in_source_order() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.render_mode = TranscriptRenderMode::Raw;
    chat.entries.extend([
        completed_tool(1, "grep -rn alpha src"),
        thought(2, "first thought"),
        completed_tool(3, "cat notes.md"),
        thought(4, "latest thought"),
    ]);

    assert_eq!(
        transcript_text(&mut chat, 80),
        [
            "✓ Tool · done",
            "│ grep -rn alpha src",
            "",
            "○ Thinking",
            "│ first thought",
            "",
            "✓ Tool · done",
            "│ cat notes.md",
            "",
            "○ Thinking",
            "│ latest thought",
            "",
        ]
    );
}

#[test]
fn a_running_tool_breaks_the_earlier_group_then_joins_when_completed() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.entries.push(completed_tool(1, "grep -rn alpha src"));
    chat.entries.push(completed_tool(2, "grep -rn beta src"));
    chat.entries.push(ChatEntry::tool(
        3,
        "cat notes.md",
        None,
        ToolStatus::Running,
    ));

    assert_eq!(
        transcript_text(&mut chat, 80),
        [
            "✓ Tool · done",
            "│ grep -rn alpha src, grep -rn beta src",
            "",
            "● Tool · running",
            "│ cat notes.md",
            "",
        ]
    );

    chat.entries[2].touch(4);
    chat.entries[2].tool_status = Some(ToolStatus::Completed);

    // Once completed, the third summary joins the same run immediately.
    assert_eq!(
        transcript_text(&mut chat, 80),
        [
            "✓ Tool · done",
            "│ grep -rn alpha src, grep -rn beta src, cat notes.md",
            "",
        ]
    );
}

#[test]
fn exact_diffstats_are_available_only_after_the_tool_finishes() {
    let item = |status: &str| TranscriptItem {
        stable_id: "tool:edit".into(),
        position: 1,
        latest_content_event_ordinal: None,
        created_at_ms: 1,
        last_changed_at_ms: 2,
        body: TranscriptBody::Tool {
            call: serde_json::json!({
                "toolCallId": "edit",
                "title": "Edit src/lib.rs",
                "status": status,
                "content": [{
                    "type": "diff",
                    "path": "/workspace/src/lib.rs",
                    "oldText": "alpha\n",
                    "newText": "alpha\nbeta\n"
                }]
            }),
            terminal_outputs: Vec::new(),
            terminal_refs: Vec::new(),
            presentation: None,
        },
    };

    assert_eq!(materialized_tool_diffstats(&item("in_progress")), None);
    assert_eq!(
        materialized_tool_diffstats(&item("completed")),
        Some(vec!["/workspace/src/lib.rs  +1 −0".into()])
    );
}

#[test]
fn transcript_blocks_keep_role_headers_and_wrapped_body_indented() {
    let entry = ChatEntry::plain(1, ChatRole::User, "alpha beta gamma");
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.entries.push(entry);
    let text = transcript_text(&mut chat, 12);

    assert_eq!(text, ["❯ You", "│ alpha beta", "│ gamma", ""]);
}

/// A preview is the conversation's own rendering minus its rail: the same
/// rows, wrapped the same way, without the gutter that only means something
/// under a role header.
// Hard-won: 7ab8368b: session previews repeated the transcript gutter after drawing their own prefix.
#[test]
fn agent_preview_tail_matches_the_conversation_body_rows_without_the_gutter() {
    let text = "# heading\n\nfirst paragraph with some words to wrap\n\n- alpha\n- beta";
    let entry = ChatEntry::plain(0, ChatRole::Agent, text);
    // The conversation spends two columns on the gutter, so a preview asked
    // for 38 columns of text renders the same rows as a 40-column transcript.
    let body = render_transcript_entry(&entry, 40, TranscriptRenderMode::Rich)
        .into_iter()
        .skip(1) // header row
        .filter(|line| !line_is_empty(line))
        .map(without_role_gutter)
        .collect::<Vec<_>>();
    assert!(!body.is_empty());
    assert!(
        body.iter().all(|line| line
            .spans
            .first()
            .is_none_or(|span| span.content != role_gutter())),
        "the comparison rows have no gutter left to match"
    );

    assert_eq!(render_agent_message_tail(text, 38, usize::MAX), body);
    assert_eq!(
        render_agent_message_tail(text, 38, 2),
        body[body.len() - 2..].to_vec()
    );
}

/// A session-list summary wraps only as far as its rows need. Those rows, and
/// the ellipsis that says more was left out, are exactly what wrapping the
/// whole message and keeping its first rows gives.
#[test]
fn agent_preview_head_matches_the_first_rows_of_the_whole_message() {
    let findings = (0..40)
        .map(|index| {
            format!(
                "### Finding {index}: `src/file_{index}.rs:{index}`\n\n- {}\n\n",
                "the daemon publishes a revision and every waiter wakes ".repeat(4)
            )
        })
        .collect::<String>();
    let sources = [
        "word ".repeat(4000),
        findings.replace('\n', " "),
        findings,
        "漢字かな交じり文 ".repeat(600),
        "x".repeat(9000),
        "short".to_owned(),
        "\n\n  \n".to_owned(),
        "first line\n**late-corpus diagnostics,**\nthird line".to_owned(),
    ];
    for source in &sources {
        for width in [1, 2, 7, 38, 80] {
            for maximum in [1, 2, 3] {
                let entry = ChatEntry::plain(0, ChatRole::Agent, source);
                let mut whole = entry_body_rows(
                    &entry,
                    width + ROLE_GUTTER_WIDTH,
                    TranscriptRenderMode::Rich,
                )
                .into_iter()
                .filter(|line| !line_is_empty(line))
                .map(without_role_gutter)
                .collect::<Vec<_>>();
                let truncated = whole.len() > maximum;
                whole.truncate(maximum);
                if truncated && let Some(last) = whole.last_mut() {
                    append_trimmed_ellipsis(last, 0);
                }
                assert_eq!(
                    render_agent_message_head(source, width, maximum),
                    whole,
                    "width {width}, {maximum} rows, source {:?}",
                    &source[..source.len().min(40)]
                );
            }
        }
    }
}

#[test]
fn agent_preview_head_removes_punctuation_before_its_ellipsis() {
    let lines = render_agent_message_head(
        "first line\n**late-corpus diagnostics,**\nthird line",
        80,
        2,
    );
    let rendered = lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>();

    // No role gutter: the session list draws its own prefix in front of these
    // rows, and a second marker there means nothing.
    assert_eq!(rendered, ["first line", "late-corpus diagnostics…"]);
    assert!(
        lines[1]
            .spans
            .last()
            .unwrap()
            .style
            .add_modifier
            .contains(Modifier::BOLD)
    );
}

#[test]
fn changing_theme_recolors_cached_conversation_without_changing_text() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.entries
        .push(ChatEntry::plain(1, ChatRole::User, "inspect the renderer"));
    chat.entries.push(ChatEntry::plain(
        2,
        ChatRole::Agent,
        "**Done.** Use `cargo test`.",
    ));
    let mut previous: Option<Vec<Line<'static>>> = None;
    for theme in theme::UiTheme::ALL {
        theme::with_theme(theme, || {
            let lines = transcript_lines(&mut chat, 60);
            assert!(
                lines
                    .iter()
                    .flat_map(|line| &line.spans)
                    .any(|span| { span.style.fg == Some(theme::palette().accent) })
            );
            if let Some(previous) = &previous {
                assert_eq!(line_text(lines.clone()), line_text(previous.clone()));
                assert_ne!(&lines, previous, "existing rows must adopt the new colors");
            }
            previous = Some(lines);
        });
    }
}

#[test]
fn transcript_snapshot_tail_counts_only_nonempty_rows() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.entries.push(ChatEntry::plain(
        1,
        ChatRole::Agent,
        "one\n\ntwo\n\nthree\n\nfour\n\nfive",
    ));

    let mut snapshot = chat.transcript_snapshot();
    let tail = line_text(snapshot.rich_tail(80, 4));

    assert_eq!(tail.len(), 4);
    assert!(tail.iter().all(|line| !line.trim().is_empty()));
    assert_eq!(tail, ["│ two", "│ three", "│ four", "│ five"]);
}

#[test]
fn browser_transcript_is_bounded_utf8_safe_and_supports_deltas() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.entries.push(ChatEntry::plain(
        1,
        ChatRole::Agent,
        (0..1_005)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n"),
    ));
    chat.entries.push(ChatEntry::plain(
        2,
        ChatRole::Thought,
        "🦀".repeat(BROWSER_LINE_BYTES),
    ));
    chat.latest_seq = 2;

    let full = chat.transcript_snapshot().browser_transcript(None);
    assert_eq!(
        full.entries
            .iter()
            .map(|entry| entry.lines.len())
            .sum::<usize>(),
        BROWSER_TRANSCRIPT_LINES
    );
    assert_eq!(full.entries.last().unwrap().role, "thought");
    assert!(
        full.entries[0]
            .lines
            .first()
            .is_some_and(|line| line.contains("earlier lines omitted"))
    );
    let truncated = &full.entries.last().unwrap().lines[0];
    assert!(truncated.ends_with("… [truncated]"));
    assert!(truncated.len() <= BROWSER_LINE_BYTES);
    assert!(!full.reset);

    let delta = chat.transcript_snapshot().browser_transcript(Some(1));
    assert!(!delta.reset);
    assert_eq!(delta.entries.len(), 1);
    assert_eq!(delta.entries[0].updated_seq, 2);
    assert!(chat.transcript_snapshot().browser_transcript(Some(0)).reset);
}

#[test]
fn browser_transcript_excludes_entries_before_provider_compaction() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.entries
        .push(ChatEntry::plain(1, ChatRole::User, "old"));
    chat.entries
        .push(ChatEntry::plain(3, ChatRole::Agent, "current"));
    chat.latest_seq = 3;
    chat.last_compaction_seq = 2;

    let browser = chat.transcript_snapshot().browser_transcript(None);
    assert_eq!(browser.entries.len(), 1);
    assert_eq!(browser.entries[0].lines, ["current"]);
    assert_eq!(browser_tail_label(&browser.entries[0]), "Agent: current");
}

/// A delta has to be proportional to what changed, not to the window. The
/// bodies the projection records no change ordinal for still overshoot, so
/// this asserts a large reduction rather than a minimal one.
// Hard-won: 93fa5d9a: one transcript update retransmitted the full browser window.
#[test]
fn a_delta_costs_a_fraction_of_the_window_it_updates() {
    let mut session = long_materialized_session(600);
    let frontier = session.applied_event_ordinal;
    let bytes =
        |transcript: &BrowserTranscript| serde_json::to_string(&transcript.entries).unwrap().len();
    let window = bytes(&TranscriptSnapshot::from_materialized(&session).browser_transcript(None));

    let appended = frontier + 1;
    session.transcript.push(fixture_agent_item(appended));
    session.applied_event_ordinal = appended;
    let delta = TranscriptSnapshot::from_materialized(&session).browser_transcript(Some(frontier));

    println!("window {window} bytes, delta {} bytes", bytes(&delta));
    assert!(
        bytes(&delta) * 4 < window,
        "one appended message resent {} of {window} bytes",
        bytes(&delta)
    );
}

/// The conversation a delta test needs: settled messages the projection
/// records an exact update cursor for.
fn message_session(items: u64) -> MaterializedSession {
    let mut session = MaterializedSession::empty("session-delta");
    session.transcript = (1..=items)
        .map(|position| match position % 2 {
            1 => fixture_user_item(position),
            _ => fixture_agent_item(position),
        })
        .collect();
    session.applied_event_ordinal = items;
    session
}

fn delta_ids(session: &MaterializedSession, after_seq: u64) -> Vec<u64> {
    let delta = TranscriptSnapshot::from_materialized(session).browser_transcript(Some(after_seq));
    assert!(!delta.reset, "the window still covers the viewer's cursor");
    delta.entries.iter().map(|entry| entry.id).collect()
}

// Hard-won: 93fa5d9a: one transcript update retransmitted the full browser window.
#[test]
fn appending_one_message_marks_only_that_entry_changed() {
    let mut session = message_session(8);
    let opened = TranscriptSnapshot::from_materialized(&session).browser_transcript(None);
    assert_eq!(opened.entries.len(), 8);
    let cursor = opened.latest_seq;

    session.transcript.push(fixture_agent_item(9));
    session.applied_event_ordinal = 9;

    assert_eq!(delta_ids(&session, cursor), [9]);
}

// Hard-won: 93fa5d9a: one transcript update retransmitted the full browser window.
#[test]
fn a_growing_agent_message_is_the_only_entry_its_delta_carries() {
    let mut session = message_session(6);
    let cursor = TranscriptSnapshot::from_materialized(&session)
        .browser_transcript(None)
        .latest_seq;

    let streaming = Arc::make_mut(&mut session.transcript[5]);
    let TranscriptBody::Agent { chunks, .. } = &mut streaming.body else {
        panic!("expected an agent message");
    };
    chunks.push(serde_json::json!({
        "content": {"type": "text", "text": " and one more thing"}
    }));
    streaming.latest_content_event_ordinal = Some(7);
    streaming.last_changed_at_ms = FIXTURE_MS + 1;
    session.applied_event_ordinal = 7;

    assert_eq!(delta_ids(&session, cursor), [6]);
    let delta = TranscriptSnapshot::from_materialized(&session).browser_transcript(Some(cursor));
    assert!(delta.entries[0].lines[0].ends_with(" and one more thing"));
}

/// I1-7: after a resume the pane opened on the revealed reply ("message 7 of
/// N") and stayed there when a new reply arrived. The reveal is not a user
/// scroll, so new content brings the view back to the tail.
// Hard-won: bf09b3d6: a resumed pane stayed on its opening reveal after a new reply arrived.
#[test]
fn new_content_after_the_opening_reveal_follows_the_tail() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.entries.push(ChatEntry::plain(
        1,
        ChatRole::Agent,
        "response advertised on the dashboard",
    ));
    for index in 0..8 {
        chat.entries.push(ChatEntry::plain(
            index + 2,
            ChatRole::System,
            format!("terminal failure {index}\n{}", "output\n".repeat(12)),
        ));
    }
    let opened = drawn_transcript(&mut chat, 60, 24);
    assert!(
        shows(&opened, "End to follow"),
        "the reveal opens mid-history"
    );

    chat.entries.push(ChatEntry::plain(
        20,
        ChatRole::Agent,
        "the reply after resume",
    ));
    let rows = drawn_transcript(&mut chat, 60, 24);
    assert!(shows(&rows, "the reply after resume"));
    assert!(!shows(&rows, "End to follow"));
}

/// D-14: a narrow pinned pane opened on the reveal of an earlier reply, the
/// dashboard switched away and back (a relaunch did this through the
/// workspace it opened first), and the pane came back parked on that reply
/// while the new reply landed below it. The saved position was the reveal's
/// anchor, which the reopened view took for a reader's own scroll.
// Hard-won: 24c88abc: a reopened pane treated its saved opening reveal as a user scroll.
#[test]
fn a_view_reopened_from_the_opening_reveal_follows_new_rows() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.entries
        .push(ChatEntry::plain(1, ChatRole::Agent, "earlier reply"));
    chat.entries.push(ChatEntry::plain(
        2,
        ChatRole::User,
        format!("detach probe\n{}", "wrapped line\n".repeat(30)),
    ));
    let opened = drawn_transcript(&mut chat, 31, 12);
    assert!(
        shows(&opened, "earlier reply"),
        "the reveal parks on the earlier reply: {opened:?}"
    );
    let position = chat.transcript_position();

    let mut unchanged = ChatState::new(&snapshot(), &[]);
    unchanged.entries = chat.entries.clone();
    unchanged.restore_transcript_position(position);
    assert_eq!(drawn_transcript(&mut unchanged, 31, 12), opened);

    let mut reopened = ChatState::new(&snapshot(), &[]);
    reopened.entries = chat.entries.clone();
    reopened
        .entries
        .push(ChatEntry::plain(3, ChatRole::Agent, "reply to the probe"));
    reopened.restore_transcript_position(position);
    let rows = drawn_transcript(&mut reopened, 31, 12);

    assert!(shows(&rows, "reply to the probe"), "{rows:?}");

    // A deliberate reader scroll survives the same save and restore path.
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.entries.extend(
        (0..40).map(|index| ChatEntry::plain(index + 1, ChatRole::User, format!("line {index}"))),
    );
    drawn_transcript(&mut chat, 60, 24);
    chat.handle_key(key(KeyCode::PageUp));
    drawn_transcript(&mut chat, 60, 24);
    let position = chat.transcript_position();

    let mut reopened = ChatState::new(&snapshot(), &[]);
    reopened.entries = chat.entries.clone();
    reopened
        .entries
        .push(ChatEntry::plain(100, ChatRole::Agent, "late reply"));
    reopened.restore_transcript_position(position);
    let rows = drawn_transcript(&mut reopened, 60, 24);

    assert!(shows(&rows, "End to follow"), "{rows:?}");
    assert!(!shows(&rows, "late reply"));
}

// Hard-won: 24c88abc: switching back to a tail repeated the opening reveal.
#[test]
fn a_reopened_tail_does_not_repeat_the_opening_reveal() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.entries.push(ChatEntry::plain(
        1,
        ChatRole::Agent,
        (1..=60)
            .map(|line| format!("reply line {line}\n"))
            .collect::<String>(),
    ));
    drawn_transcript(&mut chat, 60, 24);
    chat.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL));
    let expected = drawn_transcript(&mut chat, 60, 24);

    for _ in 0..3 {
        let position = chat.transcript_position();
        let mut reopened = ChatState::new(&snapshot(), &[]);
        reopened.entries = chat.entries.clone();
        reopened.restore_transcript_position(position);
        assert_eq!(drawn_transcript(&mut reopened, 60, 24), expected);
        chat = reopened;
    }
}

// Hard-won: 2b229f70: a saved full-history index was restored against the tail.
#[test]
fn a_reopened_scroll_survives_tail_first_history_loading_in_either_order() {
    let session = long_materialized_session(TAIL_SEED_ITEMS as u64 + 120);
    for entry in [1, 121] {
        let mut chat = ChatState::from_materialized(&session, &[], &[]);
        chat.anchor = TranscriptAnchor::Row { entry, row: 3 };
        let expected = drawn_transcript(&mut chat, 60, 24);
        let position: TranscriptPosition =
            serde_json::from_value(serde_json::to_value(chat.transcript_position()).unwrap())
                .unwrap();
        // Fixed previous-build handoff shape: legacy indexes refer to the
        // full transcript, even when the new view has only loaded its tail.
        let legacy: TranscriptPosition = serde_json::from_value(serde_json::json!({
            "Row": {"entry": entry, "row": 3}
        }))
        .unwrap();

        for saved in [position, legacy] {
            for draw_before_history in [false, true] {
                let mut reopened = ChatState::from_materialized_tail(&session, &[], &[]);
                let prefix = converted_prefix(&session, &reopened);
                reopened.restore_transcript_position(saved);
                if draw_before_history {
                    let rows = drawn_transcript(&mut reopened, 60, 24);
                    if entry < reopened.unconverted_prefix() {
                        assert!(shows(&rows, "Loading"));
                        assert!(
                            reopened
                                .frame_surfaces()
                                .surface(SurfaceId::Transcript)
                                .is_none()
                        );
                        assert!(reopened.transcript_scrollbar.pointer.geometry().is_none());
                    }
                    // Switching away again while history loads must keep the
                    // original position, rather than save a temporary viewport.
                    let saved = reopened.transcript_position();
                    reopened.restore_transcript_position(saved);
                }
                assert!(reopened.splice_transcript_prefix(prefix));
                assert_eq!(
                    drawn_transcript(&mut reopened, 60, 24),
                    expected,
                    "entry {entry}, draw before history {draw_before_history}, saved {saved:?}"
                );
            }
        }
    }

    // Saving from a partial view must use the same entry identity after
    // arriving messages move the next open's tail-loading boundary.
    let mut partial = ChatState::from_materialized_tail(&session, &[], &[]);
    partial.anchor = TranscriptAnchor::Row { entry: 1, row: 3 };
    drawn_transcript(&mut partial, 60, 24);
    let saved = partial.transcript_position();
    let mut newer = session.clone();
    let next = newer.transcript.len() as u64 + 1;
    newer
        .transcript
        .extend((next..next + 20).map(fixture_user_item));
    newer.applied_event_ordinal += 20;
    let mut expected = ChatState::from_materialized(&newer, &[], &[]);
    expected.anchor = TranscriptAnchor::Row { entry: 121, row: 3 };
    let expected = drawn_transcript(&mut expected, 60, 24);

    let mut reopened = ChatState::from_materialized_tail(&newer, &[], &[]);
    let prefix = converted_prefix(&newer, &reopened);
    reopened.restore_transcript_position(saved);
    assert!(shows(&drawn_transcript(&mut reopened, 60, 24), "Loading"));
    assert!(reopened.splice_transcript_prefix(prefix));
    assert_eq!(drawn_transcript(&mut reopened, 60, 24), expected);
}

#[test]
fn mouse_wheel_reaches_the_tail_across_a_large_collapsed_tool_run() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.entries
        .push(ChatEntry::plain(1, ChatRole::User, "before tools"));
    chat.entries
        .extend((2..102).map(|seq| completed_tool(seq, &format!("command {seq}"))));
    chat.entries.extend((0..20).map(|index| {
        ChatEntry::plain(102 + index, ChatRole::User, format!("tail message {index}"))
    }));
    let wheel = |kind| mouse_in(kind, Rect::new(0, 10, 60, 1));
    let mut rows = drawn_transcript(&mut chat, 60, 24);

    let mut upward_steps = 0;
    while !shows(&rows, "before tools") && upward_steps < 40 {
        chat.handle_mouse(wheel(MouseEventKind::ScrollUp));
        rows = drawn_transcript(&mut chat, 60, 24);
        upward_steps += 1;
    }
    assert!(shows(&rows, "before tools"), "wheel up reached old history");

    for _ in 0..=upward_steps {
        chat.handle_mouse(wheel(MouseEventKind::ScrollDown));
        rows = drawn_transcript(&mut chat, 60, 24);
    }
    assert!(
        shows(&rows, "tail message 19"),
        "wheel down reached the tail"
    );
    assert!(
        !rows.iter().any(|row| row.contains("End to follow")),
        "the conversation resumed following after crossing hidden tool entries"
    );
}

// Hard-won: 1f30f668: scrolling an empty transcript panicked on a missing render-cache entry.
#[test]
fn the_wheel_over_an_empty_transcript_has_nothing_to_scroll() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    let rows = drawn_transcript(&mut chat, 40, 24);

    chat.handle_mouse(wheel(MouseEventKind::ScrollUp));
    chat.handle_mouse(wheel(MouseEventKind::ScrollDown));

    assert_eq!(drawn_transcript(&mut chat, 40, 24), rows);
}

#[test]
fn scrolled_history_stays_put_while_new_messages_stream_in() {
    let mut chat = numbered_chat(40);
    let _ = drawn_transcript(&mut chat, 60, 24);
    chat.handle_key(key(KeyCode::PageUp));
    let before = drawn_transcript(&mut chat, 60, 24);
    assert!(shows(&before, "End to follow"));

    for index in 40..50 {
        chat.entries.push(ChatEntry::plain(
            index as u64,
            ChatRole::User,
            format!("message {index}"),
        ));
    }
    chat.entries
        .push(ChatEntry::plain(100, ChatRole::Agent, "late reply"));
    let after = drawn_transcript(&mut chat, 60, 24);

    assert_eq!(
        visible_messages(&before),
        visible_messages(&after),
        "appending messages must not move a scrolled-back viewport"
    );
    assert!(!visible_messages(&after).is_empty());
    assert!(
        shows(&after, "End to follow"),
        "the scrolled reader remains marked as behind the tail"
    );
    assert!(!shows(&after, "late reply"));
}

#[test]
fn adjacent_thought_messages_coalesce_without_an_extra_separator() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    for (seq, id, text) in [(1, "one", "first thought"), (2, "two", "second thought")] {
        chat.apply_session_update(
            seq,
            &serde_json::json!({
                "sessionUpdate": "agent_thought_chunk",
                "messageId": id,
                "content": {"type": "text", "text": text}
            }),
        );
    }

    assert_eq!(chat.entries.len(), 1);
    assert_eq!(chat.entries[0].text, "first thought\nsecond thought");
    let rendered = transcript_text(&mut chat, 80);
    assert_eq!(
        rendered
            .iter()
            .filter(|line| line.contains("Thinking"))
            .count(),
        1
    );
    assert_eq!(
        rendered,
        ["○ Thinking", "│ first thought", "│ second thought", ""]
    );
}

#[test]
fn materialized_terminal_content_renders_output_and_exit_summary() {
    let mut session = MaterializedSession::empty("session-terminal");
    session.applied_event_ordinal = 1;
    session.applied_event_digest = "a".repeat(64);
    session.transcript = vec![Arc::new(TranscriptItem {
        stable_id: "tool:bash".into(),
        position: 1,
        latest_content_event_ordinal: None,
        created_at_ms: 1,
        last_changed_at_ms: 1,
        body: TranscriptBody::Tool {
            call: serde_json::json!({
                "toolCallId": "bash",
                "title": "Bash",
                "status": "completed",
                "content": [{"type": "terminal", "terminalId": "term-1"}]
            }),
            terminal_outputs: vec![TerminalOutputRecord {
                terminal_id: "term-1".into(),
                // Colored output from a real build tool: the escape must
                // not survive into the terminal hel is drawing on.
                output: "\u{1b}[32mtests passed\u{1b}[0m".into(),
                truncated: false,
                exit_code: Some(0),
                signal: None,
            }],
            terminal_refs: vec!["term-1".into()],
            presentation: None,
        },
    })];

    let entries = materialized_chat_entries(&session);
    assert_eq!(entries[0].tool_content, ["tests passed\nexited 0"]);

    let mut chat = ChatState::from_materialized(&session, &[], &[]);
    chat.render_mode = TranscriptRenderMode::Raw;
    let rendered = transcript_text(&mut chat, 80);
    assert!(
        rendered.iter().any(|line| line.contains("tests passed")),
        "raw rows show the captured output: {rendered:?}"
    );
    assert!(
        rendered.iter().any(|line| line.contains("exited 0")),
        "raw rows show how the terminal ended: {rendered:?}"
    );
    assert!(
        !rendered.iter().any(|line| line.contains("terminal term-1")),
        "the id placeholder is replaced once output exists: {rendered:?}"
    );
    assert!(
        !rendered.iter().any(|line| line.contains('\u{1b}')),
        "escape sequences are sanitized out: {rendered:?}"
    );

    let browser = TranscriptSnapshot::from_materialized(&session).browser_transcript(None);
    assert_eq!(
        browser.entries[0].lines,
        ["Bash"],
        "the remote viewer shows the decluttered title, not the output"
    );
}

#[test]
fn kimi_text_and_captured_terminal_output_render_once_and_only_in_raw_mode() {
    const OUTPUT: &str = "toolchain inventory";
    let mut session = MaterializedSession::empty("session-kimi-terminal");
    session.applied_event_ordinal = 1;
    session.applied_event_digest = "a".repeat(64);
    session.transcript = vec![Arc::new(TranscriptItem {
        stable_id: "tool:kimi-shell".into(),
        position: 1,
        latest_content_event_ordinal: None,
        created_at_ms: 1,
        last_changed_at_ms: 1,
        body: TranscriptBody::Tool {
            call: serde_json::json!({
                "toolCallId": "kimi-shell",
                "title": "Execute `inspect toolchain`",
                "status": "completed",
                "content": [{
                    "type": "content",
                    "content": {"type": "text", "text": OUTPUT}
                }],
                "rawOutput": {
                    "type": "Bash",
                    "output": OUTPUT.as_bytes(),
                    "exit_code": 1,
                    "command": "inspect toolchain"
                }
            }),
            terminal_outputs: vec![TerminalOutputRecord {
                terminal_id: "term-1".into(),
                output: OUTPUT.into(),
                truncated: false,
                exit_code: Some(1),
                signal: None,
            }],
            terminal_refs: vec!["term-1".into()],
            presentation: None,
        },
    })];

    let entries = materialized_chat_entries(&session);
    assert_eq!(entries[0].tool_content, [OUTPUT, "exited 1"]);

    let mut chat = ChatState::from_materialized(&session, &[], &[]);
    let rich = transcript_text(&mut chat, 80);
    assert!(
        !rich.iter().any(|line| line.contains(OUTPUT)),
        "Rich mode shows the tool call, not its duplicate output: {rich:?}"
    );

    chat.render_mode = TranscriptRenderMode::Raw;
    let raw = transcript_text(&mut chat, 80);
    assert_eq!(
        raw.iter().filter(|line| line.contains(OUTPUT)).count(),
        1,
        "Raw mode keeps one copy of the output: {raw:?}"
    );
    assert!(raw.iter().any(|line| line.contains("exited 1")));

    let browser = TranscriptSnapshot::from_materialized(&session).browser_transcript(None);
    assert!(
        browser
            .entries
            .iter()
            .flat_map(|entry| &entry.lines)
            .all(|line| !line.contains(OUTPUT)),
        "the remote Rich feed suppresses the duplicate output"
    );
}

#[test]
fn legacy_kimi_duplicate_is_suppressed_without_reprojecting_history() {
    const OUTPUT: &str = "legacy failed output";
    let mut session = MaterializedSession::empty("session-legacy-kimi-terminal");
    session.applied_event_ordinal = 2;
    session.applied_event_digest = "a".repeat(64);
    session.transcript = vec![
        Arc::new(TranscriptItem {
            stable_id: "tool:kimi-shell".into(),
            position: 1,
            latest_content_event_ordinal: None,
            created_at_ms: 1,
            last_changed_at_ms: 2,
            body: TranscriptBody::Tool {
                call: serde_json::json!({
                    "toolCallId": "kimi-shell",
                    "title": "Execute `inspect toolchain`",
                    "status": "completed",
                    "content": [{
                        "type": "content",
                        "content": {"type": "text", "text": OUTPUT}
                    }],
                    "rawOutput": {
                        "type": "Bash",
                        "output": OUTPUT.as_bytes(),
                        "exit_code": 1,
                        "command": "inspect toolchain"
                    }
                }),
                terminal_outputs: Vec::new(),
                terminal_refs: Vec::new(),
                presentation: None,
            },
        }),
        Arc::new(TranscriptItem {
            stable_id: "terminal:term-1".into(),
            position: 2,
            latest_content_event_ordinal: None,
            created_at_ms: 2,
            last_changed_at_ms: 2,
            body: TranscriptBody::TerminalOutput {
                record: TerminalOutputRecord {
                    terminal_id: "term-1".into(),
                    output: OUTPUT.into(),
                    truncated: false,
                    exit_code: Some(1),
                    signal: None,
                },
            },
        }),
    ];

    let entries = materialized_chat_entries(&session);
    assert!(entries[1].raw_only);
    let mut chat = ChatState::from_materialized(&session, &[], &[]);
    let rich = transcript_text(&mut chat, 80);
    assert!(
        !rich.iter().any(|line| line.contains(OUTPUT)),
        "an existing duplicate becomes quiet after upgrading: {rich:?}"
    );
    let browser = TranscriptSnapshot::from_materialized(&session).browser_transcript(None);
    assert!(
        browser
            .entries
            .iter()
            .flat_map(|entry| &entry.lines)
            .all(|line| !line.contains(OUTPUT))
    );
}

const STANDALONE_OUTPUT: &str = "cargo build finished";

fn terminal_record(exit_code: Option<u32>, signal: Option<&str>) -> TerminalOutputRecord {
    TerminalOutputRecord {
        terminal_id: "term-1".into(),
        output: STANDALONE_OUTPUT.into(),
        truncated: false,
        exit_code,
        signal: signal.map(str::to_owned),
    }
}

fn terminal_output_item(position: u64, record: TerminalOutputRecord) -> Arc<TranscriptItem> {
    Arc::new(TranscriptItem {
        stable_id: format!("terminal:{}", record.terminal_id),
        position,
        latest_content_event_ordinal: None,
        created_at_ms: position as i64,
        last_changed_at_ms: position as i64,
        body: TranscriptBody::TerminalOutput { record },
    })
}

/// A hel-hosted command whose output no tool call refers to, after an agent
/// message so the feed has something else to show.
fn standalone_terminal_session(record: TerminalOutputRecord) -> MaterializedSession {
    let mut session = MaterializedSession::empty("session-standalone-terminal");
    session.applied_event_ordinal = 2;
    session.transcript = vec![
        agent_message_item("agent:1", 1, "running the build"),
        terminal_output_item(2, record),
    ];
    session
}

fn fallback_terminal_session(record: TerminalOutputRecord) -> MaterializedSession {
    let mut call =
        mj_core::acp::fallback_terminal_tool_call(&record.terminal_id, "cargo build".into());
    call.status = if record.exited_cleanly() {
        ToolCallStatus::Completed
    } else {
        ToolCallStatus::Failed
    };
    let mut session = MaterializedSession::empty("session-fallback-terminal");
    session.applied_event_ordinal = 1;
    session.transcript = vec![Arc::new(TranscriptItem {
        stable_id: format!("tool:{}", call.tool_call_id),
        position: 1,
        latest_content_event_ordinal: None,
        created_at_ms: 1,
        last_changed_at_ms: 2,
        body: TranscriptBody::Tool {
            call: serde_json::to_value(call).unwrap(),
            terminal_refs: vec![record.terminal_id.clone()],
            terminal_outputs: vec![record],
            presentation: None,
        },
    })];
    session
}

fn browser_lines(session: &MaterializedSession) -> Vec<String> {
    TranscriptSnapshot::from_materialized(session)
        .browser_transcript(None)
        .entries
        .into_iter()
        .flat_map(|entry| entry.lines)
        .collect()
}

#[test]
fn a_cleanly_exited_standalone_terminal_item_renders_only_in_raw_mode() {
    let session = standalone_terminal_session(terminal_record(Some(0), None));

    let mut chat = ChatState::from_materialized(&session, &[], &[]);
    let rich = transcript_text(&mut chat, 80);
    assert!(
        rich.iter().any(|line| line.contains("running the build")),
        "the rest of the conversation still renders: {rich:?}"
    );
    assert!(
        !rich.iter().any(|line| line.contains(STANDALONE_OUTPUT)),
        "a clean command's output is left out of the rich feed: {rich:?}"
    );
    assert!(
        !rich.iter().any(|line| line.contains("exited 0")),
        "and so is its exit summary: {rich:?}"
    );

    let browser = browser_lines(&session);
    assert!(
        browser
            .iter()
            .any(|line| line.contains("running the build")),
        "the rest of the conversation still reaches the remote viewer: {browser:?}"
    );
    assert!(
        !browser.iter().any(|line| line.contains(STANDALONE_OUTPUT)),
        "the remote viewer mirrors the rich feed: {browser:?}"
    );

    chat.render_mode = TranscriptRenderMode::Raw;
    let raw = transcript_text(&mut chat, 80);
    assert!(
        raw.iter().any(|line| line.contains(STANDALONE_OUTPUT)),
        "raw rows keep the captured output: {raw:?}"
    );
    assert!(
        raw.iter().any(|line| line.contains("exited 0")),
        "raw rows keep how the terminal ended: {raw:?}"
    );
}

#[test]
fn a_failed_fallback_terminal_tool_remains_visible() {
    let session = fallback_terminal_session(terminal_record(Some(3), None));
    let entries = materialized_chat_entries(&session);
    assert!(!entries[0].raw_only);

    let mut chat = ChatState::from_materialized(&session, &[], &[]);
    let rich = transcript_text(&mut chat, 80);
    assert!(rich.iter().any(|line| line.contains("cargo build")));
    assert!(rich.iter().any(|line| line.contains(STANDALONE_OUTPUT)));
}

#[test]
fn an_abnormally_ended_standalone_terminal_item_renders_everywhere() {
    for (record, summary) in [
        (terminal_record(Some(3), None), "exited 3"),
        (terminal_record(None, Some("SIGKILL")), "killed by SIGKILL"),
        (
            terminal_record(Some(0), Some("SIGKILL")),
            "killed by SIGKILL",
        ),
        (terminal_record(None, None), "released before exit"),
    ] {
        let session = standalone_terminal_session(record);

        let mut chat = ChatState::from_materialized(&session, &[], &[]);
        let rich = transcript_text(&mut chat, 80);
        assert!(
            rich.iter().any(|line| line.contains(STANDALONE_OUTPUT)),
            "{summary}: the rich feed keeps the output: {rich:?}"
        );
        assert!(
            rich.iter().any(|line| line.contains(summary)),
            "{summary}: the rich feed says how it ended: {rich:?}"
        );

        let browser = browser_lines(&session);
        assert!(
            browser.iter().any(|line| line.contains(STANDALONE_OUTPUT)),
            "{summary}: the remote viewer keeps the output: {browser:?}"
        );
        assert!(
            browser.iter().any(|line| line.contains(summary)),
            "{summary}: the remote viewer says how it ended: {browser:?}"
        );
    }
}

#[test]
fn a_clean_standalone_terminal_item_between_completed_tools_keeps_one_run() {
    let mut session = MaterializedSession::empty("session-terminal-between-tools");
    session.applied_event_ordinal = 3;
    session.transcript = vec![
        fixture_tool_item(1),
        terminal_output_item(2, terminal_record(Some(0), None)),
        fixture_tool_item(3),
    ];
    let mut chat = ChatState::from_materialized(&session, &[], &[]);
    chat.entries
        .push(ChatEntry::plain(4, ChatRole::User, "now ship it"));

    let text = transcript_text(&mut chat, 80);

    assert_eq!(
        text,
        [
            "✓ Tool · done",
            "│ read, read",
            "",
            "❯ You",
            "│ now ship it",
            "",
        ],
        "the omitted entry neither renders nor splits the run"
    );
}

/// Grok Build's final update replaces `content` with plain text, so the
/// output hel captured is attached to the item with nothing in the call
/// pointing at it. It is still the only copy of what the command printed.
// Hard-won: b364ed85: Grok replacing a call terminal reference lost its captured output.
#[test]
fn attached_terminal_output_renders_when_the_call_no_longer_refers_to_it() {
    let mut session = MaterializedSession::empty("session-dropped-terminal");
    session.applied_event_ordinal = 1;
    session.applied_event_digest = "a".repeat(64);
    session.transcript = vec![Arc::new(TranscriptItem {
        stable_id: "tool:bash".into(),
        position: 1,
        latest_content_event_ordinal: None,
        created_at_ms: 1,
        last_changed_at_ms: 1,
        body: TranscriptBody::Tool {
            call: serde_json::json!({
                "toolCallId": "bash",
                "title": "Bash",
                "status": "completed",
                "content": [{
                    "type": "content",
                    "content": {"type": "text", "text": "ran the build"}
                }]
            }),
            terminal_outputs: vec![TerminalOutputRecord {
                terminal_id: "term-1".into(),
                output: "build finished".into(),
                truncated: false,
                exit_code: Some(0),
                signal: None,
            }],
            terminal_refs: vec!["term-1".into()],
            presentation: None,
        },
    })];

    let entries = materialized_chat_entries(&session);
    assert_eq!(
        entries[0].tool_content,
        ["ran the build", "build finished\nexited 0"],
        "the captured output follows the content the call still carries"
    );
}

/// Codex runs the command in its own terminal, which hel never opened, and
/// reports the text in `rawOutput` beside the reference.
#[test]
fn codex_raw_output_renders_for_a_terminal_hel_has_no_record_for() {
    let call = |raw_output: serde_json::Value| {
        serde_json::json!({
            "toolCallId": "exec",
            "title": "Shell",
            "status": "completed",
            "content": [{"type": "terminal", "terminalId": "exec-1"}],
            "rawOutput": raw_output
        })
    };
    let details = |raw_output: serde_json::Value| {
        let call = ToolCall::deserialize(&call(raw_output)).expect("valid ACP tool call");
        tool_content_details(&call.content, &[], call.raw_output.as_ref())
    };

    assert_eq!(
        details(serde_json::json!({"formatted_output": "tests passed", "exit_code": 0})),
        ["tests passed\nexited 0"]
    );
    assert_eq!(
        details(serde_json::json!({"formatted_output": "still running"})),
        ["still running"],
        "an exit line needs an exit code to report"
    );
    assert_eq!(
        details(serde_json::json!({"exit_code": 0})),
        ["terminal exec-1"],
        "without output there is nothing to show but the id"
    );
}

#[test]
fn current_stored_tool_presentation_is_preserved() {
    let mut session = MaterializedSession::empty("session-stored-tool-summary");
    session.applied_event_ordinal = 1;
    session.applied_event_digest = "a".repeat(64);
    session.transcript = vec![Arc::new(TranscriptItem {
        stable_id: "tool:git".into(),
        position: 1,
        latest_content_event_ordinal: None,
        created_at_ms: 1,
        last_changed_at_ms: 1,
        body: TranscriptBody::Tool {
            call: serde_json::to_value(
                ToolCall::new("git", "Bash")
                    .kind(ToolKind::Execute)
                    .status(ToolCallStatus::Completed)
                    .raw_input(serde_json::json!({ "command": "git add src/lib.rs" })),
            )
            .unwrap(),
            terminal_outputs: Vec::new(),
            terminal_refs: Vec::new(),
            presentation: Some(Box::new(mj_core::transcript::ToolCallPresentation {
                summary: "git".into(),
                source: "git add src/lib.rs".into(),
                source_kind: mj_core::transcript::ToolSummarySourceKind::RawInput,
                tool_kind: ToolKind::Execute,
                summary_version: mj_transcript::transcript::TOOL_SUMMARY_VERSION,
            })),
        },
    })];

    let browser = TranscriptSnapshot::from_materialized(&session).browser_transcript(None);
    assert_eq!(browser.entries[0].lines, ["git"]);
}

#[test]
fn stale_stored_tool_presentation_is_reparsed_for_rich_surfaces() {
    let source = "python3 <<'PYEOF'\nprint('x')\nPYEOF\ngrep -n x file | head";
    let mut session = MaterializedSession::empty("session-stale-tool-summary");
    session.applied_event_ordinal = 1;
    session.applied_event_digest = "a".repeat(64);
    session.transcript = vec![Arc::new(TranscriptItem {
        stable_id: "tool:heredoc".into(),
        position: 1,
        latest_content_event_ordinal: None,
        created_at_ms: 1,
        last_changed_at_ms: 1,
        body: TranscriptBody::Tool {
            call: serde_json::to_value(
                ToolCall::new("heredoc", "Bash")
                    .kind(ToolKind::Execute)
                    .status(ToolCallStatus::Completed)
                    .raw_input(serde_json::json!({ "command": source })),
            )
            .unwrap(),
            terminal_outputs: Vec::new(),
            terminal_refs: Vec::new(),
            presentation: Some(Box::new(mj_core::transcript::ToolCallPresentation {
                summary: "python3 grep | head".into(),
                source: source.into(),
                source_kind: mj_core::transcript::ToolSummarySourceKind::RawInput,
                tool_kind: ToolKind::Execute,
                summary_version: 0,
            })),
        },
    })];

    let browser = TranscriptSnapshot::from_materialized(&session).browser_transcript(None);
    assert_eq!(browser.entries[0].lines, ["python3 ; grep | head"]);
}

#[test]
fn browser_uses_rich_group_order_and_changes_its_topology_key() {
    let first = completed_execute_tool(1, "Running: git add src/lib.rs");
    let initial = TranscriptSnapshot::from_entries(vec![first.clone()]).browser_transcript(None);
    assert_eq!(initial.entries.len(), 1);
    assert_eq!(initial.entries[0].lines, ["git add"]);

    let grouped = TranscriptSnapshot::from_entries(vec![
        first,
        thought(2, "checking commands"),
        completed_execute_tool(3, "Running: gh pr create --draft"),
    ])
    .browser_transcript(None);

    assert_ne!(grouped.presentation_key, initial.presentation_key);
    assert_eq!(grouped.entries.len(), 2);
    assert_eq!(grouped.entries[0].role, "thought");
    assert_eq!(grouped.entries[0].lines, ["checking commands"]);
    assert_eq!(grouped.entries[1].role, "tool");
    assert_eq!(grouped.entries[1].id, 1);
    assert_eq!(grouped.entries[1].updated_seq, 3);
    assert_eq!(grouped.entries[1].lines, ["git add, gh pr create"]);
}

/// The transcript pane the last frame registered.
fn transcript_pane(chat: &ChatState) -> SurfaceFrame {
    *chat
        .frame_surfaces()
        .surface(SurfaceId::Transcript)
        .expect("the transcript is registered")
}

/// One auto-scroll tick: scroll the transcript, redraw so the registry
/// describes the rows now on screen, and re-resolve the still pointer against
/// it. This is the sequence the dashboard loop runs on its interval.
fn autoscroll_tick(chat: &mut ChatState, selection: &mut SelectionState, direction: i8) {
    if direction < 0 {
        chat.scroll_history_up(3);
    } else {
        chat.scroll_history_down(3);
    }
    drawn_transcript_selecting(chat, 40, 12, true);
    selection.retrack(chat.frame_surfaces());
}

/// A drag held at the transcript's top edge keeps pulling older rows into the
/// selection, and the copied text is the cached rows for the whole span —
/// including the rows the viewport has scrolled past.
#[test]
fn autoscrolling_a_transcript_drag_selects_rows_the_viewport_scrolled_past() {
    let mut chat = numbered_chat(60);
    drawn_transcript(&mut chat, 40, 12);
    let rows = transcript_text(&mut chat, 40);
    let pane = transcript_pane(&chat);
    let height = usize::from(pane.rect.height);
    let mut selection = SelectionState::new();

    // Press on the last visible row, then drag onto the top edge, which is
    // where a held pointer asks for auto-scroll.
    selection.on_mouse_down(
        pane.rect.right() - 1,
        pane.rect.bottom() - 1,
        chat.frame_surfaces(),
    );
    selection.on_mouse_drag(pane.rect.x, pane.rect.y, chat.frame_surfaces());
    let mut span = selection.range().expect("dragging").end.row
        - selection.range().expect("dragging").start.row;
    assert_eq!(span + 1, height, "the drag starts covering the viewport");
    assert_eq!(
        selection.autoscroll_request(chat.frame_surfaces()),
        Some((SurfaceId::Transcript, -1))
    );

    for _ in 0..4 {
        autoscroll_tick(&mut chat, &mut selection, -1);
        let range = selection.range().expect("still dragging");
        let grown = range.end.row - range.start.row;
        assert_eq!(grown, span + 3, "each tick pulls three more rows in");
        span = grown;
    }

    let range = selection.range().expect("still dragging");
    assert!(
        span + 1 > height,
        "the selection outgrew the viewport it started in"
    );
    let copied = chat
        .transcript_selection_text(&range)
        .expect("the selection has text");
    let start = rows.len() - (span + 1);
    // Copying leaves out the role gutter, which only decorates a row.
    assert_eq!(
        copied.split('\n').collect::<Vec<_>>(),
        rows[start..]
            .iter()
            .map(|row| row.strip_prefix(role_gutter()).unwrap_or(row).trim_end())
            .collect::<Vec<_>>()
    );
}

#[test]
fn scrolling_under_a_frozen_base_moves_the_registered_top_row_by_the_rows_crossed() {
    let mut chat = numbered_chat(60);
    drawn_transcript(&mut chat, 40, 12);
    let pinned = transcript_pane(&chat).top_row;

    chat.scroll_history_up(3);
    drawn_transcript_selecting(&mut chat, 40, 12, true);
    assert_eq!(pinned - transcript_pane(&chat).top_row, 3);

    chat.scroll_history_up(7);
    drawn_transcript_selecting(&mut chat, 40, 12, true);
    assert_eq!(pinned - transcript_pane(&chat).top_row, 10);

    chat.scroll_history_down(4);
    drawn_transcript_selecting(&mut chat, 40, 12, true);
    assert_eq!(pinned - transcript_pane(&chat).top_row, 6);

    // With no selection to hold it, the base re-pins to whatever is on screen.
    drawn_transcript(&mut chat, 40, 12);
    assert_eq!(transcript_pane(&chat).top_row, pinned);
}

#[test]
fn a_width_change_or_a_rebuilt_cache_invalidates_a_frozen_row_space() {
    let mut chat = numbered_chat(60);
    drawn_transcript(&mut chat, 40, 12);
    drawn_transcript_selecting(&mut chat, 40, 12, true);
    assert!(
        !chat.transcript_selection_invalidated(),
        "a steady layout keeps the row space"
    );

    drawn_transcript_selecting(&mut chat, 60, 12, true);
    assert!(
        chat.transcript_selection_invalidated(),
        "rewrapped rows are not the rows the selection was measured in"
    );

    drawn_transcript_selecting(&mut chat, 60, 12, true);
    assert!(!chat.transcript_selection_invalidated());
    chat.invalidate_render_cache();
    drawn_transcript_selecting(&mut chat, 60, 12, true);
    assert!(chat.transcript_selection_invalidated());
}

#[test]
fn a_jump_across_the_deep_past_invalidates_instead_of_walking_it() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.render_mode = TranscriptRenderMode::Raw;
    chat.entries = (0..40)
        .map(|index| {
            ChatEntry::plain(
                index,
                ChatRole::User,
                (0..700)
                    .map(|row| format!("entry {index} row {row}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        })
        .collect();
    drawn_transcript(&mut chat, 40, 12);
    drawn_transcript_selecting(&mut chat, 40, 12, true);
    assert!(!chat.transcript_selection_invalidated());

    chat.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::CONTROL));
    drawn_transcript_selecting(&mut chat, 40, 12, true);

    assert!(
        chat.transcript_selection_invalidated(),
        "a jump past the walk budget drops the selection instead of rendering the history"
    );
}

/// A range that stops mid-row is cut on the cells the row occupies, so a wide
/// grapheme is never split into half a character.
#[test]
fn a_transcript_endpoint_row_is_cut_on_the_cells_it_occupies() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.entries
        .push(ChatEntry::plain(1, ChatRole::Agent, "世界 wide row"));
    drawn_transcript(&mut chat, 40, 24);
    // The body row follows the entry's header; its gutter takes two columns
    // and each of the wide graphemes after it takes two more.
    let body = transcript_pane(&chat).top_row + 1;

    assert_eq!(
        chat.transcript_selection_text(&SelectionRange {
            start: ContentPos::new(body, 2),
            end: ContentPos::new(body, 5),
        }),
        Some("世界".into())
    );
    assert_eq!(
        chat.transcript_selection_text(&SelectionRange {
            start: ContentPos::new(body, 3),
            end: ContentPos::new(body, 8),
        }),
        Some("界 wi".into())
    );
}

fn scrollbar_chat() -> ChatState {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.entries = vec![ChatEntry::plain(
        1,
        ChatRole::Agent,
        (0..200).map(|i| format!("line {i}\n")).collect::<String>(),
    )];
    drawn_transcript(&mut chat, 60, 24);
    chat
}

fn scrollbar_mouse(chat: &mut ChatState, kind: MouseEventKind, column: u16, row: u16) {
    chat.handle_mouse(MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    });
}

#[test]
fn scrollbar_drag_reaches_both_ends_and_release_stops_capture() {
    use crossterm::event::MouseButton::Left;
    let mut chat = scrollbar_chat();
    let geometry = chat.transcript_scrollbar.pointer.geometry().unwrap();
    assert!(geometry.max_scroll > 0);
    scrollbar_mouse(
        &mut chat,
        MouseEventKind::Down(Left),
        geometry.thumb.x,
        geometry.thumb.y,
    );
    assert!(chat.transcript_scrollbar_dragging());
    scrollbar_mouse(&mut chat, MouseEventKind::Drag(Left), 0, 0);
    assert_eq!(chat.anchor, TranscriptAnchor::Row { entry: 0, row: 0 });
    drawn_transcript(&mut chat, 60, 24);
    scrollbar_mouse(&mut chat, MouseEventKind::Drag(Left), 0, u16::MAX);
    assert_eq!(chat.anchor, TranscriptAnchor::Bottom);
    scrollbar_mouse(&mut chat, MouseEventKind::Up(Left), 0, u16::MAX);
    assert!(!chat.transcript_scrollbar_dragging());
    scrollbar_mouse(&mut chat, MouseEventKind::Drag(Left), 0, 0);
    assert_eq!(chat.anchor, TranscriptAnchor::Bottom);
    chat.entries
        .push(ChatEntry::plain(2, ChatRole::Agent, "new output"));
    let rows = drawn_transcript(&mut chat, 60, 24);
    assert!(rows.iter().any(|row| row.contains("new output")));
}

#[test]
fn grabbing_scrollbar_thumb_does_not_jump_and_preserves_grab_offset() {
    use crossterm::event::MouseButton::Left;
    let mut chat = scrollbar_chat();
    chat.entries[0] = ChatEntry::plain(
        1,
        ChatRole::Agent,
        (0..40).map(|i| format!("line {i}\n")).collect::<String>(),
    );
    chat.invalidate_render_cache();
    chat.anchor = TranscriptAnchor::Bottom;
    drawn_transcript(&mut chat, 60, 24);
    let geometry = chat.transcript_scrollbar.pointer.geometry().unwrap();
    assert!(geometry.thumb.height > 1);
    let grab_row = geometry.thumb.bottom() - 1;
    scrollbar_mouse(
        &mut chat,
        MouseEventKind::Down(Left),
        geometry.track.x,
        grab_row,
    );
    assert_eq!(chat.anchor, TranscriptAnchor::Bottom);
    scrollbar_mouse(
        &mut chat,
        MouseEventKind::Drag(Left),
        geometry.track.x,
        grab_row - 1,
    );
    assert!(matches!(chat.anchor, TranscriptAnchor::Row { .. }));
    assert_eq!(
        chat.transcript_scrollbar.pointer.grab_offset(),
        geometry.thumb.height - 1
    );
}

#[test]
fn scrollbar_keeps_unseen_history_lazy_and_cancels_drag_on_resize() {
    use crossterm::event::MouseButton::Left;
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.entries = (0..1000)
        .map(|i| ChatEntry::plain(i, ChatRole::User, "message"))
        .collect();
    drawn_transcript(&mut chat, 60, 24);
    assert!(
        chat.render_cache
            .entries
            .iter()
            .filter(|entry| entry.is_some())
            .count()
            < 30
    );
    let geometry = chat.transcript_scrollbar.pointer.geometry().unwrap();
    scrollbar_mouse(
        &mut chat,
        MouseEventKind::Down(Left),
        geometry.thumb.x,
        geometry.thumb.y,
    );
    drawn_transcript(&mut chat, 40, 20);
    assert!(!chat.transcript_scrollbar_dragging());
}

#[test]
fn scrollbar_drag_keeps_its_mapping_when_history_renders_or_output_arrives() {
    use crossterm::event::MouseButton::Left;
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.entries = (0..100)
        .map(|i| ChatEntry::plain(i, ChatRole::User, "long message\n".repeat(20)))
        .collect();
    drawn_transcript(&mut chat, 60, 24);
    let geometry = chat.transcript_scrollbar.pointer.geometry().unwrap();
    scrollbar_mouse(
        &mut chat,
        MouseEventKind::Down(Left),
        geometry.thumb.x,
        geometry.thumb.y,
    );
    let estimates = chat.transcript_scrollbar.estimates.clone();
    let row = geometry.track.y + geometry.track.height / 2;
    scrollbar_mouse(&mut chat, MouseEventKind::Drag(Left), geometry.track.x, row);
    drawn_transcript(&mut chat, 60, 24);
    chat.entries
        .push(ChatEntry::plain(100, ChatRole::Agent, "new output"));
    drawn_transcript(&mut chat, 60, 24);
    assert!(chat.transcript_scrollbar_dragging());
    assert_eq!(chat.transcript_scrollbar.estimates, estimates);
    let anchor = chat.anchor;
    scrollbar_mouse(&mut chat, MouseEventKind::Drag(Left), geometry.track.x, row);
    drawn_transcript(&mut chat, 60, 24);
    assert_eq!(chat.anchor, anchor);
}

/// `symbols = "ascii"` is for a Linux console or a locale without UTF-8, so the
/// conversation has to reach it as well as the borders and status marks. The
/// role marks, the header's timestamp separator and the system rule were the
/// last Unicode a session with one exchange in it still drew.
#[test]
fn the_ascii_symbol_set_reaches_the_transcript_role_marks_and_timestamps() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    let timed = |seq: u64, role: ChatRole, text: &str| {
        let mut entry = ChatEntry::plain(seq, role, text);
        entry.recorded_at_ms = Some(75_600_000);
        entry
    };
    chat.entries = vec![
        timed(1, ChatRole::User, "ask"),
        timed(2, ChatRole::Agent, "answer"),
        ChatEntry::plain(3, ChatRole::Thought, "consider"),
        timed(4, ChatRole::System, "harness session started"),
        ChatEntry::tool(5, "Edit", None, ToolStatus::Completed),
        ChatEntry::plain(6, ChatRole::Plan, "step one"),
        ChatEntry::plain(7, ChatRole::PlanProposal, "step two"),
    ];
    chat.latest_seq = 7;

    let unicode = transcript_text(&mut chat, 60);
    assert!(
        unicode.iter().any(|line| line.starts_with("● Agent · ")),
        "the default set is unchanged: {unicode:#?}"
    );

    let mut ascii_chat = ChatState::new(&snapshot(), &[]);
    ascii_chat.entries = chat.entries.clone();
    ascii_chat.latest_seq = 7;
    let ascii = crate::theme::with_symbols(mj_core::config::SymbolSet::Ascii, || {
        transcript_text(&mut ascii_chat, 60)
    });
    for line in &ascii {
        assert!(line.is_ascii(), "{line:?} in {ascii:#?}");
    }
    for expected in ["> You - ", "* Agent - ", "o Thinking", "- Mjolnir - "] {
        assert!(
            ascii.iter().any(|line| line.starts_with(expected)),
            "{expected:?} in {ascii:#?}"
        );
    }
}

/// One item of R10's session D, as its store held it.
fn session_d_item(value: serde_json::Value) -> Arc<TranscriptItem> {
    Arc::new(serde_json::from_value(value).unwrap())
}

/// Session D's `sleep 60` tool call with `status`, last changed at
/// `last_changed_at_ms`.
fn session_d_sleep(status: &str, last_changed_at_ms: i64) -> Arc<TranscriptItem> {
    session_d_item(serde_json::json!({
        "stable_id": "tool:exec-7ee05740-cd4d-46f7-acda-c5154d550799",
        "position": 27,
        "created_at_ms": 1_790_343_723_040_i64,
        "last_changed_at_ms": last_changed_at_ms,
        "body": {"kind": "tool", "call": {
            "toolCallId": "exec-7ee05740-cd4d-46f7-acda-c5154d550799",
            "title": "sleep 60", "kind": "execute", "status": status,
            "rawInput": {"command": "sleep 60"}
        }}
    }))
}

/// R10-1, replayed in the order session D's journal recorded it. Esc ended
/// the turn (the "Interrupted" marker, ordinal 35) while the `sleep 60`
/// Codex had started kept running, because Codex owns that process; 48 s
/// later the command's completion arrived (ordinal 45) and the row turned
/// from "running" into "✓ Tool · done", as if the interrupted turn had
/// finished the work.
// Hard-won: 5ff5884d: a tool finishing after interruption was rendered as done.
#[test]
fn a_tool_that_ends_after_its_turn_was_interrupted_is_not_shown_as_done() {
    let prompt = session_d_item(serde_json::json!({
        "stable_id": "user:prompt-7b3aba1e44a67b21c81f588fb67efd01",
        "position": 9,
        "created_at_ms": 1_790_343_719_503_i64,
        "last_changed_at_ms": 1_790_343_719_503_i64,
        "body": {"kind": "user", "content": [
            {"type": "text", "text": "Run the shell command sleep 60 and then say done."}
        ]}
    }));
    let listing = session_d_item(serde_json::json!({
        "stable_id": "tool:exec-listing",
        "position": 20,
        "created_at_ms": 1_790_343_721_000_i64,
        "last_changed_at_ms": 1_790_343_721_500_i64,
        "body": {"kind": "tool", "call": {
            "toolCallId": "exec-listing", "title": "ls", "kind": "execute",
            "status": "completed", "rawInput": {"command": "ls"}
        }}
    }));
    let interrupted = session_d_item(serde_json::json!({
        "stable_id": "system:turn-interrupted:prompt-7b3aba1e44a67b21c81f588fb67efd01",
        "position": 35,
        "created_at_ms": 1_790_343_735_122_i64,
        "last_changed_at_ms": 1_790_343_735_122_i64,
        "body": {"kind": "system", "text": "Interrupted"}
    }));
    let tool_rows = |chat: &mut ChatState| {
        transcript_text(chat, 80)
            .into_iter()
            .filter(|line| line.contains("Tool"))
            .collect::<Vec<_>>()
    };

    // Ordinal 44: the turn has ended and the command is still running.
    let mut session = MaterializedSession::empty("session-d");
    session.applied_event_ordinal = 44;
    session.transcript = vec![
        prompt,
        listing,
        session_d_sleep("in_progress", 1_790_343_723_040),
        interrupted,
    ];
    let mut chat = ChatState::from_materialized(&session, &[], &[]);
    let rows = tool_rows(&mut chat);
    assert!(
        rows.iter().any(|row| row.contains("Tool · running")),
        "{rows:#?}"
    );

    // Ordinal 45: the command ends on its own.
    session.applied_event_ordinal = 45;
    session.transcript[2] = session_d_sleep("completed", 1_790_343_782_961);
    chat.apply_materialized(&session, &[], &[]);
    let rows = tool_rows(&mut chat);
    assert!(
        rows.iter()
            .any(|row| row.contains("Tool · ended after interrupt")),
        "{rows:#?}"
    );
    // The command that finished before the interruption is still done.
    assert_eq!(
        rows.iter()
            .filter(|row| row.contains("Tool · done"))
            .count(),
        1,
        "{rows:#?}"
    );
    // The web viewer's rows come from the same entries.
    let browser = TranscriptSnapshot::from_materialized(&session).browser_transcript(None);
    let labels = browser
        .entries
        .iter()
        .map(|entry| entry.label.as_str())
        .collect::<Vec<_>>();
    assert!(
        labels.contains(&"Tool · ended after interrupt") && labels.contains(&"Tool · done"),
        "{labels:#?}"
    );
}

fn click_text_action(chat: &mut ChatState, rows: &[String], text: &str) -> ChatAction {
    let (row, line, offset) = rows
        .iter()
        .enumerate()
        .find_map(|(row, line)| line.find(text).map(|offset| (row, line, offset)))
        .unwrap_or_else(|| panic!("missing {text:?} in {rows:#?}"));
    chat.handle_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: u16::try_from(display_width(&line[..offset])).unwrap(),
        row: u16::try_from(row).unwrap(),
        modifiers: KeyModifiers::NONE,
    })
}

// Hard-won: 743ae5d4: underlined transcript links dropped their destinations and did nothing on click.
#[test]
fn clicking_link_text_opens_that_links_destination() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.entries.push(ChatEntry::plain(
        1,
        ChatRole::Agent,
        "See [the report](https://example.com/one) and [the notes](https://example.com/two)."
            .to_owned(),
    ));
    let rows = drawn_transcript(&mut chat, 80, 12);

    assert_eq!(
        click_text_action(&mut chat, &rows, "notes"),
        ChatAction::OpenLink("https://example.com/two".to_owned())
    );
    assert_eq!(
        click_text_action(&mut chat, &rows, "report"),
        ChatAction::OpenLink("https://example.com/one".to_owned())
    );
    assert_eq!(click_text_action(&mut chat, &rows, "See"), ChatAction::None);
}

#[test]
fn links_with_the_same_text_open_their_own_destinations() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.entries.push(ChatEntry::plain(
        1,
        ChatRole::Agent,
        "First [issue](https://example.com/a)\n\nSecond [issue](https://example.com/b)".to_owned(),
    ));
    let rows = drawn_transcript(&mut chat, 80, 12);
    let second = rows
        .iter()
        .position(|row| row.contains("Second"))
        .expect("second paragraph is drawn");
    let line = &rows[second];
    let offset = line.find("issue").unwrap();
    let action = chat.handle_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: u16::try_from(display_width(&line[..offset])).unwrap(),
        row: u16::try_from(second).unwrap(),
        modifiers: KeyModifiers::NONE,
    });
    assert_eq!(
        action,
        ChatAction::OpenLink("https://example.com/b".to_owned())
    );
    assert_eq!(
        click_text_action(&mut chat, &rows, "issue"),
        ChatAction::OpenLink("https://example.com/a".to_owned())
    );
}

#[test]
fn clicking_a_non_web_link_copies_it_instead_of_opening_it() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.entries.push(ChatEntry::plain(
        1,
        ChatRole::Agent,
        "Run [this helper](file:///tmp/helper.sh) now.".to_owned(),
    ));
    let rows = drawn_transcript(&mut chat, 80, 12);

    assert_eq!(
        click_text_action(&mut chat, &rows, "helper"),
        ChatAction::CopyLink("file:///tmp/helper.sh".to_owned())
    );
}

// Hard-won: 743ae5d4: underlined transcript links did nothing on click, including wrapped spans.
#[test]
fn a_link_wrapped_across_rows_opens_from_its_continuation_row() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.entries.push(ChatEntry::plain(
        1,
        ChatRole::Agent,
        "Please read [the open upstream report about integer division](https://example.com/issue) today."
            .to_owned(),
    ));
    let rows = drawn_transcript(&mut chat, 28, 16);
    let division_row = rows
        .iter()
        .position(|row| row.contains("division"))
        .expect("link text is drawn");
    assert!(
        !rows[division_row].contains("Please"),
        "the link must wrap for this test: {rows:#?}"
    );
    assert_eq!(
        click_text_action(&mut chat, &rows, "division"),
        ChatAction::OpenLink("https://example.com/issue".to_owned())
    );
    assert_eq!(
        click_text_action(&mut chat, &rows, "today"),
        ChatAction::None
    );

    let mut standalone = ChatState::new(&snapshot(), &[]);
    standalone.entries.push(ChatEntry::plain(
        1,
        ChatRole::Agent,
        "[alpha bravo charlie delta echo foxtrot golf](https://example.com/only)".to_owned(),
    ));
    let standalone_rows = drawn_transcript(&mut standalone, 20, 16);
    let expected = ChatAction::OpenLink("https://example.com/only".to_owned());

    for word in ["alpha", "echo", "golf"] {
        assert_eq!(
            click_text_action(&mut standalone, &standalone_rows, word),
            expected,
            "{standalone_rows:#?}"
        );
    }
}

fn agent_rows(text: &str, width: u16) -> (ChatState, Vec<String>) {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.entries
        .push(ChatEntry::plain(1, ChatRole::Agent, text.to_owned()));
    let rows = drawn_transcript(&mut chat, width, 24);
    (chat, rows)
}

fn opens(url: &str) -> ChatAction {
    ChatAction::OpenLink(url.to_owned())
}

#[test]
fn a_plain_url_opens_without_its_trailing_punctuation() {
    let (mut chat, rows) = agent_rows(
        "The report is at https://example.com/some_path/report. Also (see https://example.com/x).",
        100,
    );

    assert_eq!(
        click_text_action(&mut chat, &rows, "some_path"),
        opens("https://example.com/some_path/report")
    );
    assert_eq!(
        click_text_action(&mut chat, &rows, "example.com/x"),
        opens("https://example.com/x")
    );
    assert_eq!(
        click_text_action(&mut chat, &rows, "report is"),
        ChatAction::None
    );
}

#[test]
fn a_url_inside_a_link_label_opens_the_links_destination() {
    let (mut chat, rows) = agent_rows("[https://example.com/label](https://example.com/dest)", 80);

    assert_eq!(
        click_text_action(&mut chat, &rows, "label"),
        opens("https://example.com/dest")
    );
}

fn golden_chat_buffer(chat: &mut ChatState, width: u16, height: u16) -> ratatui::buffer::Buffer {
    use ratatui::{Terminal, backend::TestBackend};

    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
    terminal
        .draw(|frame| crate::chat::active::render_full_frame(frame, chat, false))
        .expect("draw chat surface");
    terminal.backend().buffer().clone()
}

fn append_transcript_golden_state(
    output: &mut String,
    label: &str,
    width: u16,
    height: u16,
    rows: &[String],
    details: &[String],
) {
    use std::fmt::Write as _;

    if !output.is_empty() {
        output.push('\n');
    }
    writeln!(output, "=== {label} ({width}x{height}) ===").expect("write state header");
    output.push_str(&rows.join("\n"));
    output.push('\n');
    for detail in details {
        writeln!(output, "{detail}").expect("write state detail");
    }
}

#[test]
fn golden_transcript_navigation() {
    let mut output = String::new();

    let mut chat = numbered_chat(40);
    let rows = crate::golden::buffer_lines(&golden_chat_buffer(&mut chat, 40, 24));
    append_transcript_golden_state(&mut output, "tail before wheel", 40, 24, &rows, &[]);
    chat.handle_mouse(wheel(MouseEventKind::ScrollUp));
    let rows = crate::golden::buffer_lines(&golden_chat_buffer(&mut chat, 40, 24));
    append_transcript_golden_state(&mut output, "wheel up", 40, 24, &rows, &[]);
    chat.handle_mouse(wheel(MouseEventKind::ScrollDown));
    let rows = crate::golden::buffer_lines(&golden_chat_buffer(&mut chat, 40, 24));
    append_transcript_golden_state(&mut output, "wheel down follows", 40, 24, &rows, &[]);

    let mut chat = numbered_chat(40);
    let rows = crate::golden::buffer_lines(&golden_chat_buffer(&mut chat, 60, 24));
    append_transcript_golden_state(&mut output, "page navigation tail", 60, 24, &rows, &[]);
    chat.handle_key(key(KeyCode::PageUp));
    let rows = crate::golden::buffer_lines(&golden_chat_buffer(&mut chat, 60, 24));
    append_transcript_golden_state(&mut output, "page up", 60, 24, &rows, &[]);
    chat.handle_key(key(KeyCode::PageDown));
    let rows = crate::golden::buffer_lines(&golden_chat_buffer(&mut chat, 60, 24));
    append_transcript_golden_state(&mut output, "page down follows", 60, 24, &rows, &[]);
    chat.handle_key(key(KeyCode::PageUp));
    chat.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL));
    let rows = crate::golden::buffer_lines(&golden_chat_buffer(&mut chat, 60, 24));
    append_transcript_golden_state(&mut output, "ctrl end follows", 60, 24, &rows, &[]);

    let mut chat = numbered_chat(200);
    let _ = golden_chat_buffer(&mut chat, 40, 24);
    chat.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::CONTROL));
    let rows = crate::golden::buffer_lines(&golden_chat_buffer(&mut chat, 40, 24));
    append_transcript_golden_state(&mut output, "ctrl home", 40, 24, &rows, &[]);
    chat.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL));
    let rows = crate::golden::buffer_lines(&golden_chat_buffer(&mut chat, 40, 24));
    append_transcript_golden_state(&mut output, "ctrl end", 40, 24, &rows, &[]);

    let mut chat = numbered_chat(200);
    chat.set_input("draft prompt".into());
    let _ = golden_chat_buffer(&mut chat, 40, 24);
    chat.handle_key(keypad_key(KeyCode::Home));
    let rows = crate::golden::buffer_lines(&golden_chat_buffer(&mut chat, 40, 24));
    append_transcript_golden_state(
        &mut output,
        "keypad home leaves draft cursor",
        40,
        24,
        &rows,
        &[format!("input cursor: {}", chat.input_cursor)],
    );
    chat.handle_key(keypad_key(KeyCode::End));
    let rows = crate::golden::buffer_lines(&golden_chat_buffer(&mut chat, 40, 24));
    append_transcript_golden_state(
        &mut output,
        "keypad end leaves draft cursor",
        40,
        24,
        &rows,
        &[format!("input cursor: {}", chat.input_cursor)],
    );
    chat.handle_key(key(KeyCode::Home));
    let rows = crate::golden::buffer_lines(&golden_chat_buffer(&mut chat, 40, 24));
    append_transcript_golden_state(
        &mut output,
        "plain home edits draft",
        40,
        24,
        &rows,
        &[format!("input cursor: {}", chat.input_cursor)],
    );
    chat.handle_key(key(KeyCode::End));
    let rows = crate::golden::buffer_lines(&golden_chat_buffer(&mut chat, 40, 24));
    append_transcript_golden_state(
        &mut output,
        "plain end edits draft",
        40,
        24,
        &rows,
        &[format!("input cursor: {}", chat.input_cursor)],
    );

    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.entries.push(ChatEntry::plain(
        1,
        ChatRole::Agent,
        "response advertised on the dashboard",
    ));
    for index in 0..8 {
        chat.entries.push(ChatEntry::plain(
            index + 2,
            ChatRole::System,
            format!("terminal failure {index}\n{}", "output\n".repeat(12)),
        ));
    }
    let rows = crate::golden::buffer_lines(&golden_chat_buffer(&mut chat, 60, 24));
    append_transcript_golden_state(&mut output, "opening reveal", 60, 24, &rows, &[]);
    chat.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL));
    let rows = crate::golden::buffer_lines(&golden_chat_buffer(&mut chat, 60, 24));
    append_transcript_golden_state(&mut output, "revealed transcript tail", 60, 24, &rows, &[]);

    let saved = chat.transcript_position();
    let mut reopened = ChatState::new(&snapshot(), &[]);
    reopened.entries = chat.entries.clone();
    reopened.restore_transcript_position(saved);
    let rows = crate::golden::buffer_lines(&golden_chat_buffer(&mut reopened, 60, 24));
    append_transcript_golden_state(&mut output, "reopened transcript tail", 60, 24, &rows, &[]);

    let mut chat = numbered_chat(2);
    let before = crate::golden::buffer_lines(&golden_chat_buffer(&mut chat, 40, 24));
    chat.handle_mouse(wheel(MouseEventKind::ScrollUp));
    chat.handle_key(key(KeyCode::PageUp));
    let rows = crate::golden::buffer_lines(&golden_chat_buffer(&mut chat, 40, 24));
    append_transcript_golden_state(
        &mut output,
        "short transcript ignores scrolling",
        40,
        24,
        &rows,
        &[format!("viewport unchanged: {}", before == rows)],
    );

    let mut chat = scrollbar_chat();
    let geometry = chat.transcript_scrollbar.pointer.geometry().unwrap();
    let rows = crate::golden::buffer_lines(&golden_chat_buffer(&mut chat, 60, 24));
    let surface = chat
        .frame_surfaces()
        .surface_at(geometry.track.x, geometry.track.y);
    append_transcript_golden_state(
        &mut output,
        "scrollbar before track click",
        60,
        24,
        &rows,
        &[format!("track content surface: {surface:?}")],
    );
    scrollbar_mouse(
        &mut chat,
        MouseEventKind::Down(MouseButton::Left),
        geometry.track.x,
        geometry.track.y + geometry.track.height / 2,
    );
    let rows = crate::golden::buffer_lines(&golden_chat_buffer(&mut chat, 60, 24));
    append_transcript_golden_state(
        &mut output,
        "scrollbar track seeks",
        60,
        24,
        &rows,
        &[format!("anchor: {:?}", chat.anchor)],
    );
    let before_release = chat.anchor;
    scrollbar_mouse(
        &mut chat,
        MouseEventKind::Up(MouseButton::Left),
        geometry.track.x,
        geometry.track.y,
    );
    let rows = crate::golden::buffer_lines(&golden_chat_buffer(&mut chat, 60, 24));
    append_transcript_golden_state(
        &mut output,
        "scrollbar release preserves seek",
        60,
        24,
        &rows,
        &[format!(
            "anchor unchanged: {}",
            chat.anchor == before_release
        )],
    );

    let mut chat = ChatState::new(&snapshot(), &[]);
    for (width, height) in [(60, 24), (4, 24), (3, 24), (2, 24), (1, 1), (0, 0)] {
        let _ = golden_chat_buffer(&mut chat, width, height);
        scrollbar_mouse(
            &mut chat,
            MouseEventKind::Down(MouseButton::Left),
            width.saturating_sub(1),
            0,
        );
        let rows = crate::golden::buffer_lines(&golden_chat_buffer(&mut chat, width, height));
        append_transcript_golden_state(
            &mut output,
            "empty transcript drag",
            width,
            height,
            &rows,
            &[format!(
                "scrollbar dragging: {}",
                chat.transcript_scrollbar_dragging()
            )],
        );
    }

    mj_core::golden::assert_platform_golden(
        env!("CARGO_MANIFEST_DIR"),
        "transcript-navigation",
        &output,
    );
}

#[test]
fn golden_transcript_link_routing() {
    let mut output = String::new();
    let code =
        "Run `curl https://example.com/inline` or:\n\n```\nGET https://example.com/block\n```";
    let (mut chat, rows) = agent_rows(code, 80);
    for (label, text) in [
        ("inline code URL", "example.com/inline"),
        ("fenced code URL", "example.com/block"),
        ("ordinary code text", "GET"),
    ] {
        let action = click_text_action(&mut chat, &rows, text);
        append_transcript_golden_state(
            &mut output,
            label,
            80,
            24,
            &rows,
            &[format!("action: {action:?}")],
        );
    }

    let table = "| Issue | Status |\n| --- | --- |\n| [first bug](https://example.com/1) | open |\n| see https://example.com/2 | closed |";
    let (mut chat, rows) = agent_rows(table, 80);
    for (label, text) in [
        ("wide table link", "first bug"),
        ("wide table URL", "example.com/2"),
        ("wide table plain cell", "closed"),
    ] {
        let action = click_text_action(&mut chat, &rows, text);
        append_transcript_golden_state(
            &mut output,
            label,
            80,
            24,
            &rows,
            &[format!("action: {action:?}")],
        );
    }

    let narrow_table = "| Issue | Notes |\n| --- | --- |\n| [first bug](https://example.com/1) | a long description that cannot fit a narrow grid |";
    let (mut chat, rows) = agent_rows(narrow_table, 30);
    for (label, text) in [
        ("narrow table link", "first bug"),
        ("narrow table label", "Issue"),
    ] {
        let action = click_text_action(&mut chat, &rows, text);
        append_transcript_golden_state(
            &mut output,
            label,
            30,
            24,
            &rows,
            &[format!("action: {action:?}")],
        );
    }

    mj_core::golden::assert_platform_golden(
        env!("CARGO_MANIFEST_DIR"),
        "transcript-link-routing",
        &output,
    );
}

#[test]
fn golden_conversation_title() {
    use mj_client::review::RuntimeReviewView;
    use mj_core::review::driver::TurnReviewPhase;
    use mj_core::review::verdict::ReviewVerdict;
    use ratatui::{Terminal, backend::TestBackend};

    let mut output = String::new();
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.set_header_summary("podman", "codex3", "Review the build");
    let mut draw = |label: &str, chat: &mut ChatState| {
        let mut terminal = Terminal::new(TestBackend::new(80, 10)).expect("terminal");
        terminal
            .draw(|frame| {
                render_transcript(frame, frame.area(), chat, false, 0, 0, false);
            })
            .expect("draw conversation header");
        let buffer = terminal.backend().buffer();
        let header = (0..80).map(|x| buffer[(x, 0)].symbol()).collect::<String>();
        let title_start = header.find("Review the build").unwrap();
        let details = [format!(
            "foreground: host={:?}, session title={:?}",
            buffer[(2, 0)].fg,
            buffer[(title_start as u16, 0)].fg
        )];
        append_transcript_golden_state(
            &mut output,
            label,
            80,
            10,
            &crate::golden::buffer_lines(buffer),
            &details,
        );
    };

    draw("idle", &mut chat);
    let mut view = RuntimeReviewView {
        session_id: "session".to_owned(),
        questions: Vec::new(),
        phase: TurnReviewPhase::LaunchingReviewer,
        roles: Vec::new(),
        status: "starting the reviewer".to_owned(),
        verdict: None,
    };
    chat.set_turn_review(Some(view.clone()));
    draw("reviewing", &mut chat);
    view.phase = TurnReviewPhase::Verdict(ReviewVerdict::Findings {
        synthesis: "[P2] src/lib.rs:1 -- weak test".to_owned(),
        evidence: Default::default(),
    });
    chat.set_turn_review(Some(view));
    draw("findings", &mut chat);
    chat.set_turn_review(None);
    draw("idle restored", &mut chat);

    mj_core::golden::assert_golden(env!("CARGO_MANIFEST_DIR"), "conversation-title", &output);
}

// Hard-won: d00682c9: expanded send_message details showed only the receipt, omitting its arguments.
#[test]
fn golden_rich_transcript_tool_presentation() {
    let mut output = String::new();

    let mut chat = ChatState::new(&snapshot(), &[]);
    let mut completed = completed_tool(1, "completed command");
    completed.tool_content = vec!["completed detail".into()];
    let mut pending = ChatEntry::tool(2, "pending command", None, ToolStatus::Pending);
    pending.tool_content = vec!["pending detail".into()];
    let mut running = ChatEntry::tool(3, "running command", None, ToolStatus::Running);
    running.tool_content = vec!["running detail".into()];
    let mut failed = ChatEntry::tool(4, "failed command", None, ToolStatus::Failed);
    failed.tool_content = vec!["failed detail".into()];
    chat.entries.extend([
        completed.clone(),
        pending.clone(),
        running.clone(),
        failed.clone(),
    ]);
    let buffer = golden_chat_buffer(&mut chat, 80, 24);
    let style_details = [&completed, &pending, &running, &failed]
        .into_iter()
        .map(|entry| {
            let visual = entry_visual(entry);
            format!(
                "{} style: header={:?}, body={:?}",
                entry.text, visual.header_style.fg, visual.body_style.fg
            )
        })
        .collect::<Vec<_>>();
    append_transcript_golden_state(
        &mut output,
        "tool status labels and emphasis",
        80,
        24,
        &crate::golden::buffer_lines(&buffer),
        &style_details,
    );

    let mut chat = ChatState::new(&snapshot(), &[]);
    let mut running = ChatEntry::tool(1, "running command", None, ToolStatus::Running);
    running.tool_content = vec!["running command --with every argument".into()];
    chat.entries.extend([
        running,
        completed_tool(2, "completed command"),
        ChatEntry::tool(3, "failed command", None, ToolStatus::Failed),
    ]);
    let _ = golden_chat_buffer(&mut chat, 80, 24);
    let target = chat
        .transcript_tool_click_targets
        .iter()
        .find(|target| target.start_seq == 2)
        .copied()
        .expect("completed tool is clickable");
    let running_target = chat
        .transcript_tool_click_targets
        .iter()
        .find(|target| target.start_seq == 1)
        .copied()
        .expect("running tool is clickable");
    let targets = chat
        .transcript_tool_click_targets
        .iter()
        .map(|target| target.start_seq)
        .collect::<BTreeSet<_>>();
    assert_eq!(targets, BTreeSet::from([1, 2, 3]));
    append_transcript_golden_state(
        &mut output,
        "every tool has a click target",
        80,
        24,
        &crate::golden::buffer_lines(&golden_chat_buffer(&mut chat, 80, 24)),
        &[format!("tool click targets: {targets:?}")],
    );
    chat.handle_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: target.rect.x,
        row: target.rect.y,
        modifiers: KeyModifiers::NONE,
    });
    append_transcript_golden_state(
        &mut output,
        "completed command expanded",
        80,
        24,
        &crate::golden::buffer_lines(&golden_chat_buffer(&mut chat, 80, 24)),
        &[format!("expanded tools: {:?}", chat.expanded_tool_calls)],
    );
    chat.handle_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: running_target.rect.x,
        row: running_target.rect.y,
        modifiers: KeyModifiers::NONE,
    });
    append_transcript_golden_state(
        &mut output,
        "running command expanded",
        80,
        24,
        &crate::golden::buffer_lines(&golden_chat_buffer(&mut chat, 80, 24)),
        &[format!("expanded tools: {:?}", chat.expanded_tool_calls)],
    );

    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.entries.extend([
        thought(1, "thinking about tools"),
        completed_tool(2, "first-command"),
        completed_tool(3, "second-command"),
    ]);
    let rows = crate::golden::buffer_lines(&golden_chat_buffer(&mut chat, 60, 24));
    click_rendered_text(&mut chat, &rows, "thinking about tools");
    let rows = crate::golden::buffer_lines(&golden_chat_buffer(&mut chat, 60, 24));
    append_transcript_golden_state(
        &mut output,
        "thought text does not expand tools",
        60,
        24,
        &rows,
        &[format!("expanded tools: {:?}", chat.expanded_tool_calls)],
    );

    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.apply_session_update(
        1,
        &serde_json::json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "shell-1",
            "title": "Bash",
            "kind": "execute",
            "status": "pending",
            "rawInput": {"command": "git add src && cargo test --workspace | cat"}
        }),
    );
    for (label, seq, status) in [
        ("execute pending", 1, None),
        ("execute running", 2, Some("in_progress")),
        ("execute completed", 3, Some("completed")),
    ] {
        if let Some(status) = status {
            chat.apply_session_update(
                seq,
                &serde_json::json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": "shell-1",
                    "status": status
                }),
            );
        }
        append_transcript_golden_state(
            &mut output,
            label,
            100,
            24,
            &crate::golden::buffer_lines(&golden_chat_buffer(&mut chat, 100, 24)),
            &[],
        );
    }
    chat.render_mode = TranscriptRenderMode::Raw;
    append_transcript_golden_state(
        &mut output,
        "raw keeps provider title",
        100,
        24,
        &crate::golden::buffer_lines(&golden_chat_buffer(&mut chat, 100, 24)),
        &[],
    );

    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.apply_session_update(
        1,
        &serde_json::json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "message-1",
            "title": "mcp__mj-agents__send_message",
            "status": "pending"
        }),
    );
    chat.apply_session_update(
        2,
        &serde_json::json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "message-1",
            "status": "in_progress",
            "rawInput": {
                "session_id": "peer-session",
                "message": "Answer on the 48 s reverse queries: yes, that is the expected shape of today's target-scoped reverse design on Vector, not a regression, and two levers exist, one cheap.\nTry a larger reader page cache."
            }
        }),
    );
    let rows = crate::golden::buffer_lines(&golden_chat_buffer(&mut chat, 80, 28));
    click_rendered_text(&mut chat, &rows, "mcp__mj-agents__send_message");
    append_transcript_golden_state(
        &mut output,
        "message input arrives while running",
        80,
        28,
        &crate::golden::buffer_lines(&golden_chat_buffer(&mut chat, 80, 28)),
        &[],
    );
    chat.apply_session_update(
        3,
        &serde_json::json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "message-1",
            "status": "completed",
            "content": [{"type": "content", "content": {
                "type": "text", "text": "{\"status\":\"queued\",\"via\":\"mailbox\"}"
            }}]
        }),
    );
    append_transcript_golden_state(
        &mut output,
        "message input survives result update",
        80,
        28,
        &crate::golden::buffer_lines(&golden_chat_buffer(&mut chat, 80, 28)),
        &[],
    );
    chat.render_mode = TranscriptRenderMode::Raw;
    append_transcript_golden_state(
        &mut output,
        "raw message includes arguments and receipt",
        80,
        28,
        &crate::golden::buffer_lines(&golden_chat_buffer(&mut chat, 80, 28)),
        &[],
    );
    let session = mj_transcript::projection::materialized_session_from_entries(
        "message-session",
        &chat.entries,
        3,
        mj_core::relay::WorkerPhase::Idle,
        Default::default(),
        Vec::new(),
        Vec::new(),
    );
    let mut restored = ChatState::from_materialized(&session, &[], &[]);
    let rows = crate::golden::buffer_lines(&golden_chat_buffer(&mut restored, 80, 28));
    click_rendered_text(&mut restored, &rows, "mcp__mj-agents__send_message");
    append_transcript_golden_state(
        &mut output,
        "restored message includes original arguments",
        80,
        28,
        &crate::golden::buffer_lines(&golden_chat_buffer(&mut restored, 80, 28)),
        &[],
    );
    chat.render_mode = TranscriptRenderMode::Rich;
    chat.apply_session_update(
        4,
        &serde_json::json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "message-1",
            "rawInput": {}
        }),
    );
    append_transcript_golden_state(
        &mut output,
        "empty input replaces earlier message arguments",
        80,
        28,
        &crate::golden::buffer_lines(&golden_chat_buffer(&mut chat, 80, 28)),
        &[],
    );

    let session = fallback_terminal_session(terminal_record(Some(0), None));
    let mut chat = ChatState::from_materialized(&session, &[], &[]);
    append_transcript_golden_state(
        &mut output,
        "clean fallback terminal in rich mode",
        80,
        24,
        &crate::golden::buffer_lines(&golden_chat_buffer(&mut chat, 80, 24)),
        &[],
    );
    chat.render_mode = TranscriptRenderMode::Raw;
    append_transcript_golden_state(
        &mut output,
        "clean fallback terminal in raw mode",
        80,
        24,
        &crate::golden::buffer_lines(&golden_chat_buffer(&mut chat, 80, 24)),
        &[],
    );

    let terminal = mj_core::relay::ActiveAgentTerminal {
        terminal_id: "term-1".into(),
        command: "cargo mutants --in-diff diff".into(),
        started_at_ms: i64::MAX,
    };
    let mut chat = ChatState::new(&snapshot(), &[]);
    let session = MaterializedSession::empty("session-live-terminal");
    chat.set_active_agent_terminals(std::slice::from_ref(&terminal), &session);
    append_transcript_golden_state(
        &mut output,
        "unclaimed live terminal",
        80,
        24,
        &crate::golden::buffer_lines(&golden_chat_buffer(&mut chat, 80, 24)),
        &[],
    );
    chat.set_active_agent_terminals(&[], &session);
    append_transcript_golden_state(
        &mut output,
        "live terminal removed after exit",
        80,
        24,
        &crate::golden::buffer_lines(&golden_chat_buffer(&mut chat, 80, 24)),
        &[],
    );

    let mut chat = ChatState::new(&snapshot(), &[]);
    for (start, name) in [(1, "older"), (5, "newer")] {
        let mut tool = completed_tool(start + 1, &format!("{name} provider title"));
        tool.tool_summary = Some(format!("{name}-command"));
        tool.tool_content = vec![format!("{name} tool details")];
        chat.entries.extend([
            thought(
                start,
                &format!("{name} thought with enough words to wrap across several rows"),
            ),
            tool,
            completed_tool(start + 2, &format!("{name}-companion")),
            ChatEntry::plain(start + 3, ChatRole::Agent, format!("{name} response")),
        ]);
    }
    for name in ["older", "newer"] {
        let rows = crate::golden::buffer_lines(&golden_chat_buffer(&mut chat, 36, 48));
        append_transcript_golden_state(
            &mut output,
            &format!("{name} tool groups collapsed"),
            36,
            48,
            &rows,
            &[],
        );
        click_rendered_text(&mut chat, &rows, &format!("{name}-command"));
        let rows = crate::golden::buffer_lines(&golden_chat_buffer(&mut chat, 36, 48));
        append_transcript_golden_state(
            &mut output,
            &format!("{name} group expanded"),
            36,
            48,
            &rows,
            &[format!("expanded tools: {:?}", chat.expanded_tool_calls)],
        );
        click_rendered_text(&mut chat, &rows, &format!("{name} provider title"));
        let rows = crate::golden::buffer_lines(&golden_chat_buffer(&mut chat, 36, 48));
        append_transcript_golden_state(
            &mut output,
            &format!("{name} group collapsed"),
            36,
            48,
            &rows,
            &[format!("expanded tools: {:?}", chat.expanded_tool_calls)],
        );
    }

    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.entries.extend([
        completed_tool(1, "first command with a long argument"),
        completed_tool(2, "second command with a long argument"),
        completed_tool(3, "third command with a long argument"),
    ]);
    let _ = golden_chat_buffer(&mut chat, 30, 24);
    let second_targets = chat
        .transcript_tool_click_targets
        .iter()
        .filter(|target| target.start_seq == 2)
        .copied()
        .collect::<Vec<_>>();
    let target = second_targets[1];
    chat.handle_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: target.rect.x,
        row: target.rect.y,
        modifiers: KeyModifiers::NONE,
    });
    append_transcript_golden_state(
        &mut output,
        "wrapped summary member expands from continuation row",
        30,
        24,
        &crate::golden::buffer_lines(&golden_chat_buffer(&mut chat, 30, 24)),
        &[
            format!("second member hitboxes: {}", second_targets.len()),
            format!("expanded tools: {:?}", chat.expanded_tool_calls),
        ],
    );

    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.apply_session_update(
        1,
        &serde_json::json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "read-config",
            "title": "read config",
            "status": "pending"
        }),
    );
    chat.apply_session_update(
        2,
        &serde_json::json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "read-config",
            "status": "completed"
        }),
    );
    append_transcript_golden_state(
        &mut output,
        "tool call status update",
        80,
        24,
        &crate::golden::buffer_lines(&golden_chat_buffer(&mut chat, 80, 24)),
        &[],
    );

    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.entries
        .push(ChatEntry::plain(1, ChatRole::Agent, "**bold**"));
    chat.render_mode = TranscriptRenderMode::Raw;
    append_transcript_golden_state(
        &mut output,
        "raw markdown markers",
        30,
        24,
        &crate::golden::buffer_lines(&golden_chat_buffer(&mut chat, 30, 24)),
        &[],
    );

    // Settings › Advanced › Tool calls: Inline. Every call keeps its own
    // row with its full command, thoughts are not folded, and output shows
    // its head and tail around a count of the lines left out.
    let kind_tool = |seq: u64, title: &str, kind: ToolKind, status: ToolStatus| {
        let call = ToolCall::new(format!("call-{seq}"), title).kind(kind);
        let presentation = tool_call_presentation(&call);
        let mut entry = ChatEntry::tool(seq, title, None, status);
        entry.tool_summary = Some(presentation.summary.clone());
        entry.tool_presentation = Some(presentation);
        entry
    };
    let mut tests = kind_tool(
        2,
        "cargo test -p brokk-mj-chat --lib",
        ToolKind::Execute,
        ToolStatus::Completed,
    );
    tests.tool_content = vec![format!(
        "running 12 tests\n{}\ntest result: ok. 12 passed\nexited 0",
        (1..=10)
            .map(|test| format!("test case_{test} ... ok"))
            .collect::<Vec<_>>()
            .join("\n")
    )];
    let mut read = kind_tool(4, "Read src/lib.rs", ToolKind::Read, ToolStatus::Completed);
    read.tool_content = vec!["fn main() {}".into()];
    let mut edit = kind_tool(5, "Edit src/lib.rs", ToolKind::Edit, ToolStatus::Completed);
    edit.tool_diffstats = vec!["src/lib.rs +3 -1".into()];
    let mut failed = kind_tool(6, "cat missing.txt", ToolKind::Execute, ToolStatus::Failed);
    failed.tool_content = vec![format!(
        "cat: missing.txt: No such file or directory {}\nexited 1",
        "x".repeat(80)
    )];
    let mut running = kind_tool(
        7,
        "sleep 30 && echo done",
        ToolKind::Execute,
        ToolStatus::Running,
    );
    running.tool_content = vec!["waiting".into()];
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.set_tool_display(ToolDisplay {
        layout: mj_core::config::ToolOutput::Inline,
        output_lines: 5,
    });
    chat.entries.extend([
        thought(1, "first thought"),
        tests,
        thought(3, "second thought"),
        read,
        edit,
        failed,
        running,
    ]);
    let rows = crate::golden::buffer_lines(&golden_chat_buffer(&mut chat, 80, 36));
    append_transcript_golden_state(&mut output, "inline tool calls", 80, 36, &rows, &[]);
    click_rendered_text(&mut chat, &rows, "cargo test -p brokk-mj-chat");
    append_transcript_golden_state(
        &mut output,
        "inline call expanded",
        80,
        36,
        &crate::golden::buffer_lines(&golden_chat_buffer(&mut chat, 80, 36)),
        &[format!("expanded tools: {:?}", chat.expanded_tool_calls)],
    );
    chat.expanded_tool_calls.clear();
    chat.set_tool_display(ToolDisplay {
        layout: mj_core::config::ToolOutput::Inline,
        output_lines: 1,
    });
    let rows = crate::theme::with_symbols(crate::theme::SymbolSet::Ascii, || {
        crate::golden::buffer_lines(&golden_chat_buffer(&mut chat, 80, 36))
    });
    append_transcript_golden_state(
        &mut output,
        "inline with one output line and ASCII symbols",
        80,
        36,
        &rows,
        &[],
    );

    mj_core::golden::assert_platform_golden(
        env!("CARGO_MANIFEST_DIR"),
        "rich-transcript-tool-presentation",
        &output,
    );
}

/// Wrapping is only how a row fits the pane, so copying a wrapped message
/// gives back its source lines: no gutter, no wrap indent, and no newline
/// where a URL or a sentence was split across rows.
#[test]
fn copying_wrapped_rows_rejoins_the_lines_wrapping_split() {
    let url = "https://github.com/organizations/BrokkAi/settings/apps/mergecopbot/permissions";
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.entries.push(ChatEntry::plain(
        1,
        ChatRole::Agent,
        format!("1. Open {url}. You can also get there through BrokkAi settings.\n\nThen save."),
    ));
    drawn_transcript(&mut chat, 40, 24);
    let width = chat.render_cache.width;
    let rows = render_transcript_entry(
        &chat.entries[0],
        usize::from(width),
        TranscriptRenderMode::Rich,
    );
    assert!(rows.len() > 5, "the fixture must wrap");
    // Skip the header row, and stop before the entry's trailing blank row.
    let body = transcript_pane(&chat).top_row + 1;
    let last = body + rows.len() - 3;

    assert_eq!(
        chat.transcript_selection_text(&SelectionRange {
            start: ContentPos::new(body, 0),
            end: ContentPos::new(last, width - 1),
        }),
        Some(format!(
            "1. Open {url}. You can also get there through BrokkAi settings.\n\nThen save."
        ))
    );
}
