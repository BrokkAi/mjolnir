use super::*;
use mj_checkpoint::archive::{
    CanonicalExecutionState, CanonicalSessionState, CanonicalTranscriptItem,
};
use std::collections::BTreeMap;
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Default)]
struct FakeBackend {
    prompts: Mutex<Vec<String>>,
}

impl CompactionBackend for FakeBackend {
    fn compact<'a>(
        &'a self,
        prompt: String,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
        self.prompts.lock().unwrap().push(prompt);
        Box::pin(async { Ok("<state_snapshot>kept</state_snapshot>".into()) })
    }
}

/// A backend that fails every request the same way, counting the attempts.
struct FailingBackend {
    message: &'static str,
    attempts: AtomicUsize,
}

impl FailingBackend {
    fn new(message: &'static str) -> Self {
        Self {
            message,
            attempts: AtomicUsize::new(0),
        }
    }
}

impl CompactionBackend for FailingBackend {
    fn compact<'a>(
        &'a self,
        _prompt: String,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
        self.attempts.fetch_add(1, Ordering::Relaxed);
        let message = self.message;
        Box::pin(async move { Err(anyhow::anyhow!("{message}")) })
    }
}

/// A backend that rejects an oversize prompt the way a provider does and
/// summarizes anything that fits.
struct OversizeRejectingBackend {
    prompt_limit: usize,
    rejections: AtomicUsize,
}

impl CompactionBackend for OversizeRejectingBackend {
    fn compact<'a>(
        &'a self,
        prompt: String,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
        let rejected = prompt.len() > self.prompt_limit;
        if rejected {
            self.rejections.fetch_add(1, Ordering::Relaxed);
        }
        Box::pin(async move {
            if rejected {
                Err(anyhow::anyhow!(
                    "prompt is too long: input exceeds the context window"
                ))
            } else {
                Ok("<state_snapshot>kept</state_snapshot>".to_owned())
            }
        })
    }
}

fn user(text: &str) -> CanonicalTranscriptBody {
    CanonicalTranscriptBody::User {
        content: vec![serde_json::json!({"type": "text", "text": text})],
    }
}

fn agent(text: &str) -> CanonicalTranscriptBody {
    CanonicalTranscriptBody::Agent {
        chunks: vec![serde_json::json!({"content": {"type": "text", "text": text}})],
        streaming: false,
    }
}

/// A canonical tool item as `projection` writes it: a whole ACP
/// `ToolCall`, not a `sessionUpdate`-tagged update.
fn tool_call(status: &str, text: &str) -> Value {
    serde_json::json!({
        "toolCallId": "call-1",
        "title": "read file",
        "status": status,
        "content": [{"type": "content", "content": {"type": "text", "text": text}}]
    })
}

fn snapshot(bodies: Vec<CanonicalTranscriptBody>) -> CanonicalSessionSnapshot {
    let transcript = bodies
        .into_iter()
        .enumerate()
        .map(|(index, body)| CanonicalTranscriptItem {
            stable_id: format!("item-{index}"),
            position: index as u64 + 1,
            latest_content_event_ordinal: None,
            created_at_ms: 0,
            last_changed_at_ms: 0,
            body,
        })
        .collect();
    CanonicalSessionSnapshot {
        event_frontier: 0,
        event_frontier_digest: "0".repeat(64),
        session: CanonicalSessionState {
            execution: CanonicalExecutionState::Idle,
            last_activity_at_ms: None,
            session_title: None,
            configuration: BTreeMap::new(),
        },
        transcript,
        queued_prompts: Vec::new(),
    }
}

fn exchanges(turns: &[(&str, &str)]) -> CanonicalSessionSnapshot {
    snapshot(
        turns
            .iter()
            .flat_map(|(prompt, answer)| [user(prompt), agent(answer)])
            .collect(),
    )
}

fn completed_tool_output(text: &str) -> TurnEvent {
    TurnEvent::Tool(tool_call("completed", text))
}

