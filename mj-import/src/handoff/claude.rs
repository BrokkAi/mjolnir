//! Claude native compaction aligned to canonical history by tool identity.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::Path;

use anyhow::{Context, Result};
use mj_checkpoint::archive::{
    ArchiveManifest, CanonicalSessionSnapshot, CanonicalTranscriptBody, CheckpointPayloadReader,
    PayloadRole,
};
use mj_transcript::handoff::reusable_handoff;
use serde::Deserialize;
use serde_json::{Value, value::RawValue};
use tokio_util::sync::CancellationToken;

use crate::compaction::PriorCompaction;

struct NativeCompaction {
    summary: String,
    before_event: usize,
}

struct ToolEvent {
    id: String,
    is_result: bool,
}

pub(super) fn prior_compaction_from_archive(
    archive_path: &Path,
    manifest: &ArchiveManifest,
    snapshot: &CanonicalSessionSnapshot,
    cancel: &CancellationToken,
) -> Result<Option<PriorCompaction>> {
    check_cancel(cancel)?;
    let name = format!("{}.jsonl", manifest.session.native_session_id);
    let descriptor = manifest.payloads.iter().find(|descriptor| {
        matches!(&descriptor.role, PayloadRole::NativeArtifact { relative_path }
                if relative_path.file_name().is_some_and(|part| part == name.as_str())
                    && !relative_path.components().any(|part| part.as_os_str() == "subagents"))
    });
    let Some(descriptor) = descriptor else {
        return Ok(None);
    };
    let mut scanner = ClaudeCompactionScanner::new(cancel);
    CheckpointPayloadReader::open(archive_path)?.copy_payload(descriptor, &mut scanner)?;
    prior_compaction_from_transcript(scanner.finish()?, snapshot)
}

