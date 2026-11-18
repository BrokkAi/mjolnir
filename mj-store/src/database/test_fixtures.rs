//! Records that tests of the store, and of the code above it, start from.

use super::*;
use mj_core::state::QueuedCommandKind;

pub fn event_digest(value: u64) -> String {
    format!("{value:064x}")
}

pub fn materialized_session(session_id: &str) -> MaterializedSession {
    MaterializedSession {
        active_turn: None,
        last_turn_outcome: None,
        session_id: session_id.into(),
        applied_event_ordinal: 7,
        applied_event_digest: event_digest(7),
        last_activity_at_ms: Some(1_500),
        execution: MaterializedExecutionState::Running {
            started_at_ms: 1_000,
        },
        session_title: Some("Relay refactor".into()),
        configuration: mj_core::state::SessionConfiguration {
            values: BTreeMap::from([
                ("model".into(), serde_json::json!("gpt-5.6-sol")),
                ("effort".into(), serde_json::json!("high")),
            ]),
            ..Default::default()
        },
        transcript: vec![
            Arc::new(TranscriptItem {
                stable_id: "user:1".into(),
                position: 1,
                latest_content_event_ordinal: None,
                created_at_ms: 1_000,
                last_changed_at_ms: 1_000,
                body: TranscriptBody::User {
                    content: vec![serde_json::json!({
                        "type": "text",
                        "text": "build it"
                    })],
                },
            }),
            Arc::new(TranscriptItem {
                stable_id: "agent:2".into(),
                position: 2,
                latest_content_event_ordinal: Some(2),
                created_at_ms: 1_100,
                last_changed_at_ms: 1_300,
                body: TranscriptBody::Agent {
                    chunks: vec![serde_json::json!({
                        "content": {"type": "text", "text": "Working on it"},
                        "messageId": "answer-1",
                        "_meta": {"provider": "test"}
                    })],
                    streaming: false,
                },
            }),
            Arc::new(TranscriptItem {
                stable_id: "tool:call-1".into(),
                position: 3,
                latest_content_event_ordinal: None,
                created_at_ms: 1_200,
                last_changed_at_ms: 1_400,
                body: TranscriptBody::Tool {
                    call: (serde_json::json!({
                        "toolCallId": "call-1",
                        "title": "Edit files",
                        "kind": "edit",
                        "status": "completed",
                        "content": [{
                            "type": "content",
                            "content": {"type": "text", "text": "done"}
                        }],
                        "locations": [{"path": "src/main.rs", "line": 4}],
                        "rawInput": {"path": "src/main.rs"},
                        "rawOutput": {"changed": true},
                        "_meta": {"provider": "test"}
                    }))
                    .into(),
                    terminal_outputs: Vec::new(),
                    terminal_refs: Vec::new(),
                    presentation: None,
                },
            }),
            Arc::new(TranscriptItem {
                stable_id: "plan:1".into(),
                position: 4,
                latest_content_event_ordinal: None,
                created_at_ms: 1_250,
                last_changed_at_ms: 1_350,
                body: TranscriptBody::Plan {
                    plan: serde_json::json!({
                        "entries": [{
                            "content": "Implement relay",
                            "priority": "high",
                            "status": "in_progress",
                            "_meta": {"provider": "test"}
                        }],
                        "_meta": {"planProvider": "test"}
                    }),
                },
            }),
        ],
        queued_prompts: vec![MaterializedQueuedPrompt {
            accepted_ordinal: None,
            command_id: "prompt-2".into(),
            kind: QueuedCommandKind::Prompt,
            content: vec![serde_json::json!({"type": "text", "text": "then test"})],
            queued_at_ms: 1_500,
        }],
        pending_elicitations: vec![mj_core::elicitation::ElicitationRequest {
            id: "elicitation-1".into(),
            message: "Choose one".into(),
            title: Some("Question".into()),
            description: None,
            fields: Vec::new(),
            tool_permission: None,
        }],
    }
}

pub fn checkpoint_test_session(session_id: &str) -> SessionRecord {
    SessionRecord {
        oracle_choice: None,
        oracle: true,
        notifications_ignored: false,
        project: None,
        target_runtime: None,
        launch_base: None,
        launch_branch: None,
        checkout: None,
        publication: None,
        build_cache: None,
        container_workspace: None,
        subagents: None,
        create_managed_worktree: None,
        workspace_id: mj_core::workspace::DEFAULT_WORKSPACE_ID.to_owned(),
        archived: false,
        id: session_id.into(),
        title: "checkpoint transition".into(),
        harness_kind: mj_core::config::HarnessKind::Codex,
        last_profile: "codex".into(),
        bundle_id: "project".into(),
        project_directory: None,
        managed_worktree: None,
        review: None,
        no_project_memory: false,
        no_github_events: false,
        no_github_credentials: false,
        target_template_id: "podman".into(),
        resource_allocation: None,
        additional_mounts: Vec::new(),
        state: SessionState::Running,
        target: None,
        native_session_id: Some("native-session".into()),
        acp_session_title: None,
        session_title_override: None,
        created_at: "2026-08-12T00:00:00Z".into(),
        updated_at: "2026-08-12T00:00:00Z".into(),
        viewed_through_event_ordinal: 0,
        draft_input: String::new(),
        last_error: None,
        dismissed_failure: None,
        failure_generation: 0,
        last_checkpoint_error: None,
        checkpoint: None,
    }
}

/// A bundle of one local repository.
pub fn local_bundle(repository: &std::path::Path) -> mj_core::config::ProjectBundle {
    mj_core::config::ProjectBundle {
        primary_repo: "project".into(),
        repositories: vec![mj_core::config::ProjectRepository {
            id: "project".into(),
            github: None,
            local: Some(repository.to_path_buf()),
            destination: std::path::PathBuf::from("project"),
            git_ref: None,
        }],
    }
}

/// The recovery gate tests share for one data directory.
pub fn worker_test_gate() -> std::sync::Arc<crate::recovery_gate::RecoveryGate> {
    use std::sync::{Arc, Mutex, OnceLock};
    static GATES: OnceLock<
        Mutex<
            std::collections::BTreeMap<std::path::PathBuf, Arc<crate::recovery_gate::RecoveryGate>>,
        >,
    > = OnceLock::new();
    GATES
        .get_or_init(Mutex::default)
        .lock()
        .unwrap()
        .entry(mj_core::config::data_dir())
        .or_default()
        .clone()
}

/// A worker permit for a test that stages or starts a worker without a daemon.
pub fn worker_test_permit(
    id: &str,
    _reason: &'static str,
) -> anyhow::Result<Option<crate::worker_lifecycle::WorkerPermit>> {
    if let Some(owner) = crate::worker_lifecycle::current(id) {
        return Ok(Some(owner));
    }
    let Ok(admission) =
        worker_test_gate().try_start(id, crate::recovery_gate::BackgroundWork::WorkerReplacement)
    else {
        return Ok(None);
    };
    crate::worker_lifecycle::WorkerPermit::acquire(id, &admission).map(Some)
}
