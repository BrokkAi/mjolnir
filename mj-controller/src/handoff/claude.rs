//! Claude's native compaction records from a verified checkpoint.

use std::io::{BufRead, BufReader, Cursor};
use std::path::Path;

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, ensure};
use chrono::DateTime;
use mj_checkpoint::archive::{
    ArchiveManifest, CanonicalSessionSnapshot, CanonicalTranscriptBody, PayloadRole,
    read_verified_checkpoint_payload,
};
use mj_core::config::HarnessKind;
use mj_core::transcript::materialized_content_text;
use mj_transcript::handoff::reusable_handoff;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::compaction::PriorCompaction;

const MAX_NATIVE_TRANSCRIPT_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug)]
struct NativeCompaction {
    pub summary: String,
    pub boundary_at_ms: i64,
    pub summary_at_ms: i64,
    before_record: usize,
}

#[derive(Debug)]
struct NativeTranscript {
    records: Vec<NativeRecord>,
    compactions: Vec<NativeCompaction>,
}

#[derive(Debug)]
enum NativeRecord {
    User { text: String },
    ToolUse { id: String },
    ToolResult { id: String },
}

/// Read only the primary session JSONL. Subagent files and project memory are
/// separate native artifacts and cannot establish the parent's compaction.
fn read_native_compactions(
    archive_path: &Path,
    manifest: &ArchiveManifest,
    cancel: &CancellationToken,
) -> Result<Option<NativeTranscript>> {
    check_cancel(cancel)?;
    if manifest.session.harness_kind != HarnessKind::Claude {
        return Ok(None);
    }
    let name = format!("{}.jsonl", manifest.session.native_session_id);
    let descriptor = manifest.payloads.iter().find(|descriptor| {
        matches!(&descriptor.role, PayloadRole::NativeArtifact { relative_path }
                if relative_path.file_name().is_some_and(|part| part == name.as_str())
                    && !relative_path.components().any(|part| part.as_os_str() == "subagents"))
    });
    let Some(descriptor) = descriptor else {
        return Ok(None);
    };
    check_cancel(cancel)?;
    let bytes =
        read_verified_checkpoint_payload(archive_path, descriptor, MAX_NATIVE_TRANSCRIPT_BYTES)?;
    check_cancel(cancel)?;
    parse_native_compactions(&bytes, cancel).map(Some)
}

/// The native summary is usable only when its surrounding native conversation
/// can be aligned to this checkpoint's canonical conversation by identity.
pub(super) fn prior_compaction(
    archive_path: &Path,
    manifest: &ArchiveManifest,
    snapshot: &CanonicalSessionSnapshot,
    cancel: &CancellationToken,
) -> Result<Option<PriorCompaction>> {
    check_cancel(cancel)?;
    let Some(transcript) = read_native_compactions(archive_path, manifest, cancel)? else {
        return Ok(None);
    };
    for native in transcript.compactions.iter().rev() {
        check_cancel(cancel)?;
        if let Some(prior) = map_compaction(
            native,
            &transcript.records[..native.before_record],
            &transcript.records[native.before_record..],
            snapshot,
        ) {
            check_cancel(cancel)?;
            return Ok(Some(prior));
        }
    }
    check_cancel(cancel)?;
    Ok(None)
}

fn check_cancel(cancel: &CancellationToken) -> Result<()> {
    ensure!(
        !cancel.is_cancelled(),
        "Claude native compaction preparation cancelled"
    );
    Ok(())
}

