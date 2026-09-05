use super::*;
use crate::hel_chat::test_support::{
    agent_message_item, agent_transcript_item, drawn_transcript, drawn_transcript_selecting, key,
    line_text, mouse_in, queued, snapshot, transcript_text,
};
use crate::hel_selection::SelectionState;
use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MouseEvent, MouseEventKind,
};
use hel::hel_acp::RuntimeEvent;
use hel::hel_worker::{SequencedEvent, WorkerEvent};

fn completed_tool(seq: u64, title: &str) -> ChatEntry {
    ChatEntry::tool(seq, title, None, ToolStatus::Completed)
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

/// A prompt Hel generated for a review is stored as a User item, because that
/// is what the harness received, but it must never render as the user's.
#[test]
fn a_tool_entry_carries_its_state_and_its_changed_files_as_data() {
    // The terminal formats a diffstat for a terminal. A browser re-parsing
    // that formatting is how every diffstat came to render as one unsplit
    // path, so the projection carries the numbers instead.
    let mut entry = ChatEntry::plain(1, ChatRole::Tool, "edit src/main.rs".to_owned());
    entry.tool_status = Some(ToolStatus::Failed);
    entry.tool_diffstats = vec![
        format_diffstat_for_test("src/main.rs", 12, 3),
        "not a diffstat".to_owned(),
    ];
    let browser = browser_entry(&entry);

    assert_eq!(browser.tool_status, Some("failed"));
    assert_eq!(browser.tone, "failed");
    assert_eq!(browser.glyph, "\u{d7}");
    assert_eq!(browser.diffstats.len(), 1, "an unparseable line was kept");
    assert_eq!(browser.diffstats[0].path, "src/main.rs");
    assert_eq!(browser.diffstats[0].insertions, 12);
    assert_eq!(browser.diffstats[0].deletions, 3);
}

/// Build a diffstat the way `format_diffstat` does, including the Unicode
/// MINUS SIGN, so this check fails if that format ever changes.
fn format_diffstat_for_test(path: &str, insertions: u32, deletions: u32) -> String {
    format!("{path}  +{insertions} \u{2212}{deletions}")
}

#[test]
fn every_role_publishes_the_glyph_and_tone_the_terminal_draws() {
    for (role, glyph, tone) in [
        (ChatRole::User, "\u{276f}", "user"),
        (ChatRole::Agent, "\u{25cf}", "agent"),
        (ChatRole::Thought, "\u{25cb}", "thinking"),
        (ChatRole::Plan, "\u{25c7}", "plan"),
        (ChatRole::PlanProposal, "\u{25c8}", "plan-proposal"),
        (ChatRole::System, "\u{2500}", "system"),
    ] {
        let entry = ChatEntry::plain(1, role, "text".to_owned());
        let browser = browser_entry(&entry);
        assert_eq!(browser.glyph, glyph, "{role:?}");
        assert_eq!(browser.tone, tone, "{role:?}");
        assert!(
            browser.tool_status.is_none(),
            "{role:?} carries a tool state"
        );
    }
}

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
                "text": hel::hel_second_opinion::PRIMARY_CONTEXT_REQUEST,
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
fn captured_mouse_wheel_scrolls_history_and_returns_to_following() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.entries.extend(
        (0..40)
            .map(|index| ChatEntry::plain(index + 1, ChatRole::User, format!("message {index}"))),
    );
    let tail = drawn_transcript(&mut chat, 40, 24);
    assert!(tail.iter().any(|line| line.contains("message 39")));

    chat.handle_mouse(wheel(MouseEventKind::ScrollUp));
    let scrolled = drawn_transcript(&mut chat, 40, 24);
    assert!(!scrolled.iter().any(|line| line.contains("message 39")));

    chat.handle_mouse(wheel(MouseEventKind::ScrollDown));
    let followed = drawn_transcript(&mut chat, 40, 24);
    assert!(followed.iter().any(|line| line.contains("message 39")));
    assert!(!followed.iter().any(|line| line.contains("End to follow")));
}

