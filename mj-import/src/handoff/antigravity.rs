//! Antigravity native compaction aligned to canonical history by agent replies.
//!
//! Antigravity writes a `CHECKPOINT` step when it compacts its context. The
//! checkpoint has no tool identity to align on, so it is anchored to the latest
//! agent reply before it whose text identifies exactly one canonical agent item.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use mj_checkpoint::archive::{
    ArchiveManifest, CanonicalSessionSnapshot, CanonicalTranscriptBody, CheckpointPayloadReader,
    PayloadRole,
};
use serde::Deserialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::{
    LineScanner, check_cancel, collect_retained_positions, is_single_path_component, lineage_start,
};
use crate::compaction::PriorCompaction;

const TRANSCRIPT_FILE: &str = "transcript_full.jsonl";

struct NativeCheckpoint {
    summary: String,
    before_reply: usize,
}

/// The directories, below the harness home, that hold a session's transcripts.
fn transcript_directories(native_session_id: &str) -> [&str; 5] {
    [
        "antigravity-cli",
        "brain",
        native_session_id,
        ".system_generated",
        "logs",
    ]
}

/// `transcript.jsonl` truncates checkpoint content, so only the full transcript
/// is read.
fn transcript_path(native_session_id: &str) -> PathBuf {
    transcript_directories(native_session_id)
        .iter()
        .collect::<PathBuf>()
        .join(TRANSCRIPT_FILE)
}

pub(super) fn prior_compaction_from_archive(
    archive_path: &Path,
    manifest: &ArchiveManifest,
    snapshot: &CanonicalSessionSnapshot,
    cancel: &CancellationToken,
) -> Result<Option<PriorCompaction>> {
    check_cancel(cancel)?;
    if !is_single_path_component(&manifest.session.native_session_id) {
        return Ok(None);
    }
    let expected = transcript_path(&manifest.session.native_session_id);
    let descriptor = manifest.payloads.iter().find(|descriptor| {
        matches!(&descriptor.role, PayloadRole::NativeArtifact { relative_path }
            if *relative_path == expected)
    });
    let Some(descriptor) = descriptor else {
        return Ok(None);
    };
    let transcript = scan_transcript(cancel, |writer| {
        CheckpointPayloadReader::open(archive_path)?.copy_payload(descriptor, writer)
    })?;
    prior_compaction_from_transcript(transcript, snapshot, cancel)
}

pub(super) fn prior_compaction_from_home(
    harness_home: &Path,
    native_session_id: &str,
    snapshot: &CanonicalSessionSnapshot,
    cancel: &CancellationToken,
) -> Result<Option<PriorCompaction>> {
    check_cancel(cancel)?;
    if !is_single_path_component(native_session_id) {
        return Ok(None);
    }
    // The home is the session's own tree; refuse any link that could point out of it.
    let mut path = harness_home.to_path_buf();
    for directory in transcript_directories(native_session_id) {
        path.push(directory);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => return Ok(None),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        }
    }
    path.push(TRANSCRIPT_FILE);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_file() => {
            prior_compaction_from_reader(File::open(path)?, snapshot, cancel)
        }
        Ok(_) => Ok(None),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub(super) fn prior_compaction_from_reader(
    mut reader: impl Read,
    snapshot: &CanonicalSessionSnapshot,
    cancel: &CancellationToken,
) -> Result<Option<PriorCompaction>> {
    check_cancel(cancel)?;
    let transcript = scan_transcript(
        cancel,
        |writer| Ok(io::copy(&mut reader, writer).map(drop)?),
    )?;
    prior_compaction_from_transcript(transcript, snapshot, cancel)
}

#[derive(Default)]
struct AntigravityTranscript {
    replies: Vec<String>,
    checkpoints: Vec<NativeCheckpoint>,
}

#[derive(Deserialize)]
struct RawStep {
    #[serde(rename = "type")]
    kind: Option<String>,
    status: Option<String>,
    content: Option<String>,
    truncated_fields: Option<Vec<String>>,
}