fn map_compaction(
    native: &NativeCompaction,
    preceding: &[NativeRecord],
    following: &[NativeRecord],
    snapshot: &CanonicalSessionSnapshot,
) -> Option<PriorCompaction> {
    if native.summary_at_ms < native.boundary_at_ms {
        return None;
    }
    let context_start = snapshot.current_context_start();
    let generic = reusable_handoff(snapshot);
    let lineage_start = generic.map_or(context_start, |state| {
        context_start.max(state.through_position)
    });
    let canonical_users = snapshot
        .transcript
        .iter()
        .filter_map(|item| {
            if item.position <= lineage_start {
                return None;
            }
            match &item.body {
                CanonicalTranscriptBody::User { content } => {
                    let text = materialized_content_text(content);
                    Some((
                        item.position,
                        mj_core::relay::strip_hidden_prompt_context(&text)
                            .trim()
                            .to_owned(),
                    ))
                }
                _ => None,
            }
        })
        .collect::<Vec<_>>();
    let native_users = preceding
        .iter()
        .filter_map(|record| match record {
            NativeRecord::User { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let anchor_text = *native_users.last()?;
    let matching_anchors = canonical_users
        .iter()
        .enumerate()
        .filter(|(_, (_, text))| text == anchor_text)
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let [anchor_index] = matching_anchors.as_slice() else {
        return None;
    };
    let expected = &canonical_users[..=*anchor_index];
    if native_users.len() < expected.len()
        || !native_users[native_users.len() - expected.len()..]
            .iter()
            .zip(expected)
            .all(|(native, (_, canonical))| *native == canonical)
    {
        return None;
    }

    let mut uses = HashMap::<&str, (usize, usize)>::new();
    let mut results = HashMap::<&str, (usize, usize)>::new();
    for (index, record) in preceding.iter().enumerate() {
        match record {
            NativeRecord::ToolUse { id, .. } => {
                let entry = uses.entry(id).or_default();
                entry.0 += 1;
                entry.1 = index;
            }
            NativeRecord::ToolResult { id, .. } => {
                let entry = results.entry(id).or_default();
                entry.0 += 1;
                entry.1 = index;
            }
            NativeRecord::User { .. } => {}
        }
    }
    let completed = uses
        .iter()
        .filter_map(|(id, (count, use_index))| {
            let (result_count, result_index) = results.get(id)?;
            (*count == 1 && *result_count == 1 && use_index < result_index).then_some(*id)
        })
        .collect::<HashSet<_>>();
    let post_ids = following
        .iter()
        .filter_map(|record| match record {
            NativeRecord::ToolUse { id, .. } | NativeRecord::ToolResult { id, .. } => {
                Some(id.as_str())
            }
            _ => None,
        })
        .collect::<HashSet<_>>();
    let (tool_position, _) = snapshot
        .transcript
        .iter()
        .filter_map(|item| {
            if item.position <= canonical_users[*anchor_index].0 {
                return None;
            }
            let CanonicalTranscriptBody::Tool { call, .. } = &item.body else {
                return None;
            };
            let id = call.get("toolCallId")?.as_str()?;
            completed.contains(id).then_some((item.position, id))
        })
        .filter(|(_, id)| !post_ids.contains(id))
        .max_by_key(|(position, _)| *position)?;

    let cutoff = tool_position.checked_sub(1)?;
    if cutoff <= lineage_start {
        return None;
    }
    let mut represented_users = expected
        .iter()
        .map(|(position, _)| *position)
        .collect::<HashSet<_>>();
    let mut retained_positions = Vec::new();
    for item in snapshot
        .transcript
        .iter()
        .filter(|item| item.position > lineage_start && item.position < tool_position)
    {
        let barrier = match &item.body {
            CanonicalTranscriptBody::User { .. } => !represented_users.remove(&item.position),
            CanonicalTranscriptBody::Tool {
                call,
                terminal_refs,
                terminal_outputs,
                ..
            } => {
                let id = call.get("toolCallId").and_then(Value::as_str);
                !id.is_some_and(|id| completed.contains(id) && !post_ids.contains(id))
                    || !terminal_refs.is_empty()
                    || !terminal_outputs.is_empty()
            }
            CanonicalTranscriptBody::Message { .. } | CanonicalTranscriptBody::Plan { .. } => true,
            CanonicalTranscriptBody::Agent { .. } => item
                .latest_content_event_ordinal
                .is_none_or(|ordinal| ordinal >= tool_position),
            CanonicalTranscriptBody::Thought { .. } | CanonicalTranscriptBody::System { .. } => {
                false
            }
            _ => false,
        };
        if barrier {
            retained_positions.push(item.position);
        }
    }
    Some(PriorCompaction {
        through_position: cutoff,
        summary: native.summary.clone(),
        retained_positions,
    })
}

fn parse_native_compactions(bytes: &[u8], cancel: &CancellationToken) -> Result<NativeTranscript> {
    parse_native_compactions_from(BufReader::new(Cursor::new(bytes)), cancel)
}

fn parse_native_compactions_from(
    reader: impl BufRead,
    cancel: &CancellationToken,
) -> Result<NativeTranscript> {
    let mut records = Vec::new();
    let mut compactions = Vec::new();
    let mut pending_boundary = None;
    let mut lines = reader.split(b'\n');
    let mut line_number = 0;
    loop {
        check_cancel(cancel)?;
        let Some(line) = lines.next() else {
            break;
        };
        line_number += 1;
        let line = line?;
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let record: Value = serde_json::from_slice(&line)
            .with_context(|| format!("parse Claude native transcript line {line_number}"))?;
        check_cancel(cancel)?;
        if record.get("isMeta").and_then(Value::as_bool) == Some(true)
            || record.get("isSidechain").and_then(Value::as_bool) == Some(true)
        {
            continue;
        }
        let kind = record.get("type").and_then(Value::as_str);
        let boundary = kind == Some("system")
            && matches!(
                record.get("subtype").and_then(Value::as_str),
                Some("compact_boundary" | "compaction")
            );
        let summary = kind == Some("user")
            && record
                .get("isCompactSummary")
                .or_else(|| record.pointer("/message/isCompactSummary"))
                .and_then(Value::as_bool)
                == Some(true);
        if boundary {
            pending_boundary = timestamp_ms(&record);
            continue;
        }
        if summary {
            if let (Some(boundary_at_ms), Some(summary_at_ms), Some(text)) = (
                pending_boundary.take(),
                timestamp_ms(&record),
                content_text(record.pointer("/message/content")),
            ) && summary_at_ms >= boundary_at_ms
                && !text.trim().is_empty()
            {
                compactions.push(NativeCompaction {
                    summary: text,
                    boundary_at_ms,
                    summary_at_ms,
                    before_record: records.len(),
                });
            }
            continue;
        }
        // A boundary without its immediately following summary is not a valid
        // pair. Other record types may occur between the two in newer Claude
        // streams, but a real conversational record cannot.
        if matches!(kind, Some("user" | "assistant")) {
            pending_boundary = None;
        }
        match kind {
            Some("user") => {
                let content = record.pointer("/message/content");
                let has_tool_result = content.and_then(Value::as_array).is_some_and(|blocks| {
                    blocks.iter().any(|block| {
                        block.get("type").and_then(Value::as_str) == Some("tool_result")
                    })
                });
                if !has_tool_result && let Some(text) = content_text(content) {
                    let text = mj_core::relay::strip_hidden_prompt_context(&text).trim();
                    if !text.is_empty() {
                        records.push(NativeRecord::User {
                            text: text.to_owned(),
                        });
                    }
                }
                for block in record
                    .pointer("/message/content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if block.get("type").and_then(Value::as_str) == Some("tool_result")
                        && let Some(id) = block.get("tool_use_id").and_then(Value::as_str)
                    {
                        records.push(NativeRecord::ToolResult { id: id.to_owned() });
                    }
                }
            }
            Some("assistant") => {
                for block in record
                    .pointer("/message/content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if block.get("type").and_then(Value::as_str) == Some("tool_use")
                        && let Some(id) = block.get("id").and_then(Value::as_str)
                    {
                        records.push(NativeRecord::ToolUse { id: id.to_owned() });
                    }
                }
            }
            _ => {}
        }
    }
    Ok(NativeTranscript {
        records,
        compactions,
    })
}

fn timestamp_ms(record: &Value) -> Option<i64> {
    record
        .get("timestamp")?
        .as_str()
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.timestamp_millis())
}

fn content_text(content: Option<&Value>) -> Option<String> {
    let content = content?;
    if let Some(text) = content.as_str() {
        return Some(text.to_owned());
    }
    let blocks = content.as_array()?;
    let text = blocks
        .iter()
        .filter_map(|block| {
            (block.get("type").and_then(Value::as_str) == Some("text"))
                .then(|| block.get("text").and_then(Value::as_str))
                .flatten()
        })
        .collect::<Vec<_>>()
        .join("\n");
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_checkpoint::archive::{
        CanonicalExecutionState, CanonicalSessionState, CanonicalTranscriptItem,
    };
    use mj_transcript::handoff::{HANDOFF_STATE_KEY, HandoffState};
    use serde_json::json;
    use std::future::Future;
    use std::io::{self, Read};
    use std::pin::Pin;
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    struct RecordingBackend(Mutex<Vec<String>>);

    impl crate::compaction::CompactionBackend for RecordingBackend {
        fn compact<'a>(
            &'a self,
            prompt: String,
        ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
            self.0.lock().unwrap().push(prompt);
            Box::pin(async { Ok("<state_snapshot>merged</state_snapshot>".into()) })
        }
    }

    struct CancelAfterLine<'a> {
        inner: Cursor<&'a [u8]>,
        cancel: CancellationToken,
        reads: &'a AtomicUsize,
    }

    impl Read for CancelAfterLine<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.inner.read(buf)
        }
    }

    impl BufRead for CancelAfterLine<'_> {
        fn fill_buf(&mut self) -> io::Result<&[u8]> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            self.inner.fill_buf()
        }

        fn consume(&mut self, amount: usize) {
            self.inner.consume(amount);
            if amount > 0 {
                self.cancel.cancel();
            }
        }
    }

    fn snapshot(bodies: Vec<CanonicalTranscriptBody>) -> CanonicalSessionSnapshot {
        CanonicalSessionSnapshot {
            command_ledger: None,
            assessment_state: None,
            event_frontier: bodies.len() as u64 + 10,
            event_frontier_digest: "a".repeat(64),
            session: CanonicalSessionState {
                execution: CanonicalExecutionState::Idle,
                last_activity_at_ms: Some(900_000),
                session_title: None,
                configuration: Default::default(),
            },
            transcript: bodies
                .into_iter()
                .enumerate()
                .map(|(index, body)| CanonicalTranscriptItem {
                    stable_id: format!("entry-{index}"),
                    position: index as u64 + 1,
                    latest_content_event_ordinal: None,
                    created_at_ms: 900_000,
                    last_changed_at_ms: 900_000,
                    body,
                })
                .collect(),
            queued_prompts: Vec::new(),
        }
    }

    fn user(text: &str) -> CanonicalTranscriptBody {
        CanonicalTranscriptBody::User {
            content: vec![json!({"type":"text","text":text})],
        }
    }

    fn tool(id: &str, terminal: bool) -> CanonicalTranscriptBody {
        CanonicalTranscriptBody::Tool {
            call: json!({"toolCallId":id,"title":"Run","kind":"execute","status":"completed"}),
            terminal_outputs: Vec::new(),
            terminal_refs: if terminal {
                vec!["terminal-1".to_owned()]
            } else {
                Vec::new()
            },
            presentation: None,
        }
    }

    fn native(records: Vec<NativeRecord>) -> (NativeCompaction, Vec<NativeRecord>) {
        let pair = NativeCompaction {
            summary: "prior state".into(),
            boundary_at_ms: 100,
            summary_at_ms: 101,
            before_record: records.len(),
        };
        (pair, records)
    }

    #[test]
    fn native_parser_keeps_complete_pairs_and_text_block_summary() {
        let lines = [
            json!({"type":"user","timestamp":"2026-01-01T00:00:00Z","message":{"content":"work"}}),
            json!({"type":"system","subtype":"compact_boundary","timestamp":"2026-01-01T00:01:00Z"}),
            json!({"type":"user","isCompactSummary":true,"timestamp":"2026-01-01T00:01:01Z","message":{"content":"old state"}}),
            json!({"type":"assistant","timestamp":"2026-01-01T00:02:00Z","message":{"content":[{"type":"tool_use","id":"tool-a"}]}}),
            json!({"type":"user","timestamp":"2026-01-01T00:02:01Z","message":{"content":[{"type":"tool_result","tool_use_id":"tool-a"}]}}),
            json!({"type":"system","subtype":"compact_boundary","timestamp":"2026-01-01T00:03:00Z"}),
            json!({"type":"user","isCompactSummary":true,"timestamp":"2026-01-01T00:03:01Z","message":{"content":[{"type":"text","text":"new state"}]}}),
            json!({"type":"system","subtype":"compact_boundary","timestamp":"2026-01-01T00:04:00Z"}),
            json!({"type":"user","timestamp":"2026-01-01T00:04:01Z","message":{"content":"more work"}}),
        ];
        let body = lines
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        let parsed = parse_native_compactions(body.as_bytes(), &CancellationToken::new()).unwrap();
        assert_eq!(parsed.compactions.len(), 2);
        assert_eq!(parsed.compactions[1].summary, "new state");
        assert_eq!(parsed.compactions[1].before_record, 3);
    }

    #[test]
    fn native_parser_observes_cancellation_before_and_between_large_lines() {
        let first = json!({"type":"user","message":{"content":"x".repeat(70 * 1024)}}).to_string();
        let body = format!(
            "{first}\n{}\n",
            json!({"type":"user","message":{"content":"later"}})
        );
        let reads = AtomicUsize::new(0);
        let canceled = CancellationToken::new();
        canceled.cancel();
        let reader = CancelAfterLine {
            inner: Cursor::new(body.as_bytes()),
            cancel: canceled.clone(),
            reads: &reads,
        };
        assert!(parse_native_compactions_from(reader, &canceled).is_err());
        assert_eq!(reads.load(Ordering::Relaxed), 0);

        let cancel = CancellationToken::new();
        let reader = CancelAfterLine {
            inner: Cursor::new(body.as_bytes()),
            cancel: cancel.clone(),
            reads: &reads,
        };
        let error = parse_native_compactions_from(reader, &cancel).unwrap_err();
        assert!(error.to_string().contains("cancelled"));
        assert!(reads.load(Ordering::Relaxed) > 0);
    }

    // The native and relay clocks need not agree; tool identity establishes the cut.
    #[test]
    fn native_tool_barrier_replays_the_spanning_tool_and_agent_suffix() {
        let mut canonical = snapshot(vec![
            user("implement it"),
            CanonicalTranscriptBody::Agent {
                chunks: vec![json!({"content":{"type":"text","text":"working"}})],
                streaming: false,
            },
            tool("tool-a", false),
            CanonicalTranscriptBody::Agent {
                chunks: vec![json!({"content":{"type":"text","text":"continued"}})],
                streaming: false,
            },
        ]);
        canonical.transcript[1].latest_content_event_ordinal = Some(2);
        let (pair, records) = native(vec![
            NativeRecord::User {
                text: "implement it".into(),
            },
            NativeRecord::ToolUse {
                id: "tool-a".into(),
            },
            NativeRecord::ToolResult {
                id: "tool-a".into(),
            },
        ]);
        let prior = map_compaction(&pair, &records, &[], &canonical).unwrap();
        assert_eq!(prior.through_position, 2);
        assert!(prior.retained_positions.is_empty());
    }

    // Late terminal output can mutate a prior tool without updating its row timestamp.
    #[test]
    fn terminal_and_controller_plan_are_retained_without_replaying_the_whole_turn() {
        let mut canonical = snapshot(vec![
            user("work"),
            tool("tool-a", true),
            CanonicalTranscriptBody::Plan {
                plan: json!({"entries":[]}),
            },
            CanonicalTranscriptBody::Agent {
                chunks: vec![json!({"content":{"type":"text","text":"late update"}})],
                streaming: false,
            },
            tool("tool-b", false),
        ]);
        canonical.transcript[3].latest_content_event_ordinal = Some(10);
        let (pair, records) = native(vec![
            NativeRecord::User {
                text: "work".into(),
            },
            NativeRecord::ToolUse {
                id: "tool-a".into(),
            },
            NativeRecord::ToolResult {
                id: "tool-a".into(),
            },
            NativeRecord::ToolUse {
                id: "tool-b".into(),
            },
            NativeRecord::ToolResult {
                id: "tool-b".into(),
            },
        ]);
        let prior = map_compaction(&pair, &records, &[], &canonical).unwrap();
        assert_eq!(prior.through_position, 4);
        assert_eq!(prior.retained_positions, vec![2, 3, 4]);
        canonical.transcript[1].body = tool("tool-a", false);
        let prior = map_compaction(&pair, &records, &[], &canonical).unwrap();
        assert_eq!(prior.through_position, 4);
        assert_eq!(prior.retained_positions, vec![3, 4]);
    }

    #[test]
    fn repeated_tool_id_after_boundary_is_not_a_safe_anchor() {
        let canonical = snapshot(vec![user("work"), tool("tool-a", false)]);
        let (pair, records) = native(vec![
            NativeRecord::User {
                text: "work".into(),
            },
            NativeRecord::ToolUse {
                id: "tool-a".into(),
            },
            NativeRecord::ToolResult {
                id: "tool-a".into(),
            },
        ]);
        let following = [NativeRecord::ToolUse {
            id: "tool-a".into(),
        }];
        assert!(map_compaction(&pair, &records, &following, &canonical).is_none());
    }

    #[test]
    fn unrepresented_user_is_replayed_before_the_tool_barrier() {
        let canonical = snapshot(vec![user("first"), user("second"), tool("tool-a", false)]);
        let (pair, records) = native(vec![
            NativeRecord::User {
                text: "first".into(),
            },
            NativeRecord::ToolUse {
                id: "tool-a".into(),
            },
            NativeRecord::ToolResult {
                id: "tool-a".into(),
            },
        ]);
        let prior = map_compaction(&pair, &records, &[], &canonical).unwrap();
        assert_eq!(prior.through_position, 2);
        assert_eq!(prior.retained_positions, vec![2]);
    }

    #[test]
    fn current_context_boundary_limits_native_anchor_search() {
        let mut canonical = snapshot(vec![
            user("duplicate"),
            CanonicalTranscriptBody::System {
                text: "context cleared".into(),
            },
            user("duplicate"),
            tool("tool-a", false),
        ]);
        canonical.transcript[1].stable_id = "context-cleared:2".into();
        let (pair, records) = native(vec![
            NativeRecord::User {
                text: "duplicate".into(),
            },
            NativeRecord::ToolUse {
                id: "tool-a".into(),
            },
            NativeRecord::ToolResult {
                id: "tool-a".into(),
            },
        ]);
        assert_eq!(
            map_compaction(&pair, &records, &[], &canonical)
                .unwrap()
                .through_position,
            3
        );
    }

    #[test]
    fn validated_hidden_handoff_bounds_native_user_lineage() {
        let mut before_handoff = snapshot(vec![user("old harness request")]);
        before_handoff.event_frontier = 1;
        let state = HandoffState::capture(&before_handoff, "generic earlier state".into()).unwrap();
        let mut canonical = snapshot(vec![
            user("old harness request"),
            user("new request"),
            tool("tool-a", false),
        ]);
        canonical.session.configuration.values.insert(
            HANDOFF_STATE_KEY.into(),
            serde_json::to_value(state).unwrap(),
        );
        let (pair, records) = native(vec![
            NativeRecord::User {
                text: "new request".into(),
            },
            NativeRecord::ToolUse {
                id: "tool-a".into(),
            },
            NativeRecord::ToolResult {
                id: "tool-a".into(),
            },
        ]);
        let prior = map_compaction(&pair, &records, &[], &canonical).unwrap();
        assert_eq!(prior.through_position, 2);
    }

    #[tokio::test]
    async fn verified_claude_checkpoint_supplies_seed_and_retained_plan_to_compaction_prompt() {
        use mj_checkpoint::archive::{
            ArchiveInput, BundleManifest, NativeArtifact, SessionManifest, TargetManifest,
            verify_archive_streaming, write_archive_atomic,
        };
        let mut canonical = snapshot(vec![
            user("work"),
            CanonicalTranscriptBody::System {
                text: "startup notice".into(),
            },
            CanonicalTranscriptBody::Thought {
                chunks: vec![json!({"content":{"type":"text","text":"private thought"}})],
                streaming: false,
            },
            CanonicalTranscriptBody::Agent {
                chunks: vec![json!({"content":{"type":"text","text":"old agent text"}})],
                streaming: false,
            },
            tool("tool-a", false),
            CanonicalTranscriptBody::Plan {
                plan: json!({"entries":[{"content":"keep plan","priority":"high","status":"in_progress"}]}),
            },
            tool("tool-b", false),
            CanonicalTranscriptBody::Agent {
                chunks: vec![json!({"content":{"type":"text","text":"new agent text"}})],
                streaming: false,
            },
        ]);
        canonical.transcript[3].latest_content_event_ordinal = Some(4);
        canonical.transcript[7].latest_content_event_ordinal = Some(8);
        let native = [
            json!({"type":"user","message":{"content":"work"}}),
            json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"tool-a"}]}}),
            json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tool-a"}]}}),
            json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"tool-b"}]}}),
            json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tool-b"}]}}),
            json!({"type":"system","subtype":"compact_boundary","timestamp":"2026-01-01T00:00:00Z"}),
            json!({"type":"user","isCompactSummary":true,"timestamp":"2026-01-01T00:00:01Z","message":{"content":"native prior state"}}),
        ].iter().map(Value::to_string).collect::<Vec<_>>().join("\n");
        let input = ArchiveInput {
            session: SessionManifest {
                id: "session-1".into(),
                title: "Work".into(),
                harness_kind: HarnessKind::Claude,
                profile_id: "claude".into(),
                native_session_id: "native-1".into(),
                created_at: "2026-01-01T00:00:00Z".into(),
                checkpointed_at: "2026-01-01T00:01:00Z".into(),
                hel_version: "test".into(),
                relay_version: "test".into(),
                adapter_version: "test".into(),
            },
            target: TargetManifest {
                template_id: "test".into(),
                target_kind: "bare".into(),
                details: Default::default(),
            },
            bundle: BundleManifest {
                id: "bundle".into(),
                primary_repository: "repo".into(),
            },
            canonical_session: canonical,
            native_artifacts: vec![NativeArtifact {
                relative_path: "projects/-work/native-1.jsonl".into(),
                data: native.into_bytes(),
                mode: 0o600,
            }],
            repositories: Vec::new(),
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("checkpoint.zip");
        write_archive_atomic(&path, &input).unwrap();
        let verified = verify_archive_streaming(&path).unwrap();
        let prior = prior_compaction(
            &path,
            &verified.manifest,
            &verified.canonical_session,
            &CancellationToken::new(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(prior.through_position, 6);
        assert_eq!(prior.retained_positions, vec![6]);
        let backend = RecordingBackend(Mutex::new(Vec::new()));
        crate::compaction::compact_snapshot_since(
            &verified.canonical_session,
            crate::compaction::CompactionBudget::uniform(64 * 1024),
            &backend,
            Some(&prior),
        )
        .await
        .unwrap();
        let prompts = backend.0.lock().unwrap();
        assert_eq!(prompts.len(), 1);
        assert!(prompts[0].contains("native prior state"));
        assert!(prompts[0].contains("keep plan"));
        assert!(prompts[0].contains("tool-b"));
        assert!(!prompts[0].contains("old agent text"));
        assert!(!prompts[0].contains("private thought"));
    }
}