#[test]
fn conversation_title_is_the_dashboard_summary_without_the_session_name() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.set_header_summary("precision-3260/bifrost-fuzz", "kimi");
    chat.turn_started_at_epoch_seconds = Some(7_847);
    chat.set_current_step_start(Some(20_000_000));

    assert_eq!(
        transcript_title(&chat, 20_000),
        " precision-3260/bifrost-fuzz  Turn 3h22m  Step 0s  kimi "
    );

    chat.render_mode = TranscriptRenderMode::Raw;
    assert_eq!(
        transcript_title(&chat, 20_000),
        " precision-3260/bifrost-fuzz  Turn 3h22m  Step 0s  kimi · raw source "
    );

    // An idle session that left a command running names it in the same place
    // the turn clock goes.
    chat.render_mode = TranscriptRenderMode::Rich;
    chat.turn_started_at_epoch_seconds = None;
    chat.set_session_activity(crate::usage_format::SessionActivity {
        execution: None,
        harness_turn_started_at_ms: None,
        foreground_tool_started_at_ms: None,
        background_commands: vec![hel::hel_worker::BackgroundCommand {
            started_at_ms: 17_384_000,
            command: "cargo test".into(),
        }],
        active_user_shells: Vec::new(),
    });
    assert_eq!(
        transcript_title(&chat, 20_000),
        " precision-3260/bifrost-fuzz    BG 43m36s  kimi "
    );

    let previous_activity = chat.session_activity().clone();
    chat.set_session_activity(crate::usage_format::SessionActivity {
        foreground_tool_started_at_ms: Some(19_988_000),
        ..previous_activity
    });
    assert_eq!(
        transcript_title(&chat, 20_000),
        " precision-3260/bifrost-fuzz  Step 12s  kimi "
    );
}

#[test]
fn conversation_title_shows_review_activity_then_restores_primary_activity() {
    use hel::hel_review::driver::{RoleState, RoleStatus, TurnReviewPhase, VALIDATOR_ROLE};
    use mj_controller::hel_review_host::RuntimeReviewView;

    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.set_header_summary("podman", "codex3");
    let idle = transcript_title(&chat, 20_000);
    assert!(idle.contains("[idle]"));
    let mut view = RuntimeReviewView {
        session_id: "session".to_owned(),
        tier: hel::hel_review::lanes::ReviewTier::Quick,
        phase: TurnReviewPhase::LaunchingReviewer,
        roles: Vec::new(),
        status: "starting the reviewer".to_owned(),
        verdict: None,
    };
    chat.set_turn_review(Some(view.clone()));
    assert_eq!(
        transcript_title(&chat, 20_000),
        " podman  [Reviewing]  codex3 "
    );
    view.phase = TurnReviewPhase::Running {
        roles: vec![RoleStatus {
            role: VALIDATOR_ROLE.to_owned(),
            label: "Validator".to_owned(),
            state: RoleState::Running,
        }],
    };
    chat.set_turn_review(Some(view));
    assert!(transcript_title(&chat, 20_000).contains("[Validating]"));
    chat.set_turn_review(None);
    assert_eq!(transcript_title(&chat, 20_000), idle);
}

#[test]
fn live_unclaimed_terminal_renders_a_quiet_running_card() {
    let started_at_ms = hel::clock::epoch_millis();
    let terminal = hel::hel_worker::ActiveAgentTerminal {
        terminal_id: "term-1".into(),
        command: "cargo mutants --in-diff diff".into(),
        started_at_ms,
    };
    let mut chat = ChatState::new(&snapshot(), &[]);
    let session = MaterializedSession::empty("session-live-terminal");
    chat.set_active_agent_terminals(std::slice::from_ref(&terminal), &session);

    let rendered = transcript_text(&mut chat, 80);
    assert!(
        rendered
            .iter()
            .any(|line| line.contains("cargo mutants --in-diff diff")),
        "the command identifies useful live work: {rendered:?}"
    );
    assert!(
        rendered.iter().any(|line| line.contains("Running ·")),
        "the card says that the command is still running: {rendered:?}"
    );
    assert!(
        !rendered.iter().any(|line| line.contains("No messages yet")),
        "live work replaces the misleading empty state: {rendered:?}"
    );

    chat.set_active_agent_terminals(&[], &session);
    let after_exit = transcript_text(&mut chat, 80);
    assert!(
        after_exit
            .iter()
            .any(|line| line.contains("No messages yet")),
        "a successful exit removes the provisional card: {after_exit:?}"
    );
}