/// With no utility model the handoff is built without a model at all:
/// newest turns first until the budget is spent, emitted in order.
#[test]
fn a_verbatim_handoff_keeps_the_newest_turns_that_fit_and_reads_in_order() {
    let padding = "y".repeat(8 * 1024);
    let input = exchanges(&[
        ("oldest question", padding.as_str()),
        ("middle question", padding.as_str()),
        ("newest question", padding.as_str()),
    ]);

    let full = render_recent_snapshot(&input, 64 * 1024);
    assert!(full.starts_with(HANDOFF_PREAMBLE), "{}", &full[..120]);
    assert!(full.contains("oldest question"), "{full}");
    let newest = full.find("newest question").expect("newest turn present");
    let oldest = full.find("oldest question").expect("oldest turn present");
    assert!(oldest < newest, "turns must read oldest-first");

    // A budget that fits only the last turn drops the earlier ones.
    let tight = render_recent_snapshot(&input, 12 * 1024);
    assert!(tight.len() <= 12 * 1024, "{}", tight.len());
    assert!(tight.contains("newest question"), "{tight}");
    assert!(!tight.contains("oldest question"));

    // Even a budget that cannot hold one turn sends what it can rather
    // than handing the target an empty conversation.
    let starved = render_recent_snapshot(&input, MIN_CONTEXT_BYTES);
    assert!(starved.len() <= MIN_CONTEXT_BYTES, "{}", starved.len());
    assert!(starved.starts_with(HANDOFF_PREAMBLE));
}

#[tokio::test]
async fn short_history_uses_one_compaction_request() {
    let backend = FakeBackend::default();
    let handoff = compact_snapshot(
        &exchanges(&[("fix it", "done")]),
        CompactionBudget::uniform(64 * 1024),
        &backend,
    )
    .await
    .unwrap();
    assert_eq!(backend.prompts.lock().unwrap().len(), 1);
    assert!(handoff.contains("<state_snapshot>kept</state_snapshot>"));
}

#[tokio::test]
async fn large_history_pages_then_reduces_and_keeps_exact_tail() {
    let large = "x".repeat(20 * 1024);
    let input = exchanges(&[
        ("first", &large),
        ("second", &large),
        ("latest user", "latest answer"),
    ]);
    let backend = FakeBackend::default();
    let handoff = compact_snapshot(&input, CompactionBudget::uniform(32 * 1024), &backend)
        .await
        .unwrap();
    assert!(backend.prompts.lock().unwrap().len() >= 3);
    assert!(handoff.contains("latest user"));
    assert!(handoff.contains("latest answer"));
}

#[tokio::test]
async fn oversize_turn_is_split_into_summarizable_fragments() {
    let huge = "y".repeat(200 * 1024);
    let input = snapshot(vec![
        user("start"),
        agent(&huge),
        user("end"),
        agent("done"),
    ]);
    let backend = FakeBackend::default();

    compact_snapshot(&input, CompactionBudget::uniform(32 * 1024), &backend)
        .await
        .unwrap();

    assert!(backend.prompts.lock().unwrap().len() >= 6);
    assert!(
        backend
            .prompts
            .lock()
            .unwrap()
            .iter()
            .any(|prompt| prompt.contains("oversize turn fragment"))
    );
}

#[tokio::test]
async fn a_fatal_backend_failure_surfaces_on_the_first_request() {
    let backend = FailingBackend::new("session/prompt failed: 401 unauthorized");

    let error = compact_snapshot(
        &exchanges(&[("fix it", "done")]),
        CompactionBudget::uniform(64 * 1024),
        &backend,
    )
    .await
    .unwrap_err();

    assert_eq!(
        backend.attempts.load(Ordering::Relaxed),
        1,
        "a dead backend must not be asked again"
    );
    assert!(error.to_string().contains("401 unauthorized"), "{error}");
}

#[tokio::test]
async fn an_unrecognized_backend_failure_surfaces_on_the_first_request() {
    let large = "x".repeat(200 * 1024);
    let input = exchanges(&[("first", &large), ("second", &large), ("latest", "answer")]);
    let backend = FailingBackend::new("relay request failed: backend exploded");

    let error = compact_snapshot(
        &input,
        CompactionBudget::uniform(DEFAULT_CONTEXT_BYTES),
        &backend,
    )
    .await
    .unwrap_err();

    assert_eq!(
        backend.attempts.load(Ordering::Relaxed),
        1,
        "only a named size problem earns a smaller retry"
    );
    assert!(error.to_string().contains("backend exploded"), "{error}");
}