pub(super) fn prior_compaction_from_home(
    harness_home: &Path,
    native_session_id: &str,
    snapshot: &CanonicalSessionSnapshot,
    cancel: &CancellationToken,
) -> Result<Option<PriorCompaction>> {
    check_cancel(cancel)?;
    let name = format!("{native_session_id}.jsonl");
    if Path::new(&name)
        .file_name()
        .is_none_or(|part| part != name.as_str())
    {
        return Ok(None);
    }
    let projects = harness_home.join("projects");
    match fs::symlink_metadata(&projects) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return Ok(None),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    for entry in fs::read_dir(projects)? {
        check_cancel(cancel)?;
        let entry = entry?;
        if entry.file_name() == "subagents" || !entry.file_type()?.is_dir() {
            continue;
        }
        let path = entry.path().join(&name);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() => {
                return prior_compaction_from_reader(File::open(path)?, snapshot, cancel);
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    check_cancel(cancel)?;
    Ok(None)
}

pub(super) fn prior_compaction_from_reader(
    mut reader: impl Read,
    snapshot: &CanonicalSessionSnapshot,
    cancel: &CancellationToken,
) -> Result<Option<PriorCompaction>> {
    check_cancel(cancel)?;
    let mut scanner = ClaudeCompactionScanner::new(cancel);
    io::copy(&mut reader, &mut scanner)?;
    prior_compaction_from_transcript(scanner.finish()?, snapshot)
}

fn prior_compaction_from_transcript(
    scanner: ClaudeCompactionScanner<'_>,
    snapshot: &CanonicalSessionSnapshot,
) -> Result<Option<PriorCompaction>> {
    let context_start = snapshot.current_context_start();
    let lineage_start = reusable_handoff(snapshot).map_or(context_start, |state| {
        context_start.max(state.through_position)
    });
    let mut prior = None;
    for native in scanner.compactions.into_iter().rev() {
        check_cancel(scanner.cancel)?;
        let (preceding, following) = scanner.events.split_at(native.before_event);
        prior = map_compaction(native, preceding, following, snapshot, lineage_start);
        if prior.is_some() {
            break;
        }
    }
    check_cancel(scanner.cancel)?;
    Ok(prior)
}

fn check_cancel(cancel: &CancellationToken) -> io::Result<()> {
    if cancel.is_cancelled() {
        // Interrupted errors are retryable by I/O helpers.
        return Err(io::Error::other(
            "Claude native compaction preparation cancelled",
        ));
    }
    Ok(())
}

fn map_compaction(
    native: NativeCompaction,
    preceding: &[ToolEvent],
    following: &[ToolEvent],
    snapshot: &CanonicalSessionSnapshot,
    lineage_start: u64,
) -> Option<PriorCompaction> {
    let mut uses = HashMap::<&str, (usize, usize)>::new();
    let mut results = HashMap::<&str, (usize, usize)>::new();
    for (index, event) in preceding.iter().enumerate() {
        let counts = if event.is_result {
            &mut results
        } else {
            &mut uses
        };
        let (count, last_index) = counts.entry(&event.id).or_default();
        *count += 1;
        *last_index = index;
    }
    let completed = uses
        .into_iter()
        .filter_map(|(id, (use_count, use_index))| {
            let &(result_count, result_index) = results.get(id)?;
            (use_count == 1 && result_count == 1 && use_index < result_index).then_some(id)
        })
        .collect::<HashSet<_>>();
    let post_ids = following
        .iter()
        .map(|event| event.id.as_str())
        .collect::<HashSet<_>>();
    let covered_tool = |call: &Value| {
        call.get("toolCallId")
            .and_then(Value::as_str)
            .is_some_and(|id| completed.contains(id) && !post_ids.contains(id))
    };
    let tool_position = snapshot
        .transcript
        .iter()
        .filter_map(|item| match &item.body {
            CanonicalTranscriptBody::Tool { call, .. }
                if item.position > lineage_start && covered_tool(call) =>
            {
                Some(item.position)
            }
            _ => None,
        })
        .max()?;

    let cutoff = tool_position.checked_sub(1)?;
    if cutoff <= lineage_start {
        return None;
    }
    let mut retained_positions = Vec::new();
    let mut latest_plan = None;
    for item in snapshot
        .transcript
        .iter()
        .filter(|item| lineage_start < item.position && item.position < tool_position)
    {
        match &item.body {
            CanonicalTranscriptBody::Tool { call, .. } if !covered_tool(call) => {
                retained_positions.push(item.position);
            }
            CanonicalTranscriptBody::Agent { .. }
                if item
                    .latest_content_event_ordinal
                    .is_none_or(|ordinal| ordinal >= tool_position) =>
            {
                retained_positions.push(item.position);
            }
            CanonicalTranscriptBody::Plan { .. } => {
                latest_plan = latest_plan.max(Some(item.position));
            }
            _ => {}
        }
    }
    retained_positions.extend(latest_plan);
    retained_positions.sort_unstable();
    Some(PriorCompaction {
        through_position: cutoff,
        summary: native.summary,
        retained_positions,
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawRecord<'a> {
    #[serde(rename = "type")]
    kind: Option<String>,
    subtype: Option<String>,
    is_meta: Option<bool>,
    is_sidechain: Option<bool>,
    is_compact_summary: Option<bool>,
    // Unrelated record types may use a different message shape.
    #[serde(borrow)]
    message: Option<&'a RawValue>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawMessage<'a> {
    is_compact_summary: Option<bool>,
    // Decode only the summary text or tool IDs needed for the barrier.
    #[serde(borrow)]
    content: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct RawToolBlock {
    #[serde(rename = "type")]
    kind: Option<String>,
    id: Option<String>,
    tool_use_id: Option<String>,
}

fn summary_text(content: &RawValue) -> serde_json::Result<String> {
    #[derive(Deserialize)]
    struct TextBlock {
        #[serde(rename = "type")]
        kind: Option<String>,
        text: Option<String>,
    }

    match content.get().as_bytes().first() {
        Some(b'"') => serde_json::from_str(content.get()),
        Some(b'[') => Ok(serde_json::from_str::<Vec<TextBlock>>(content.get())?
            .into_iter()
            .filter(|block| block.kind.as_deref() == Some("text"))
            .filter_map(|block| block.text)
            .collect::<Vec<_>>()
            .join("\n")),
        _ => Ok(String::new()),
    }
}

struct ClaudeCompactionScanner<'a> {
    cancel: &'a CancellationToken,
    line_buf: Vec<u8>,
    line_number: usize,
    pending_boundary: bool,
    events: Vec<ToolEvent>,
    compactions: Vec<NativeCompaction>,
}

impl<'a> ClaudeCompactionScanner<'a> {
    fn new(cancel: &'a CancellationToken) -> Self {
        Self {
            cancel,
            line_buf: Vec::new(),
            line_number: 0,
            pending_boundary: false,
            events: Vec::new(),
            compactions: Vec::new(),
        }
    }

    fn finish(mut self) -> io::Result<Self> {
        check_cancel(self.cancel)?;
        if !self.line_buf.is_empty() {
            self.parse_line()?;
        }
        Ok(self)
    }

    fn parse_line(&mut self) -> io::Result<()> {
        check_cancel(self.cancel)?;
        self.line_number += 1;
        if !self.line_buf.iter().all(u8::is_ascii_whitespace) {
            self.parse_record()
                .with_context(|| {
                    format!("parse Claude native transcript line {}", self.line_number)
                })
                .map_err(io::Error::other)?;
        }
        self.line_buf.clear();
        Ok(())
    }

    fn parse_record(&mut self) -> serde_json::Result<()> {
        let record: RawRecord<'_> = serde_json::from_slice(&self.line_buf)?;
        if record.is_meta == Some(true) || record.is_sidechain == Some(true) {
            return Ok(());
        }
        let is_result = match record.kind.as_deref() {
            Some("system")
                if matches!(
                    record.subtype.as_deref(),
                    Some("compact_boundary" | "compaction")
                ) =>
            {
                self.pending_boundary = true;
                return Ok(());
            }
            Some("user") => true,
            Some("assistant") => false,
            _ => return Ok(()),
        };
        let pending_boundary = std::mem::take(&mut self.pending_boundary);
        let Some(message) = record.message else {
            return Ok(());
        };
        if !message.get().starts_with('{') {
            return Ok(());
        }
        let message: RawMessage<'_> = serde_json::from_str(message.get())?;
        let Some(content) = message.content else {
            return Ok(());
        };
        if is_result
            && (record.is_compact_summary == Some(true) || message.is_compact_summary == Some(true))
        {
            if pending_boundary {
                let summary = summary_text(content)?;
                if !summary.trim().is_empty() {
                    self.compactions.push(NativeCompaction {
                        summary,
                        before_event: self.events.len(),
                    });
                }
            }
        } else if content.get().starts_with('[') {
            for block in serde_json::from_str::<Vec<RawToolBlock>>(content.get())? {
                let id = match (is_result, block.kind.as_deref()) {
                    (true, Some("tool_result")) => block.tool_use_id,
                    (false, Some("tool_use")) => block.id,
                    _ => None,
                };
                if let Some(id) = id {
                    self.events.push(ToolEvent { id, is_result });
                }
            }
        }
        Ok(())
    }
}

impl Write for ClaudeCompactionScanner<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        check_cancel(self.cancel)?;
        for line in bytes.split_inclusive(|&byte| byte == b'\n') {
            if let Some(line) = line.strip_suffix(b"\n") {
                self.line_buf.extend_from_slice(line);
                self.parse_line()?;
            } else {
                self.line_buf.extend_from_slice(line);
            }
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_checkpoint::archive::{
        CanonicalExecutionState, CanonicalSessionState, CanonicalTranscriptItem,
    };
    use mj_core::config::HarnessKind;
    use mj_transcript::handoff::{HANDOFF_STATE_KEY, HandoffState};
    use serde_json::json;
    use std::future::Future;
    use std::io::Cursor;
    use std::pin::Pin;
    use std::sync::Mutex;

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

    fn snapshot(bodies: Vec<CanonicalTranscriptBody>) -> CanonicalSessionSnapshot {
        CanonicalSessionSnapshot {
            command_ledger: None,
            assessment_state: None,
            notification_stack: None,
            native_use_evidence: None,
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
                .map(|(index, body)| {
                    let stable_id = match &body {
                        CanonicalTranscriptBody::Message { event } => {
                            mj_core::transcript::message_item_id(&event.key)
                        }
                        _ => format!("entry-{index}"),
                    };
                    CanonicalTranscriptItem {
                        stable_id,
                        position: index as u64 + 1,
                        latest_content_event_ordinal: None,
                        created_at_ms: 900_000,
                        last_changed_at_ms: 900_000,
                        body,
                    }
                })
                .collect(),
            queued_prompts: Vec::new(),
            mailbox: None,
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

    fn mailbox_message() -> CanonicalTranscriptBody {
        use mj_core::mailbox::{MailboxEvent, MailboxEventBody};
        CanonicalTranscriptBody::Message {
            event: Box::new(MailboxEvent {
                key: "mailbox-1".into(),
                source: "parent_message".into(),
                wake: true,
                created_at_ms: 900_000,
                body: MailboxEventBody::ParentMessage {
                    text: "mailbox message".into(),
                },
            }),
        }
    }

    fn tool_exchange(id: &str) -> [ToolEvent; 2] {
        [
            ToolEvent {
                id: id.into(),
                is_result: false,
            },
            ToolEvent {
                id: id.into(),
                is_result: true,
            },
        ]
    }

    fn native_prior_for_events(
        canonical: &CanonicalSessionSnapshot,
        preceding: impl IntoIterator<Item = ToolEvent>,
        following: impl IntoIterator<Item = ToolEvent>,
    ) -> Option<PriorCompaction> {
        let cancel = CancellationToken::new();
        let mut scanner = ClaudeCompactionScanner::new(&cancel);
        scanner.events.extend(preceding);
        scanner.compactions.push(NativeCompaction {
            summary: "prior state".into(),
            before_event: scanner.events.len(),
        });
        scanner.events.extend(following);
        prior_compaction_from_transcript(scanner, canonical).unwrap()
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
        let cancel = CancellationToken::new();
        let mut scanner = ClaudeCompactionScanner::new(&cancel);
        scanner.write_all(body.as_bytes()).unwrap();
        let parsed = scanner.finish().unwrap();
        assert_eq!(parsed.compactions.len(), 2);
        assert_eq!(parsed.compactions[1].summary, "new state");
        assert_eq!(parsed.compactions[1].before_event, 2);
    }

    #[test]
    fn native_parser_observes_cancellation_before_and_during_a_large_line() {
        struct CancelMidRead<'a> {
            inner: Cursor<&'a [u8]>,
            cancel: &'a CancellationToken,
        }

        impl Read for CancelMidRead<'_> {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                let count = buf.len().min(4096);
                let read = self.inner.read(&mut buf[..count])?;
                if self.inner.position() >= 8 * 1024 {
                    self.cancel.cancel();
                }
                Ok(read)
            }
        }

        let first = json!({"type":"user","message":{"content":[
            {"type":"tool_result","tool_use_id":"tool-a","content":"x".repeat(70 * 1024)}
        ]}})
        .to_string();
        let body = format!("{first}\n");
        let canonical = snapshot(Vec::new());
        for pre_cancelled in [true, false] {
            let cancel = CancellationToken::new();
            if pre_cancelled {
                cancel.cancel();
            }
            let mut reader = CancelMidRead {
                inner: Cursor::new(body.as_bytes()),
                cancel: &cancel,
            };
            let error = prior_compaction_from_reader(&mut reader, &canonical, &cancel).unwrap_err();
            assert!(error.to_string().contains("cancelled"));
            assert_eq!(
                reader.inner.position(),
                if pre_cancelled { 0 } else { 8 * 1024 }
            );
        }
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
        let prior = native_prior_for_events(&canonical, tool_exchange("tool-a"), []).unwrap();
        assert_eq!(prior.through_position, 2);
        assert!(prior.retained_positions.is_empty());
    }

    #[test]
    fn native_barrier_retains_unfinished_tools_late_agents_and_only_the_latest_plan() {
        let mut canonical = snapshot(vec![
            user("work"),
            tool("tool-a", true),
            mailbox_message(),
            CanonicalTranscriptBody::Plan {
                plan: json!({"entries":[]}),
            },
            tool("unfinished", false),
            CanonicalTranscriptBody::Plan {
                plan: json!({"entries":[]}),
            },
            CanonicalTranscriptBody::Agent {
                chunks: vec![json!({"content":{"type":"text","text":"late update"}})],
                streaming: false,
            },
            tool("tool-b", false),
        ]);
        canonical.transcript[6].latest_content_event_ordinal = Some(10);
        let records = tool_exchange("tool-a")
            .into_iter()
            .chain([ToolEvent {
                id: "unfinished".into(),
                is_result: false,
            }])
            .chain(tool_exchange("tool-b"))
            .collect::<Vec<_>>();
        let prior = native_prior_for_events(&canonical, records, []).unwrap();
        assert_eq!(prior.through_position, 7);
        assert_eq!(prior.retained_positions, vec![5, 6, 7]);
    }

    #[test]
    fn repeated_tool_id_after_boundary_is_not_a_safe_anchor() {
        let canonical = snapshot(vec![user("work"), tool("tool-a", false)]);
        let following = [ToolEvent {
            id: "tool-a".into(),
            is_result: false,
        }];
        assert!(native_prior_for_events(&canonical, tool_exchange("tool-a"), following).is_none());
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
        assert_eq!(
            native_prior_for_events(&canonical, tool_exchange("tool-a"), [])
                .unwrap()
                .through_position,
            3
        );
    }

    #[test]
    fn validated_hidden_handoff_bounds_native_lineage() {
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
        let prior = native_prior_for_events(&canonical, tool_exchange("tool-a"), []).unwrap();
        assert_eq!(prior.through_position, 2);
    }

    #[test]
    fn claude_home_search_uses_only_regular_primary_project_transcripts() {
        let home = tempfile::tempdir().unwrap();
        let projects = home.path().join("projects");
        let slug = projects.join("-work");
        let subagents = projects.join("subagents");
        fs::create_dir_all(&slug).unwrap();
        fs::create_dir_all(&subagents).unwrap();
        let transcript = concat!(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"tool-a"}]}}"#,
            "\n",
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tool-a"}]}}"#,
            "\n",
            r#"{"type":"system","subtype":"compaction"}"#,
            "\n",
            r#"{"type":"user","message":{"isCompactSummary":true,"content":"prior state"}}"#,
        );
        fs::write(subagents.join("native-1.jsonl"), transcript).unwrap();
        let canonical = snapshot(vec![user("work"), tool("tool-a", false)]);
        let cancel = CancellationToken::new();
        let read_prior =
            || prior_compaction_from_home(home.path(), "native-1", &canonical, &cancel);

        assert!(read_prior().unwrap().is_none());
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let link = slug.join("native-1.jsonl");
            symlink(subagents.join("native-1.jsonl"), &link).unwrap();
            symlink(&subagents, projects.join("-linked")).unwrap();
            assert!(read_prior().unwrap().is_none());
            fs::remove_file(link).unwrap();
        }
        fs::write(slug.join("native-1.jsonl"), transcript).unwrap();
        let prior = read_prior().unwrap().unwrap();
        assert_eq!(prior.through_position, 1);
        assert_eq!(prior.summary, "prior state");
    }

    #[tokio::test]
    async fn verified_claude_checkpoint_supplies_seed_and_retained_plan_to_compaction_prompt() {
        use mj_checkpoint::archive::{
            ArchiveInput, BundleManifest, NativeArtifact, SessionManifest, TargetManifest,
            verify_archive_streaming, write_archive_atomic,
        };
        let mut canonical = snapshot(vec![
            user("work"),
            mailbox_message(),
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
        canonical.transcript[4].latest_content_event_ordinal = Some(5);
        canonical.transcript[8].latest_content_event_ordinal = Some(9);
        let native = [
            json!({"type":"user","message":{"content":"work"}}),
            json!({"type":"assistant","message":{"content":[
                {"type":"thinking","thinking":"x".repeat(70 * 1024)},
                {"type":"tool_use","id":"tool-a","input":{"command":"printf test"}}
            ]}}),
            json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tool-a"}]}}),
            json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"tool-b"}]}}),
            json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tool-b"}]}}),
            json!({"type":"user","message":{"content":"<task-notification>background task done</task-notification>"}}),
            json!({"type":"user","message":{"content":"mailbox message"}}),
            json!({"type":"system","subtype":"compact_boundary","timestamp":"2026-01-01T00:00:00.095Z"}),
            json!({"type":"attachment","attachment":{"type":"queued_command"}}),
            json!({"type":"user","isMeta":true,"message":{"content":"injected context"}}),
            json!({"type":"user","isCompactSummary":true,"timestamp":"2026-01-01T00:00:00Z","message":{"content":"native prior state"}}),
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
                data: (native.into_bytes()).into(),
                mode: 0o600,
            }],
            repositories: Vec::new(),
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("checkpoint.zip");
        write_archive_atomic(&path, &input).unwrap();
        let verified = verify_archive_streaming(&path).unwrap();
        let prior = prior_compaction_from_archive(
            &path,
            &verified.manifest,
            &verified.canonical_session,
            &CancellationToken::new(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(prior.through_position, 7);
        assert_eq!(prior.retained_positions, vec![7]);
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