#[test]
fn real_tool_claim_suppresses_only_the_matching_terminal_incarnation() {
    let claimed_at_ms = hel::clock::epoch_millis();
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
        },
    })];
    let mut chat = ChatState::from_materialized(&session, &[], &[]);
    chat.set_active_agent_terminals(
        &[hel::hel_worker::ActiveAgentTerminal {
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
        &[hel::hel_worker::ActiveAgentTerminal {
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

#[test]
fn materialized_conversion_preserves_each_transcript_body() {
    let mut session = MaterializedSession::empty("session-bodies");
    session.applied_event_ordinal = 9;
    session.transcript = vec![
        fixture_user_item(1),
        fixture_agent_item(2),
        fixture_thought_item(3),
        fixture_tool_item(4),
        fixture_plan_item(5),
        fixture_system_item(6),
    ];

    let entries = materialized_chat_entries(&session);

    let roles = entries.iter().map(|entry| entry.role).collect::<Vec<_>>();
    assert_eq!(
        roles,
        [
            ChatRole::User,
            ChatRole::Agent,
            ChatRole::Thought,
            ChatRole::Tool,
            ChatRole::Plan,
            ChatRole::System,
        ]
    );
    assert_eq!(entries[0].text, "question 1");
    assert!(entries[1].text.starts_with("answer 2.0 "));
    assert_eq!(entries[1].text.len(), 8 * 40 * "answer 2.0 ".len());
    assert_eq!(entries[1].message_id.as_deref(), Some("agent:2"));
    assert_eq!(entries[2].text, "thinking about 3");
    assert_eq!(entries[3].text, "read file-4");
    assert_eq!(entries[3].tool_status, Some(ToolStatus::Completed));
    assert_eq!(entries[3].tool_call_id.as_deref(), Some("tool:4"));
    assert_eq!(entries[3].tool_content.len(), 1);
    assert_eq!(entries[3].tool_locations, ["src/file-4.rs:3"]);
    assert_eq!(entries[4].plan.len(), 1);
    assert_eq!(entries[4].plan[0].text, "step 5");
    assert_eq!(entries[4].plan[0].status, PlanStatus::Running);
    assert_eq!(entries[5].text, "notice 6");
    for (index, entry) in entries.iter().enumerate() {
        assert_eq!(entry.start_seq, index as u64 + 1);
        assert_eq!(entry.recorded_at_ms, Some(FIXTURE_MS));
        assert_eq!(entry.revision, FIXTURE_MS as u64);
    }
    // The bodies the projection records a change ordinal for keep their
    // own cursor; the ones it edits in place without one keep the frontier.
    assert_eq!(
        entries.iter().map(|entry| entry.seq).collect::<Vec<_>>(),
        [1, 2, 9, 9, 9, 6]
    );
}

#[test]
fn opening_a_long_session_converts_only_the_tail() {
    let items = TAIL_SEED_ITEMS as u64 + 400;
    let session = long_materialized_session(items);

    let chat = ChatState::from_materialized_tail(&session, &[], &[]);

    assert_eq!(chat.entries.len(), TAIL_SEED_ITEMS);
    assert_eq!(chat.unconverted_prefix(), 400);
    let eager = materialized_chat_entries(&session);
    assert_eq!(chat.entries, eager[400..]);
}

#[test]
fn opening_a_short_session_converts_the_whole_transcript() {
    let session = long_materialized_session(TAIL_SEED_ITEMS as u64);

    let chat = ChatState::from_materialized_tail(&session, &[], &[]);

    assert_eq!(chat.unconverted_prefix(), 0);
    assert_eq!(chat.entries, materialized_chat_entries(&session));
}

#[test]
fn splicing_the_converted_prefix_matches_the_eager_projection() {
    let session = long_materialized_session(TAIL_SEED_ITEMS as u64 + 500);
    let mut chat = ChatState::from_materialized_tail(&session, &[], &[]);
    let prefix = converted_prefix(&session, &chat);

    assert!(chat.splice_transcript_prefix(prefix));

    assert_eq!(chat.unconverted_prefix(), 0);
    assert_eq!(chat.entries, materialized_chat_entries(&session));
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
        .filter(|row| row.starts_with("│ message "))
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
fn tool_call_updates_refresh_the_rendered_status() {
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

    assert_eq!(chat.entries.len(), 1);
    assert_eq!(chat.entries[0].tool_status, Some(ToolStatus::Completed));
    assert_eq!(tool_presentation(ToolStatus::Completed).1, "done");
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
        [
            "● Tool · running",
            "│ Edit src/lib.rs",
            "│ /workspace/src/lib.rs",
            ""
        ]
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
        [
            "✓ Tool · done",
            "│ Edit src/lib.rs",
            "│ /workspace/src/lib.rs",
            ""
        ]
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
            "│ grep, grep",
            "",
            "✓ Tool · done",
            "│ cat notes.md",
            "",
        ]
    );
}

#[test]
fn kimi_shell_tool_run_collapses_to_command_names() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.entries
        .push(completed_tool(1, "Running: rg -n project_memory src"));
    chat.entries
        .push(completed_tool(2, "Running: cargo test --lib"));
    chat.entries
        .push(completed_tool(3, "Starting background: npm run preview"));
    chat.entries
        .push(ChatEntry::plain(4, ChatRole::User, "continue"));

    assert_eq!(
        transcript_text(&mut chat, 80),
        [
            "✓ Tool · done",
            "│ rg, cargo, npm",
            "",
            "❯ You",
            "│ continue",
            "",
        ]
    );
}

/// A harness that quotes or decorates the command it reports still names a
/// tool. The label used to keep the decoration, so a collapsed streak read
/// `"sed, ./build, ls`.
#[test]
fn a_collapsed_tool_label_starts_at_the_command_name() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.entries
        .push(completed_tool(1, "Running: \"sed -n 1,10p\" notes.md"));
    chat.entries.push(completed_tool(2, "./build.sh --release"));
    chat.entries.push(completed_tool(3, "Running: `ls -la`"));
    chat.entries
        .push(ChatEntry::plain(4, ChatRole::User, "continue"));

    assert_eq!(
        transcript_text(&mut chat, 80),
        [
            "✓ Tool · done",
            "│ sed, build.sh, ls",
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
            "│ sed, cargo, cargo, cargo, Editing",
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
fn trailing_tool_stays_detailed_until_a_later_thought_appears() {
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
            "│ grep -rn alpha src",
            "",
            "✓ Tool · done",
            "│ cat notes.md",
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
            "│ grep, cat",
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
            "│ grep, grep, cat",
            "",
            "❯ You",
            "│ now ship it",
            "",
        ]
    );
}

#[test]
fn newest_completed_tool_leaves_a_lone_predecessor_expanded() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.entries.push(completed_tool(1, "grep -rn alpha src"));
    chat.entries.push(completed_tool(2, "cat notes.md"));

    let text = transcript_text(&mut chat, 80);

    assert_eq!(
        text,
        [
            "✓ Tool · done",
            "│ grep -rn alpha src",
            "",
            "✓ Tool · done",
            "│ cat notes.md",
            "",
        ]
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
            "│ grep, grep, cat",
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

    // The trailing run's last member is the newest result, so it stays
    // expanded and leaves its single predecessor alone.
    assert_eq!(
        text,
        [
            "✓ Tool · done",
            "│ grep, grep",
            "",
            "× Tool · failed",
            "│ cat missing.md",
            "",
            "✓ Tool · done",
            "│ rg gamma src",
            "",
            "✓ Tool · done",
            "│ rg delta src",
            "",
        ]
    );

    chat.entries
        .push(ChatEntry::plain(6, ChatRole::User, "now ship it"));

    assert_eq!(
        transcript_text(&mut chat, 80),
        [
            "✓ Tool · done",
            "│ grep, grep",
            "",
            "× Tool · failed",
            "│ cat missing.md",
            "",
            "✓ Tool · done",
            "│ rg, rg",
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
fn a_later_running_tool_releases_earlier_results_and_stays_expanded_when_completed() {
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
            "│ grep, grep",
            "",
            "● Tool · running",
            "│ cat notes.md",
            "",
        ]
    );

    chat.entries[2].touch(4);
    chat.entries[2].tool_status = Some(ToolStatus::Completed);

    // Once completed, the trailing tool protects its own full result.
    assert_eq!(
        transcript_text(&mut chat, 80),
        [
            "✓ Tool · done",
            "│ grep, grep",
            "",
            "✓ Tool · done",
            "│ cat notes.md",
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
            .is_none_or(|span| span.content != ROLE_GUTTER)),
        "the comparison rows have no gutter left to match"
    );

    assert_eq!(render_agent_message_tail(text, 38, usize::MAX), body);
    assert_eq!(
        render_agent_message_tail(text, 38, 2),
        body[body.len() - 2..].to_vec()
    );
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
fn blank_rows_inside_messages_keep_the_role_gutter() {
    for (role, color) in [
        (ChatRole::User, Color::Cyan),
        (ChatRole::Agent, Color::Yellow),
    ] {
        let entry = ChatEntry::plain(1, role, "1. first\n\n2. second");
        let lines = render_transcript_entry(&entry, 80, TranscriptRenderMode::Rich);
        let blank = lines
            .iter()
            .find(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
                    == ROLE_GUTTER
            })
            .expect("blank row with role gutter");

        assert_eq!(blank.spans[0].style.fg, Some(color));
        assert!(lines.last().is_some_and(line_is_empty));
        assert!(lines.last().is_some_and(|line| line.spans.is_empty()));
    }
}

#[test]
fn transcript_snapshot_tail_matches_rich_conversation_rows() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.entries
        .push(ChatEntry::plain(1, ChatRole::User, "inspect the renderer"));
    chat.entries.push(ChatEntry::plain(
        2,
        ChatRole::Agent,
        "**Done.**\n\n- shared renderer\n- live tail",
    ));
    let expected = transcript_lines(&mut chat, 32)
        .into_iter()
        .filter(|line| !line_is_empty(line))
        .collect::<Vec<_>>();
    let expected = line_text(expected);
    let expected = expected[expected.len().saturating_sub(6)..].to_vec();

    let mut snapshot = chat.transcript_snapshot();
    assert_eq!(line_text(snapshot.rich_tail(32, 6)), expected);
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

#[test]
fn markdown_list_wrapping_uses_a_hanging_indent() {
    let entry = ChatEntry::plain(1, ChatRole::Agent, "- alpha beta gamma");
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.entries.push(entry);
    let text = transcript_text(&mut chat, 13);

    assert!(text.iter().any(|line| line == "│ • alpha"));
    assert!(text.iter().any(|line| line == "│   beta"));
    assert!(text.iter().any(|line| line == "│   gamma"));
}

#[test]
fn page_navigation_keeps_end_attached_to_the_latest_message() {
    let mut chat = numbered_chat(40);
    let rows = drawn_transcript(&mut chat, 60, 24);
    assert!(shows(&rows, "message 39"), "opens on the newest message");
    assert!(!shows(&rows, "End to follow"), "the tail needs no hint");

    chat.handle_key(key(KeyCode::PageUp));
    let rows = drawn_transcript(&mut chat, 60, 24);
    assert!(!shows(&rows, "message 39"), "page up leaves the tail");
    assert!(
        shows(&rows, "End to follow"),
        "scrolled back says how to return"
    );

    chat.handle_key(key(KeyCode::PageDown));
    let rows = drawn_transcript(&mut chat, 60, 24);
    assert!(shows(&rows, "message 39"), "page down returns to the tail");

    chat.handle_key(key(KeyCode::PageUp));
    chat.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL));
    let rows = drawn_transcript(&mut chat, 60, 24);
    assert!(
        shows(&rows, "message 39"),
        "Ctrl-End follows the tail again"
    );
    assert!(!shows(&rows, "End to follow"));
}

#[test]
fn control_home_and_end_reach_both_ends_of_a_long_transcript() {
    let mut chat = numbered_chat(200);
    let _ = drawn_transcript(&mut chat, 40, 24);

    chat.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::CONTROL));
    let rows = drawn_transcript(&mut chat, 40, 24);
    assert!(shows(&rows, "message 0"), "Ctrl-Home reaches the first row");
    assert!(!shows(&rows, "message 199"));

    chat.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL));
    let rows = drawn_transcript(&mut chat, 40, 24);
    assert!(shows(&rows, "message 199"), "Ctrl-End reaches the last row");
}