#[tokio::test]
async fn an_oversize_rejection_still_splits_until_the_pages_fit() {
    let large = "x".repeat(200 * 1024);
    let input = exchanges(&[("first", &large), ("latest user", "latest answer")]);
    let backend = OversizeRejectingBackend {
        prompt_limit: 32 * 1024,
        rejections: AtomicUsize::new(0),
    };

    let handoff = compact_snapshot(
        &input,
        CompactionBudget::uniform(DEFAULT_CONTEXT_BYTES),
        &backend,
    )
    .await
    .unwrap();

    assert!(
        backend.rejections.load(Ordering::Relaxed) >= 3,
        "the pages had to shrink to fit: {} rejections",
        backend.rejections.load(Ordering::Relaxed)
    );
    assert!(handoff.contains("<state_snapshot>kept</state_snapshot>"));
    assert!(handoff.contains("latest answer"));
}

#[tokio::test]
async fn independent_pages_run_at_the_compaction_concurrency_limit() {
    struct ConcurrentBackend {
        active: AtomicUsize,
        maximum: AtomicUsize,
    }

    impl CompactionBackend for ConcurrentBackend {
        fn compact<'a>(
            &'a self,
            _prompt: String,
        ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
            Box::pin(async move {
                let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
                self.maximum.fetch_max(active, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                self.active.fetch_sub(1, Ordering::SeqCst);
                Ok("<state_snapshot>kept</state_snapshot>".to_string())
            })
        }
    }

    // Each answer fills a page on its own, so this is more than twice as
    // many independent requests as the concurrency limit.
    let large = "p".repeat(20 * 1024);
    let turns = (0..20)
        .map(|index| (format!("prompt {index}"), large.clone()))
        .collect::<Vec<_>>();
    let refs = turns
        .iter()
        .map(|(prompt, answer)| (prompt.as_str(), answer.as_str()))
        .collect::<Vec<_>>();
    let backend = ConcurrentBackend {
        active: AtomicUsize::new(0),
        maximum: AtomicUsize::new(0),
    };

    compact_snapshot(
        &exchanges(&refs),
        CompactionBudget::uniform(32 * 1024),
        &backend,
    )
    .await
    .unwrap();

    assert_eq!(
        backend.maximum.load(Ordering::SeqCst),
        COMPACTION_CONCURRENCY
    );
    assert_eq!(backend.active.load(Ordering::SeqCst), 0);
}

/// Merging two summaries at a time cost one request per pair and a round
/// per level of the tree: 33 pages became 32 further requests, run two at
/// a time. Packing a whole round into one prompt is the fix.
#[tokio::test]
async fn page_summaries_that_fit_one_prompt_reduce_in_a_single_request() {
    let large = "r".repeat(20 * 1024);
    let turns = (0..20)
        .map(|index| (format!("prompt {index}"), large.clone()))
        .collect::<Vec<_>>();
    let refs = turns
        .iter()
        .map(|(prompt, answer)| (prompt.as_str(), answer.as_str()))
        .collect::<Vec<_>>();
    let backend = FakeBackend::default();

    compact_snapshot(
        &exchanges(&refs),
        CompactionBudget::uniform(32 * 1024),
        &backend,
    )
    .await
    .unwrap();

    let prompts = backend.prompts.lock().unwrap();
    let pages = prompts
        .iter()
        .filter(|prompt| prompt.contains("<historical_transcript>"))
        .count();
    let reductions = prompts
        .iter()
        .filter(|prompt| prompt.contains("Merge these contiguous historical state snapshots"))
        .count();
    assert!(pages >= 16, "the transcript must page: {pages} pages");
    assert_eq!(
        reductions, 1,
        "summaries that fit one prompt merge in one request"
    );
}

/// The summarizer's window and the target harness's window are unrelated
/// numbers. A transcript that fits the summarizer takes one request even
/// when the handoff budget is far smaller.
#[tokio::test]
async fn a_wide_page_budget_summarizes_in_one_request_under_a_small_handoff() {
    let large = "w".repeat(100 * 1024);
    let input = exchanges(&[("first", &large), ("second", &large), ("latest", "answer")]);
    let backend = FakeBackend::default();

    let handoff = compact_snapshot(
        &input,
        CompactionBudget {
            page_bytes: 1024 * 1024,
            handoff_bytes: MIN_CONTEXT_BYTES,
        },
        &backend,
    )
    .await
    .unwrap();

    assert_eq!(backend.prompts.lock().unwrap().len(), 1);
    assert!(handoff.len() <= MIN_CONTEXT_BYTES);
}

