//! Shared fixtures for the dashboard unit tests.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use hel::hel_config::{
    CONFIG_VERSION, ContainerTemplate, HarnessKind, HarnessProfile, HelConfig, ProjectBundle,
    ProjectRepository, TargetTemplate,
};
use hel::hel_state::{
    CheckpointMetadata, HelState, MaterializedExecutionState, MaterializedSession, STATE_VERSION,
    SessionRecord, SessionState, TranscriptBody, TranscriptItem,
};
use hel::hel_targets::{DeploymentCapacityKind, DeploymentCapacityTarget, ProvisionStage};

use crate::ingest::SessionOperationDisplay;
use crate::render::SUMMARY_RULE;
use crate::{DashboardState, SessionOperationKind};

pub(crate) fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// The drawn buffer as one string per row.
pub(crate) fn buffer_lines(buffer: &ratatui::buffer::Buffer) -> Vec<String> {
    (buffer.area.y..buffer.area.bottom())
        .map(|y| {
            (buffer.area.x..buffer.area.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect()
}

/// Column of `needle` within a drawn row, counted in cells rather than bytes.
pub(crate) fn cell_column(line: &str, needle: &str) -> u16 {
    let byte = line
        .find(needle)
        .unwrap_or_else(|| panic!("missing {needle} in {line:?}"));
    line[..byte].chars().count() as u16
}

/// A drawn summary cell that holds session text rather than the rule fill.
pub(crate) fn summary_text_cell(cell: &ratatui::buffer::Cell) -> bool {
    let symbol = cell.symbol();
    !symbol.trim().is_empty() && symbol != SUMMARY_RULE
}

pub(crate) fn ctrl_key(character: char) -> KeyEvent {
    KeyEvent::new(
        KeyCode::Char(character),
        if cfg!(target_os = "macos") {
            KeyModifiers::SUPER
        } else {
            KeyModifiers::CONTROL
        },
    )
}

/// An Alt chord. Alt is the same modifier everywhere, so this must not go
/// through [`ctrl_key`], which reports SUPER on macOS.
pub(crate) fn alt_key(character: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(character), KeyModifiers::ALT)
}

pub(crate) fn mouse_in(kind: MouseEventKind, area: Rect) -> MouseEvent {
    MouseEvent {
        kind,
        column: area.x.saturating_add(1),
        row: area.y.saturating_add(1),
        modifiers: KeyModifiers::NONE,
    }
}

/// Like `mouse_in`, but clicks a specific row within `area` instead of
/// always the top-left cell, so tests can hit tail lines further down a
/// multi-line hitbox.
pub(crate) fn mouse_at_row(kind: MouseEventKind, area: Rect, row_offset: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column: area.x.saturating_add(1),
        row: area.y.saturating_add(row_offset),
        modifiers: KeyModifiers::NONE,
    }
}

pub(crate) fn config() -> HelConfig {
    HelConfig {
        version: CONFIG_VERSION,
        newer_config_version: None,
        phone: Default::default(),
        review: Default::default(),
        profiles: BTreeMap::from([
            (
                "claude-1".into(),
                HarnessProfile {
                    context_window_bytes: None,
                    kind: HarnessKind::Claude,
                    home: PathBuf::from("/profiles/claude"),
                    executable: None,
                    environment: BTreeMap::new(),
                },
            ),
            (
                "codex-1".into(),
                HarnessProfile {
                    context_window_bytes: None,
                    kind: HarnessKind::Codex,
                    home: PathBuf::from("/profiles/codex"),
                    executable: None,
                    environment: BTreeMap::new(),
                },
            ),
            (
                "codex-2".into(),
                HarnessProfile {
                    context_window_bytes: None,
                    kind: HarnessKind::Codex,
                    home: PathBuf::from("/profiles/codex-two"),
                    executable: None,
                    environment: BTreeMap::new(),
                },
            ),
        ]),
        bundles: BTreeMap::from([(
            "hel".into(),
            ProjectBundle {
                primary_repo: "hel".into(),
                repositories: vec![ProjectRepository {
                    id: "hel".into(),
                    github: Some("BrokkAi/hel".into()),
                    local: None,
                    destination: PathBuf::from("hel"),
                    git_ref: None,
                }],
            },
        )]),
        targets: BTreeMap::from([(
            "podman".into(),
            TargetTemplate::LocalPodman {
                container: ContainerTemplate {
                    image: "ubuntu:24.04".into(),
                    pull_policy: Default::default(),
                    platform: None,
                    cpus: None,
                    memory: None,
                    environment: BTreeMap::new(),
                    workspace_storage: Default::default(),
                },
            },
        )]),
    }
}

pub(crate) fn stopped_session() -> SessionRecord {
    SessionRecord {
        workspace_id: hel::hel_workspace::DEFAULT_WORKSPACE_ID.to_owned(),
        archived: false,
        container_cpus: None,
        container_memory: None,
        id: "session-1".into(),
        title: "Raise the dead".into(),
        harness_kind: HarnessKind::Codex,
        last_profile: "codex-1".into(),
        bundle_id: "hel".into(),
        project_directory: None,
        managed_worktree: None,
        target_template_id: "podman".into(),
        resource_allocation: None,
        additional_mounts: vec![],
        state: SessionState::Stopped,
        target: None,
        native_session_id: Some("native-1".into()),
        acp_session_title: Some("ACP pretty name".into()),
        session_title_override: None,
        created_at: "2026-08-09T00:00:00Z".into(),
        updated_at: "2026-08-09T01:00:00Z".into(),
        viewed_through_event_ordinal: 0,
        draft_input: String::new(),
        last_error: None,
        last_checkpoint_error: None,
        checkpoint: Some(CheckpointMetadata {
            archive_path: PathBuf::from("sessions/session-1.hel.zip"),
            sha256: "a".repeat(64),
            created_at: "2026-08-09T01:00:00Z".into(),
            event_frontier: 2,
        }),
    }
}

/// The same fixture as [`stopped_session`], but live. Dashboard interactions
/// such as rename and the container editor only apply to a session that is
/// actually on the dashboard.
pub(crate) fn running_session() -> SessionRecord {
    SessionRecord {
        state: SessionState::Running,
        ..stopped_session()
    }
}

/// Reaches the resume wizard the way the UI does: open the resume dialog with
/// no native scan results, then activate the first row, which is the session's
/// own stopped record.
pub(crate) fn open_resume_wizard(dashboard: &mut DashboardState) -> crate::DashboardAction {
    dashboard.show_resume_dialog(1, Vec::new());
    dashboard.handle_key(key(KeyCode::Enter))
}

pub(crate) fn dashboard_with_session(mut session: SessionRecord) -> DashboardState {
    session.updated_at = "2026-08-09T01:00:00Z".into();
    DashboardState::new(
        config(),
        HelState {
            version: STATE_VERSION,
            sessions: BTreeMap::from([(session.id.clone(), session)]),
            mount_history: BTreeMap::new(),
            container_sizes: BTreeMap::new(),
        },
        BTreeMap::new(),
    )
}

pub(crate) fn test_capacity_target() -> DeploymentCapacityTarget {
    DeploymentCapacityTarget {
        id: "local".into(),
        host: "local".into(),
        target_ids: vec!["podman".into()],
        kind: DeploymentCapacityKind::Host,
        local: true,
        probes: Vec::new(),
        probe_error: None,
    }
}

pub(crate) fn transcript_item(position: u64, body: TranscriptBody) -> Arc<TranscriptItem> {
    let at_ms = i64::try_from(position).unwrap() * 1_000;
    let latest_content_event_ordinal =
        matches!(&body, TranscriptBody::Agent { .. }).then_some(position);
    Arc::new(TranscriptItem {
        stable_id: format!("item-{position}"),
        position,
        latest_content_event_ordinal,
        created_at_ms: at_ms,
        last_changed_at_ms: at_ms,
        body,
    })
}

pub(crate) fn agent_message(position: u64, text: impl Into<String>) -> Arc<TranscriptItem> {
    transcript_item(
        position,
        TranscriptBody::Agent {
            chunks: vec![serde_json::json!({
                "content": {"type": "text", "text": text.into()}
            })],
            streaming: false,
        },
    )
}

pub(crate) fn thought(position: u64, text: impl Into<String>) -> Arc<TranscriptItem> {
    transcript_item(
        position,
        TranscriptBody::Thought {
            chunks: vec![serde_json::json!({
                "content": {"type": "text", "text": text.into()}
            })],
            streaming: false,
        },
    )
}

pub(crate) fn session_restart(position: u64) -> Arc<TranscriptItem> {
    let mut item = transcript_item(
        position,
        TranscriptBody::System {
            text: hel::hel_transcript::SESSION_RESTART_TEXT.into(),
        },
    );
    Arc::make_mut(&mut item).stable_id = format!(
        "{}{}",
        hel::hel_transcript::SESSION_RESTART_ITEM_PREFIX,
        position
    );
    item
}

pub(crate) fn materialized_session_for(
    session_id: &str,
    transcript: Vec<Arc<TranscriptItem>>,
) -> MaterializedSession {
    let frontier = transcript
        .iter()
        .map(|item| item.position)
        .max()
        .unwrap_or(0);
    let mut session = MaterializedSession::empty(session_id);
    session.applied_event_ordinal = frontier;
    if frontier > 0 {
        session.applied_event_digest = "a".repeat(64);
    }
    session.execution = MaterializedExecutionState::Running {
        started_at_ms: 100_000,
    };
    session.last_activity_at_ms = transcript
        .iter()
        .map(|item| item.last_changed_at_ms)
        .max()
        .or(Some(100_000));
    session.transcript = transcript;
    session
}

pub(crate) fn apply_materialized_transcript(
    dashboard: &mut DashboardState,
    transcript: Vec<Arc<TranscriptItem>>,
) {
    apply_materialized_transcript_for(dashboard, "session-1", transcript);
}

pub(crate) fn apply_materialized_transcript_for(
    dashboard: &mut DashboardState,
    session_id: &str,
    transcript: Vec<Arc<TranscriptItem>>,
) {
    dashboard.apply_materialized_session(&materialized_session_for(session_id, transcript));
}

/// A conversation of `count` numbered exchanges, so preview scroll
/// assertions can name the message they expect to see. Agent chunks
/// coalesce unless separated, so each pairs with its own prompt.
pub(crate) fn numbered_conversation(count: u64) -> Vec<Arc<TranscriptItem>> {
    (0..count)
        .flat_map(|index| {
            [
                transcript_item(
                    index * 2 + 1,
                    TranscriptBody::User {
                        content: vec![serde_json::json!({
                            "type": "text",
                            "text": format!("question {index}"),
                        })],
                    },
                ),
                agent_message(index * 2 + 2, format!("answer {index}")),
            ]
        })
        .collect()
}

pub(crate) fn operation(
    kind: SessionOperationKind,
    stage: Option<ProvisionStage>,
) -> SessionOperationDisplay {
    let active_stages = stage
        .map(|stage| [(stage, 1_000)].into_iter().collect())
        .unwrap_or_default();
    SessionOperationDisplay {
        kind,
        started_at_epoch_seconds: 1_000,
        placeholder: None,
        active_stages,
        resume_destination: None,
    }
}