#[test]
fn keypad_home_and_end_reach_both_ends_without_editing_the_prompt() {
    let mut chat = numbered_chat(200);
    chat.set_input("draft prompt".into());
    let _ = drawn_transcript(&mut chat, 40, 24);

    chat.handle_key(keypad_key(KeyCode::Home));
    let rows = drawn_transcript(&mut chat, 40, 24);
    assert!(
        shows(&rows, "message 0"),
        "keypad Home reaches the first row"
    );
    assert!(!shows(&rows, "message 199"));
    assert_eq!(chat.input_cursor, "draft prompt".len());

    chat.handle_key(keypad_key(KeyCode::End));
    let rows = drawn_transcript(&mut chat, 40, 24);
    assert!(shows(&rows, "message 199"), "keypad End follows the tail");
    assert_eq!(chat.input_cursor, "draft prompt".len());

    chat.handle_key(key(KeyCode::Home));
    assert_eq!(chat.input_cursor, 0, "plain Home still edits the prompt");
    chat.handle_key(key(KeyCode::End));
    assert_eq!(
        chat.input_cursor,
        "draft prompt".len(),
        "plain End still edits the prompt"
    );
}

#[test]
fn opening_reveals_the_dashboard_agent_excerpt_above_later_terminal_output() {
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
    assert!(shows(&opened, "response advertised on the dashboard"));
    assert!(shows(&opened, "End to follow"));

    chat.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL));
    let tail = drawn_transcript(&mut chat, 60, 24);
    assert!(shows(&tail, "terminal failure 7"));
    assert!(!shows(&tail, "End to follow"));
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
    let _ = drawn_transcript(&mut chat, 40, 24);
    chat.handle_key(key(KeyCode::PageUp));
    let before = drawn_transcript(&mut chat, 40, 24);

    for index in 40..50 {
        chat.entries.push(ChatEntry::plain(
            index as u64,
            ChatRole::User,
            format!("message {index}"),
        ));
    }
    let after = drawn_transcript(&mut chat, 40, 24);

    assert_eq!(
        visible_messages(&before),
        visible_messages(&after),
        "appending messages must not move a scrolled-back viewport"
    );
    assert!(!visible_messages(&after).is_empty());
}