#[test]
fn reduction_packing_keeps_order_and_fills_each_prompt() {
    let summaries = (0..6)
        .map(|index| format!("{index}").repeat(1024))
        .collect::<Vec<_>>();

    // Room for two of these per prompt, and no more.
    let prompt_room = reduction_prompt(&summaries[..2]).len();
    let groups = pack_reduction_groups(&summaries, prompt_room).unwrap();

    assert_eq!(groups.len(), 3);
    assert!(groups.iter().all(|group| group.len() == 2));
    assert_eq!(
        groups.concat(),
        summaries,
        "a reduction must not reorder history"
    );
}

#[tokio::test]
async fn reduction_that_cannot_pack_any_pair_is_an_error() {
    // A summary that fits a prompt alone but never with a neighbour would
    // repeat the same round forever.
    let summaries = vec!["a".repeat(4 * 1024), "b".repeat(4 * 1024)];
    let single = reduction_prompt(&summaries[..1]).len();
    let backend = FakeBackend::default();

    let error = reduce_summaries(summaries, single, Requests::new(&backend))
        .await
        .unwrap_err();

    assert!(error.to_string().contains("cannot merge"), "{error}");
    assert!(backend.prompts.lock().unwrap().is_empty());
}

#[test]
fn a_single_snapshot_too_large_for_its_own_prompt_is_an_error() {
    let error = pack_reduction_groups(&["z".repeat(64 * 1024)], MIN_CONTEXT_BYTES).unwrap_err();

    assert!(error.to_string().contains("context byte budget"), "{error}");
}

#[test]
fn failures_are_classified_by_what_a_smaller_page_could_fix() {
    for oversize in [
        "prompt is too long",
        "input exceeds the context window",
        "429 too many tokens for this model",
    ] {
        assert_eq!(
            classify_failure_detail(oversize),
            CompactionFailure::Oversize,
            "{oversize}"
        );
    }
    // Anything that does not name a size problem is fatal, including a
    // reason this boundary has no marker for.
    for fatal in [
        "401 Unauthorized: invalid API key",
        "credentials expired; run the login flow again",
        "usage limit reached until 3pm",
        "connection refused",
        "relay request failed: backend exploded",
    ] {
        assert_eq!(
            classify_failure_detail(fatal),
            CompactionFailure::Fatal,
            "{fatal}"
        );
    }
}

#[tokio::test]
async fn handoff_over_the_budget_is_an_error() {
    struct OversizeBackend;

    impl CompactionBackend for OversizeBackend {
        fn compact<'a>(
            &'a self,
            _prompt: String,
        ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
            Box::pin(async { Ok("z".repeat(64 * 1024)) })
        }
    }

    let error = compact_snapshot(
        &exchanges(&[("fix it", "done")]),
        CompactionBudget::uniform(MIN_CONTEXT_BYTES),
        &OversizeBackend,
    )
    .await
    .unwrap_err();

    assert!(error.to_string().contains("context byte budget"), "{error}");
}

#[test]
fn old_tool_outputs_follow_opencode_v2_pruning_policy() {
    let large_output = "x".repeat(TOOL_OUTPUT_PROTECT_BYTES + 1);
    let turns = vec![
        Turn {
            user: "old".into(),
            events: vec![completed_tool_output(&large_output)],
        },
        Turn {
            user: "middle".into(),
            events: Vec::new(),
        },
        Turn {
            user: "recent".into(),
            events: vec![completed_tool_output(&large_output)],
        },
        Turn {
            user: "latest".into(),
            events: Vec::new(),
        },
    ];

    let pruned = prune_old_tool_outputs(&turns);
    let rendered_head = render_turns(&pruned[..2], 0);
    let rendered_tail = render_turns(&pruned[2..], 2);
    assert!(rendered_head.contains(CLEARED_TOOL_RESULT));
    assert!(!rendered_head.contains(&large_output));
    assert!(rendered_tail.contains(&large_output));
}

#[test]
fn unfinished_tool_output_is_never_pruned() {
    let large_output = "x".repeat(TOOL_OUTPUT_PROTECT_BYTES + 1);
    let turns = vec![
        Turn {
            user: "old".into(),
            events: vec![TurnEvent::Tool(tool_call("in_progress", &large_output))],
        },
        Turn {
            user: "recent".into(),
            events: vec![completed_tool_output(&large_output)],
        },
        Turn {
            user: "latest".into(),
            events: Vec::new(),
        },
    ];

    let pruned = prune_old_tool_outputs(&turns);

    assert!(!render_turns(&pruned, 0).contains(CLEARED_TOOL_RESULT));
}