impl AntigravityTranscript {
    fn parse_record(&mut self, line: &[u8]) -> serde_json::Result<()> {
        let step: RawStep = serde_json::from_slice(line)?;
        let Some(content) = step.content.as_deref().map(str::trim) else {
            return Ok(());
        };
        if content.is_empty() {
            return Ok(());
        }
        match step.kind.as_deref() {
            Some("PLANNER_RESPONSE") => self.replies.push(content.to_owned()),
            Some("CHECKPOINT")
                if step.status.as_deref() == Some("DONE")
                    && !step
                        .truncated_fields
                        .is_some_and(|fields| fields.iter().any(|field| field == "content")) =>
            {
                self.checkpoints.push(NativeCheckpoint {
                    summary: content.to_owned(),
                    before_reply: self.replies.len(),
                });
            }
            _ => {}
        }
        Ok(())
    }
}

fn scan_transcript(
    cancel: &CancellationToken,
    feed: impl FnOnce(&mut dyn Write) -> Result<()>,
) -> Result<AntigravityTranscript> {
    let mut transcript = AntigravityTranscript::default();
    let mut scanner = LineScanner::new(cancel, |line, line_number| {
        transcript
            .parse_record(line)
            .with_context(|| format!("parse Antigravity native transcript line {line_number}"))
            .map_err(io::Error::other)
    });
    feed(&mut scanner)?;
    scanner.finish()?;
    Ok(transcript)
}