#[test]
fn a_transcript_shorter_than_the_viewport_cannot_scroll() {
    let mut chat = numbered_chat(2);
    let rows = drawn_transcript(&mut chat, 40, 24);

    chat.handle_mouse(wheel(MouseEventKind::ScrollUp));
    chat.handle_key(key(KeyCode::PageUp));

    assert_eq!(rows, drawn_transcript(&mut chat, 40, 24));
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
fn materialized_tool_and_plan_conversion_preserves_more_than_eight_details() {
    let tool_content = (0..12)
        .map(|index| {
            serde_json::json!({
                "type": "content",
                "content": {"type": "text", "text": format!("result-{index}")}
            })
        })
        .collect::<Vec<_>>();
    let locations = (0..12)
        .map(|index| {
            serde_json::json!({
                "path": format!("src/file-{index}.rs"),
                "line": index + 1
            })
        })
        .collect::<Vec<_>>();
    let plan = (0..12)
        .map(|index| {
            serde_json::json!({
                "content": format!("step-{index}"),
                "priority": "medium",
                "status": "pending"
            })
        })
        .collect::<Vec<_>>();
    let mut session = MaterializedSession::empty("session-rich-details");
    session.applied_event_ordinal = 2;
    session.applied_event_digest = "a".repeat(64);
    session.transcript = vec![
        Arc::new(TranscriptItem {
            stable_id: "tool:inspect".into(),
            position: 1,
            latest_content_event_ordinal: None,
            created_at_ms: 1,
            last_changed_at_ms: 1,
            body: TranscriptBody::Tool {
                call: serde_json::json!({
                    "toolCallId": "inspect",
                    "title": "inspect",
                    "status": "completed",
                    "content": tool_content,
                    "locations": locations
                }),
                terminal_outputs: Vec::new(),
                terminal_refs: Vec::new(),
            },
        }),
        Arc::new(TranscriptItem {
            stable_id: "plan:current".into(),
            position: 2,
            latest_content_event_ordinal: None,
            created_at_ms: 2,
            last_changed_at_ms: 2,
            body: TranscriptBody::Plan {
                plan: serde_json::json!({"entries": plan}),
            },
        }),
    ];

    let entries = materialized_chat_entries(&session);
    assert_eq!(entries[0].tool_content.len(), 12);
    assert_eq!(entries[0].tool_locations.len(), 12);
    assert_eq!(entries[1].plan.len(), 12);

    let browser = TranscriptSnapshot::from_materialized(&session).browser_transcript(None);
    // The remote viewer mirrors the TUI's Rich feed, so a tool entry is its
    // title alone: neither the content details nor the locations belong
    // there, however many the projection kept for Raw mode.
    assert_eq!(browser.entries[0].lines, ["inspect"]);
    assert!(
        browser.entries[1]
            .lines
            .iter()
            .any(|line| line == "○ step-11")
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
        hel::hel_acp::fallback_terminal_tool_call(&record.terminal_id, "cargo build".into());
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
fn a_clean_fallback_terminal_tool_renders_only_in_raw_mode() {
    let session = fallback_terminal_session(terminal_record(Some(0), None));
    let entries = materialized_chat_entries(&session);
    assert!(entries[0].raw_only);

    let mut chat = ChatState::from_materialized(&session, &[], &[]);
    let rich = transcript_text(&mut chat, 80);
    assert!(!rich.iter().any(|line| line.contains("cargo build")));
    assert!(!rich.iter().any(|line| line.contains(STANDALONE_OUTPUT)));
    chat.render_mode = TranscriptRenderMode::Raw;
    let raw = transcript_text(&mut chat, 80);
    assert!(raw.iter().any(|line| line.contains("cargo build")));
    assert!(raw.iter().any(|line| line.contains(STANDALONE_OUTPUT)));
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
    // Ends the newest result's protection, so both tools can collapse.
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
fn browser_tool_entries_show_the_title_and_diffstats_only() {
    let mut session = MaterializedSession::empty("session-browser-tool");
    session.applied_event_ordinal = 1;
    session.applied_event_digest = "a".repeat(64);
    session.transcript = vec![Arc::new(TranscriptItem {
        stable_id: "tool:edit".into(),
        position: 1,
        latest_content_event_ordinal: None,
        created_at_ms: 1,
        last_changed_at_ms: 1,
        body: TranscriptBody::Tool {
            call: serde_json::json!({
                "toolCallId": "edit",
                "title": "Edit src/lib.rs",
                "status": "completed",
                "content": [
                    {
                        "type": "content",
                        "content": {"type": "text", "text": "wrote the file"}
                    },
                    {
                        "type": "diff",
                        "path": "/workspace/src/lib.rs",
                        "oldText": "alpha\n",
                        "newText": "alpha\nbeta\n"
                    }
                ],
                "locations": [{"path": "/workspace/src/lib.rs", "line": 2}]
            }),
            terminal_outputs: Vec::new(),
            terminal_refs: Vec::new(),
        },
    })];

    let entries = materialized_chat_entries(&session);
    assert!(entries[0].tool_content.contains(&"wrote the file".into()));
    assert_eq!(entries[0].tool_locations, ["/workspace/src/lib.rs:2"]);

    let exact_diffstats = BTreeMap::from([(
        "tool:edit".to_owned(),
        materialized_tool_diffstats(&session.transcript[0]).unwrap(),
    )]);
    let browser = TranscriptSnapshot::from_materialized_with_diffstats(&session, &exact_diffstats)
        .browser_transcript(None);
    assert_eq!(
        browser.entries[0].lines,
        ["Edit src/lib.rs", "/workspace/src/lib.rs  +1 −0"],
        "the remote viewer carries the Rich feed's title and diffstat, \
             not the Raw content or locations"
    );
}

#[test]
fn appending_a_chunk_reuses_earlier_entries_by_pointer_identity() {
    let mut session = MaterializedSession::empty("session-pointer-reuse");
    session.applied_event_ordinal = 3;
    session.transcript = vec![
        user_transcript_item(1, "first"),
        user_transcript_item(2, "second"),
        agent_transcript_item("agent:3", 3),
    ];

    let mut chat = ChatState::from_materialized(&session, &[], &[]);
    // Nothing about these entries matches their item any more, so only a
    // pointer comparison can reuse them.
    for (index, entry) in chat.entries.iter_mut().take(2).enumerate() {
        entry.text = format!("reused {index}");
        entry.revision = u64::MAX;
        entry.recorded_at_ms = None;
    }

    let tail = Arc::make_mut(&mut session.transcript[2]);
    let TranscriptBody::Agent { chunks, .. } = &mut tail.body else {
        panic!("expected an agent message");
    };
    chunks.push(serde_json::json!({
        "content": {"type": "text", "text": " again"}
    }));
    tail.last_changed_at_ms = 40;
    tail.latest_content_event_ordinal = Some(4);
    session.applied_event_ordinal = 4;
    chat.apply_materialized(&session, &[], &[]);

    assert_eq!(chat.entries.len(), 3);
    assert_eq!(chat.entries[0].text, "reused 0");
    assert_eq!(chat.entries[1].text, "reused 1");
    assert!(chat.entries[0].source.is(&session.transcript[0]));
    assert!(chat.entries[1].source.is(&session.transcript[1]));
    assert_eq!(chat.entries[2].text, "hello again");
    assert!(chat.entries[2].source.is(&session.transcript[2]));
}

#[test]
fn restored_transcript_reuses_entries_through_the_field_fallback() {
    let mut session = MaterializedSession::empty("session-restored");
    session.applied_event_ordinal = 2;
    session.transcript = vec![
        user_transcript_item(1, "first"),
        user_transcript_item(2, "second"),
    ];
    let mut chat = ChatState::from_materialized(&session, &[], &[]);
    chat.entries[0].text = "reused".into();

    // A restore rebuilds every item, so nothing is pointer-identical even
    // though the content is unchanged.
    let mut restored = MaterializedSession::empty("session-restored");
    restored.applied_event_ordinal = 3;
    restored.transcript = vec![
        user_transcript_item(1, "first"),
        user_transcript_item(2, "second"),
        agent_transcript_item("agent:3", 3),
    ];
    chat.apply_materialized(&restored, &[], &[]);

    assert_eq!(
        chat.entries
            .iter()
            .map(|entry| entry.text.as_str())
            .collect::<Vec<_>>(),
        ["reused", "second", "hello"]
    );
    // Reuse re-points the entry at the item it now stands for, so the next
    // projection can take the pointer path again.
    for (entry, item) in chat.entries.iter().zip(&restored.transcript) {
        assert!(entry.source.is(item));
    }
}

#[test]
fn raw_mode_preserves_markdown_markers_and_exposes_tool_details() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.entries
        .push(ChatEntry::plain(1, ChatRole::Agent, "**bold**"));
    chat.render_mode = TranscriptRenderMode::Raw;
    assert!(transcript_text(&mut chat, 30).contains(&"│ **bold**".into()));
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
    assert_eq!(
        copied.split('\n').collect::<Vec<_>>(),
        rows[start..]
            .iter()
            .map(|row| row.trim_end())
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