#[test]
fn prior_handoff_turn_keeps_its_work_under_a_placeholder() {
    for preamble in [
        HANDOFF_PREAMBLE,
        LEGACY_HANDOFF_PREAMBLE,
        ARCHIVE_HANDOFF_PREAMBLE,
    ] {
        let handoff_text = format!("{preamble} Everything the prior harness knew, verbatim.");
        let turns = turns_from_snapshot(&snapshot(vec![
            user("real user"),
            agent("real answer"),
            user(&handoff_text),
            agent("handoff response"),
        ]))
        .unwrap();

        let rendered = render_turns(&turns, 0);
        assert_eq!(turns.len(), 2);
        assert!(rendered.contains("real user"));
        assert!(rendered.contains(HANDOFF_PLACEHOLDER));
        assert!(!rendered.contains("verbatim"));
        assert!(
            rendered.contains("handoff response"),
            "work done after a handoff is real history"
        );
    }
}

#[test]
fn thoughts_and_system_notices_are_left_out() {
    let turns = turns_from_snapshot(&snapshot(vec![
        user("do it"),
        CanonicalTranscriptBody::Thought {
            chunks: vec![serde_json::json!({"content": {"type": "text", "text": "musing"}})],
            streaming: false,
        },
        CanonicalTranscriptBody::System {
            text: "target restarted".into(),
        },
        agent("done"),
    ]))
    .unwrap();

    let rendered = render_turns(&turns, 0);
    assert!(rendered.contains("done"));
    assert!(!rendered.contains("musing"));
    assert!(!rendered.contains("target restarted"));
}

#[test]
fn plan_and_tool_events_join_their_user_turn() {
    let turns = turns_from_snapshot(&snapshot(vec![
        user("do it"),
        CanonicalTranscriptBody::Plan {
            plan: serde_json::json!({"entries": [{"content": "step one", "status": "pending", "priority": "medium"}]}),
        },
        CanonicalTranscriptBody::Tool {
            call: tool_call("completed", "tool output"),
            terminal_outputs: Vec::new(),
            terminal_refs: Vec::new(),
            presentation: None,
        },
    ]))
    .unwrap();

    assert_eq!(turns.len(), 1);
    let rendered = render_turns(&turns, 0);
    assert!(rendered.contains("step one"));
    assert!(rendered.contains("tool output"));
}

#[test]
fn agent_history_before_a_user_turn_is_an_error() {
    let error = turns_from_snapshot(&snapshot(vec![agent("orphan")])).unwrap_err();

    assert!(
        error.to_string().contains("before its first user turn"),
        "{error}"
    );
}

#[test]
fn startup_tool_history_before_a_user_turn_is_ignored() {
    let turns = turns_from_snapshot(&snapshot(vec![
        CanonicalTranscriptBody::Tool {
            call: tool_call("failed", "MCP server startup was cancelled"),
            terminal_outputs: Vec::new(),
            terminal_refs: Vec::new(),
            presentation: None,
        },
        user("do the work"),
        agent("done"),
    ]))
    .unwrap();

    let rendered = render_turns(&turns, 0);
    assert_eq!(turns.len(), 1);
    assert!(rendered.contains("do the work"));
    assert!(rendered.contains("done"));
    assert!(!rendered.contains("startup was cancelled"));
}

#[test]
fn a_transcript_without_user_turns_is_an_error() {
    let error = turns_from_snapshot(&snapshot(Vec::new())).unwrap_err();

    assert!(error.to_string().contains("no user turns"), "{error}");
}

#[tokio::test]
async fn clear_boundary_excludes_old_history_from_both_handoff_paths() {
    let mut input = exchanges(&[("old secret", "old answer"), ("new question", "new answer")]);
    input.transcript[1].stable_id = "context-cleared:reset".into();
    let verbatim = render_recent_snapshot(&input, 64 * 1024);
    assert!(!verbatim.contains("old secret"));
    assert!(!verbatim.contains("old answer"));
    assert!(verbatim.contains("new question"));
    let backend = FakeBackend::default();
    compact_snapshot(&input, CompactionBudget::uniform(64 * 1024), &backend)
        .await
        .unwrap();
    let prompts = backend.prompts.lock().unwrap();
    assert!(
        prompts
            .iter()
            .all(|prompt| !prompt.contains("old secret") && !prompt.contains("old answer"))
    );
    assert!(prompts.iter().any(|prompt| prompt.contains("new question")));
}