fn prior_compaction_from_transcript(
    transcript: AntigravityTranscript,
    snapshot: &CanonicalSessionSnapshot,
    cancel: &CancellationToken,
) -> Result<Option<PriorCompaction>> {
    let lineage_start = lineage_start(snapshot);
    // Reply text identifies a canonical item only when it occurs once.
    let mut canonical_agents = HashMap::<String, Option<u64>>::new();
    for item in &snapshot.transcript {
        if let CanonicalTranscriptBody::Agent { chunks, .. } = &item.body
            && item.position > lineage_start
        {
            let text = mj_core::transcript::materialized_chunks_text(chunks)
                .trim()
                .to_owned();
            if !text.is_empty() {
                canonical_agents
                    .entry(text)
                    .and_modify(|position| *position = None)
                    .or_insert(Some(item.position));
            }
        }
    }
    // A reply anchors a checkpoint only when it occurs once in the native
    // transcript (unique before the checkpoint and absent after it) and matches
    // exactly one canonical agent item.
    let mut reply_counts = HashMap::<&str, usize>::new();
    for reply in &transcript.replies {
        *reply_counts.entry(reply.as_str()).or_default() += 1;
    }
    let mut latest_anchor = Vec::with_capacity(transcript.replies.len() + 1);
    let mut current_anchor = None;
    latest_anchor.push(current_anchor);
    for reply in &transcript.replies {
        if reply_counts[reply.as_str()] == 1
            && let Some(position) = canonical_agents.get(reply.as_str()).copied().flatten()
        {
            current_anchor = Some(position);
        }
        latest_anchor.push(current_anchor);
    }
    for checkpoint in transcript.checkpoints.into_iter().rev() {
        check_cancel(cancel)?;
        let Some(anchor_position) = latest_anchor[checkpoint.before_reply] else {
            continue;
        };
        let Some(cutoff) = anchor_position.checked_sub(1) else {
            continue;
        };
        if cutoff <= lineage_start {
            continue;
        }
        let retained_positions =
            collect_retained_positions(snapshot, lineage_start, anchor_position, |call| {
                matches!(
                    call.get("status").and_then(Value::as_str),
                    Some("completed" | "failed")
                )
            });
        check_cancel(cancel)?;
        return Ok(Some(PriorCompaction {
            through_position: cutoff,
            summary: checkpoint.summary,
            retained_positions,
        }));
    }
    check_cancel(cancel)?;
    Ok(None)
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
            pending_prompt_context: None,
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
            mailbox: None,
        }
    }

    fn user(text: &str) -> CanonicalTranscriptBody {
        CanonicalTranscriptBody::User {
            content: vec![json!({"type":"text","text":text})],
        }
    }

    fn agent(text: &str) -> CanonicalTranscriptBody {
        CanonicalTranscriptBody::Agent {
            chunks: vec![json!({"content":{"type":"text","text":text}})],
            streaming: false,
        }
    }

    fn tool(id: &str, status: &str) -> CanonicalTranscriptBody {
        CanonicalTranscriptBody::Tool {
            call: json!({"toolCallId":id,"title":"Run","kind":"execute","status":status}),
            terminal_outputs: Vec::new(),
            terminal_refs: Vec::new(),
            presentation: None,
        }
    }

    fn reply(text: &str) -> Value {
        json!({"type":"PLANNER_RESPONSE","status":"DONE","content":text,"thinking":"x"})
    }

    fn checkpoint(summary: &str) -> Value {
        json!({"type":"CHECKPOINT","status":"DONE","content":summary})
    }

    fn jsonl(lines: &[Value]) -> String {
        lines
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn prior_for(lines: &[Value], canonical: &CanonicalSessionSnapshot) -> Option<PriorCompaction> {
        prior_compaction_from_reader(
            Cursor::new(jsonl(lines)),
            canonical,
            &CancellationToken::new(),
        )
        .unwrap()
    }

    #[test]
    fn checkpoint_anchors_to_the_unique_reply_and_retains_unsummarized_history() {
        let mut canonical = snapshot(vec![
            user("work"),
            agent("first answer"),
            tool("done", "completed"),
            tool("running", "in_progress"),
            agent("late answer"),
            CanonicalTranscriptBody::Plan {
                plan: json!({"entries":[]}),
            },
            CanonicalTranscriptBody::Plan {
                plan: json!({"entries":[]}),
            },
            agent("anchor answer"),
            user("more"),
            agent("after"),
        ]);
        canonical.transcript[1].latest_content_event_ordinal = Some(2);
        canonical.transcript[4].latest_content_event_ordinal = Some(20);
        canonical.transcript[7].latest_content_event_ordinal = Some(8);
        canonical.transcript[9].latest_content_event_ordinal = Some(10);
        let lines = [
            reply("first answer"),
            reply("late answer"),
            reply("anchor answer"),
            checkpoint("older summary"),
            reply("after"),
            checkpoint("  newest valid summary  "),
            json!({"type":"CHECKPOINT","status":"DONE","content":"cut off","truncated_fields":["content"]}),
            json!({"type":"CHECKPOINT","status":"RUNNING","content":"unfinished"}),
            json!({"type":"CHECKPOINT","status":"DONE","content":"  "}),
        ];
        let prior = prior_for(&lines, &canonical).unwrap();
        assert_eq!(prior.summary, "newest valid summary");
        assert_eq!(prior.through_position, 9);
        assert_eq!(prior.retained_positions, vec![4, 5, 7]);
    }

    #[test]
    fn ambiguous_or_recurring_replies_are_not_anchors() {
        let canonical = snapshot(vec![
            user("work"),
            agent("once"),
            agent("twice"),
            agent("twice"),
        ]);
        // The reply occurs twice before the checkpoint.
        assert!(
            prior_for(
                &[reply("twice"), reply("twice"), checkpoint("summary")],
                &canonical
            )
            .is_none()
        );
        // The reply is not unique in canonical history.
        assert!(prior_for(&[reply("twice"), checkpoint("summary")], &canonical).is_none());
        // The reply recurs after the checkpoint.
        assert!(
            prior_for(
                &[reply("once"), checkpoint("summary"), reply("once")],
                &canonical
            )
            .is_none()
        );
        assert!(prior_for(&[reply("once"), checkpoint("summary")], &canonical).is_some());
    }

    #[test]
    fn clear_and_generic_handoff_bound_the_anchor_search() {
        let mut cleared = snapshot(vec![
            user("work"),
            agent("repeated"),
            agent("before clear"),
            CanonicalTranscriptBody::System {
                text: "context cleared".into(),
            },
            user("again"),
            agent("repeated"),
        ]);
        cleared.transcript[3].stable_id = "context-cleared:4".into();
        // Only the post-clear copy counts, so the reply is unique.
        let prior = prior_for(&[reply("repeated"), checkpoint("summary")], &cleared).unwrap();
        assert_eq!(prior.through_position, 5);
        assert!(prior_for(&[reply("before clear"), checkpoint("summary")], &cleared).is_none());

        let mut before_handoff = snapshot(vec![user("old"), agent("old answer")]);
        before_handoff.event_frontier = 2;
        let state = HandoffState::capture(&before_handoff, "generic earlier state".into()).unwrap();
        let mut canonical = snapshot(vec![user("old"), agent("old answer"), user("new")]);
        let lines = [reply("old answer"), checkpoint("summary")];
        assert!(prior_for(&lines, &canonical).is_some());
        canonical.session.configuration.values.insert(
            HANDOFF_STATE_KEY.into(),
            serde_json::to_value(state).unwrap(),
        );
        assert!(prior_for(&lines, &canonical).is_none());
    }

    #[tokio::test]
    async fn verified_archive_supplies_the_checkpoint_to_the_compaction_prompt() {
        use mj_checkpoint::archive::{
            ArchiveInput, BundleManifest, NativeArtifact, SessionManifest, TargetManifest,
            verify_archive_streaming, write_archive_atomic,
        };
        let mut canonical = snapshot(vec![
            user("work"),
            agent("old agent text"),
            tool("running", "in_progress"),
            agent("anchor answer"),
            user("more"),
        ]);
        canonical.transcript[1].latest_content_event_ordinal = Some(2);
        canonical.transcript[3].latest_content_event_ordinal = Some(4);
        let full = jsonl(&[
            reply("old agent text"),
            reply("anchor answer"),
            checkpoint("native prior state"),
        ]);
        let truncated = jsonl(&[
            reply("old agent text"),
            reply("anchor answer"),
            json!({"type":"CHECKPOINT","status":"DONE","content":"cut","truncated_fields":["content"]}),
        ]);
        let native_path = |name: &str| {
            format!("antigravity-cli/brain/native-1/.system_generated/logs/{name}.jsonl")
        };
        let input = ArchiveInput {
            session: SessionManifest {
                id: "session-1".into(),
                title: "Work".into(),
                harness_kind: HarnessKind::Antigravity,
                profile_id: "antigravity".into(),
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
            native_artifacts: vec![
                NativeArtifact {
                    relative_path: native_path("transcript_full").into(),
                    data: full.into_bytes().into(),
                    mode: 0o600,
                },
                NativeArtifact {
                    relative_path: native_path("transcript").into(),
                    data: truncated.into_bytes().into(),
                    mode: 0o600,
                },
            ],
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
        assert_eq!(prior.summary, "native prior state");
        assert_eq!(prior.through_position, 3);
        assert_eq!(prior.retained_positions, vec![3]);
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
        assert!(prompts[0].contains("running"));
        assert!(!prompts[0].contains("old agent text"));
    }

    #[test]
    fn home_search_reads_only_the_regular_full_transcript() {
        let home = tempfile::tempdir().unwrap();
        let canonical = snapshot(vec![user("work"), agent("answer")]);
        let cancel = CancellationToken::new();
        let read_prior =
            || prior_compaction_from_home(home.path(), "native-1", &canonical, &cancel);
        let logs = home
            .path()
            .join("antigravity-cli/brain/native-1/.system_generated/logs");
        let file = logs.join(TRANSCRIPT_FILE);
        let body = jsonl(&[reply("answer"), checkpoint("home summary")]);

        assert!(read_prior().unwrap().is_none());
        fs::create_dir_all(&logs).unwrap();
        assert!(read_prior().unwrap().is_none());
        // The truncating transcript is never read.
        fs::write(logs.join("transcript.jsonl"), &body).unwrap();
        assert!(read_prior().unwrap().is_none());
        fs::write(&file, &body).unwrap();
        assert_eq!(read_prior().unwrap().unwrap().summary, "home summary");
        assert!(
            prior_compaction_from_home(home.path(), "../native-1", &canonical, &cancel)
                .unwrap()
                .is_none()
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let outside = tempfile::tempdir().unwrap();
            // A linked file.
            let target = outside.path().join("elsewhere.jsonl");
            fs::write(&target, &body).unwrap();
            fs::remove_file(&file).unwrap();
            symlink(&target, &file).unwrap();
            assert!(read_prior().unwrap().is_none());
            fs::remove_file(&file).unwrap();
            fs::write(&file, &body).unwrap();

            // Each linked directory level.
            let brain = home.path().join("antigravity-cli/brain");
            let session = brain.join("native-1");
            let moved = outside.path().join("native-1");
            fs::rename(&session, &moved).unwrap();
            symlink(&moved, &session).unwrap();
            assert!(read_prior().unwrap().is_none());
            fs::remove_file(&session).unwrap();
            fs::rename(&moved, &session).unwrap();

            let generated = session.join(".system_generated");
            let moved = outside.path().join(".system_generated");
            fs::rename(&generated, &moved).unwrap();
            symlink(&moved, &generated).unwrap();
            assert!(read_prior().unwrap().is_none());
            fs::remove_file(&generated).unwrap();
            fs::rename(&moved, &generated).unwrap();
            assert!(read_prior().unwrap().is_some());
        }
    }

    #[test]
    fn parser_observes_cancellation_before_and_during_a_large_line() {
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

        let body = format!("{}\n", reply(&"x".repeat(70 * 1024)));
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
}
