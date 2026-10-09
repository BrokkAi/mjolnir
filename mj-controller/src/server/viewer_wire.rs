//! Compact browser-only projection of the complete in-process viewer snapshot.
use super::{ViewerLifecycleCategory, ViewerSession, ViewerSessions, ViewerSnapshot};
use anyhow::{Context, Result};
use mj_client::runtime_feed::RuntimeCursor;
use serde::Serialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

const INTERNED_FIELDS: [&str; 5] = [
    "available_commands",
    "capabilities",
    "config_options",
    "compatible_resume_targets",
    "incompatible_resume_targets",
];

pub(super) const METADATA_FIELDS: [&str; 13] = [
    "profile_capabilities",
    "last_subagent_policy",
    "revision",
    "generated_at",
    "server_time_ms",
    "server_version",
    "workspaces",
    "profiles",
    "targets",
    "bundles",
    "review_config",
    "capacity",
    "launch_failures",
];

/// Fixed-size digests avoid retaining serialized metadata for each history
/// revision. `None` records fields omitted by their wire `skip_serializing_if`.
#[derive(Clone, Copy)]
pub(super) struct MetadataDigests(pub(super) [Option<[u8; 32]>; METADATA_FIELDS.len()]);

/// Summary rows retain exactly these serialized session fields: `id`,
/// `workspace_id`, `bundle_id`, `title`, `subagent_parent_id`,
/// `subagent_session_ids`, `publication_state`, `profile_id`, `target_id`,
/// `state`, `created_at`, `updated_at`, `has_error`, `configuration_issue`,
/// `launch_error`, `storage_problem`, `project_label`, `display_location`,
/// `lifecycle`, `transitioning`, `last_activity_at_ms`, `last_message_at_ms`,
/// `capacity_retry`, `retry_assessment_pending`, `quota_recovery`, `operation`,
/// `move_recovery`, `chat_phase`, `is_idle`, `activity_details`, `activity`,
/// and `capabilities`. Prompt and elicitation bodies become
/// `queued_prompt_count` and `pending_elicitation_count`. These are the
/// dashboard, resume-list, session-index, and sub-agent workspace/count fields
/// read by viewer.js; resume detail, Move, and conversation fields are fetched
/// from the row route.
const SUMMARY_FIELDS: &[&str] = &[
    "id",
    "workspace_id",
    "bundle_id",
    "title",
    "subagent_parent_id",
    "subagent_session_ids",
    "publication_state",
    "profile_id",
    "target_id",
    "state",
    "created_at",
    "updated_at",
    "has_error",
    "configuration_issue",
    "launch_error",
    "storage_problem",
    "project_label",
    "display_location",
    "lifecycle",
    "transitioning",
    "last_activity_at_ms",
    "last_message_at_ms",
    "capacity_retry",
    "retry_assessment_pending",
    "quota_recovery",
    "operation",
    "move_recovery",
    "chat_phase",
    "is_idle",
    "activity_details",
    "activity",
    "capabilities",
];

#[derive(Debug)]
pub(crate) struct WireRow {
    pub(crate) row: Value,
    pub(crate) interned: BTreeMap<String, Value>,
}

#[derive(Serialize)]
struct SnapshotMetadata<'a> {
    profile_capabilities: &'a mj_core::profile_capabilities::ProfileCapabilitiesSnapshot,
    last_subagent_policy: &'a mj_core::subagent::SubagentPolicy,
    revision: u64,
    generated_at: &'a str,
    server_time_ms: i64,
    #[serde(skip_serializing_if = "str::is_empty")]
    server_version: &'a str,
    #[serde(skip_serializing_if = "slice_is_empty")]
    workspaces: &'a [super::ViewerWorkspace],
    profiles: &'a [super::ViewerProfile],
    targets: &'a [super::ViewerTarget],
    bundles: &'a [super::ViewerBundle],
    review_config: &'a super::ViewerReviewConfig,
    #[serde(skip_serializing_if = "slice_is_empty")]
    capacity: &'a [super::ViewerTargetCapacity],
    #[serde(skip_serializing_if = "slice_is_empty")]
    launch_failures: &'a [super::ViewerLaunchFailure],
}

fn slice_is_empty<T>(values: &&[T]) -> bool {
    values.is_empty()
}

pub(super) fn snapshot(
    snapshot: &ViewerSnapshot,
    cursor: &RuntimeCursor,
    server_time_ms: i64,
) -> Result<Value> {
    let mut top = metadata(snapshot, server_time_ms)?;
    let mut rows = Vec::with_capacity(snapshot.sessions.len());
    let mut interned = BTreeMap::new();
    for session in snapshot.sessions.iter() {
        let wire = row(session)?;
        rows.push(wire.row);
        interned.extend(wire.interned);
    }
    top.insert("sessions".into(), Value::Array(rows));
    top.insert("cursor".into(), serde_json::to_value(cursor)?);
    top.insert("interned".into(), serde_json::to_value(interned)?);
    Ok(Value::Object(top))
}

pub(super) fn metadata(
    snapshot: &ViewerSnapshot,
    server_time_ms: i64,
) -> Result<Map<String, Value>> {
    let metadata = SnapshotMetadata {
        profile_capabilities: &snapshot.profile_capabilities,
        last_subagent_policy: &snapshot.last_subagent_policy,
        revision: snapshot.revision,
        generated_at: &snapshot.generated_at,
        server_time_ms,
        server_version: &snapshot.server_version,
        workspaces: &snapshot.workspaces,
        profiles: &snapshot.profiles,
        targets: &snapshot.targets,
        bundles: &snapshot.bundles,
        review_config: &snapshot.review_config,
        capacity: &snapshot.capacity,
        launch_failures: &snapshot.launch_failures,
    };
    serde_json::to_value(metadata)?
        .as_object()
        .cloned()
        .context("viewer metadata must serialize as an object")
}

pub(super) fn metadata_digests(snapshot: &ViewerSnapshot) -> Result<MetadataDigests> {
    let values = metadata(snapshot, snapshot.server_time_ms)?;
    let mut digests = [None; METADATA_FIELDS.len()];
    for (field, value) in &values {
        let index = METADATA_FIELDS
            .iter()
            .position(|known| *known == field.as_str())
            .context("viewer metadata field has no history digest slot")?;
        let bytes = serde_json::to_vec(value).context("serialize viewer metadata field")?;
        digests[index] = Some(Sha256::digest(bytes).into());
    }
    Ok(MetadataDigests(digests))
}

pub(crate) fn row(session: &ViewerSession) -> Result<WireRow> {
    project_row(session, is_summary(session))
}

fn project_row(session: &ViewerSession, summary: bool) -> Result<WireRow> {
    let mut row = serde_json::to_value(session)?;
    let object = row
        .as_object_mut()
        .context("viewer session must serialize as an object")?;
    if summary {
        object.retain(|field, _| SUMMARY_FIELDS.contains(&field.as_str()));
        object.insert("detail".into(), Value::Bool(false));
        object.insert(
            "queued_prompt_count".into(),
            Value::from(session.queued_prompts.len()),
        );
        object.insert(
            "pending_elicitation_count".into(),
            Value::from(session.pending_elicitations.len()),
        );
    }
    if let Some(Value::Array(agents)) = object.get_mut("native_subagents") {
        for agent in agents {
            if let Some(agent) = agent.as_object_mut() {
                agent.remove("task");
            }
        }
    }

    let mut interned = BTreeMap::new();
    for field in INTERNED_FIELDS {
        let Some(value) = object.remove(field) else {
            continue;
        };
        if is_empty_default(&value) {
            object.insert(field.into(), value);
            continue;
        }
        let key = intern_key(&value)?;
        object.insert(format!("{field}_ref"), Value::String(key.clone()));
        interned.insert(key, value);
    }
    Ok(WireRow { row, interned })
}

pub(super) fn detail_row(session: &ViewerSession) -> Result<Value> {
    let WireRow { mut row, interned } = project_row(session, false)?;
    let object = row
        .as_object_mut()
        .context("viewer session must serialize as an object")?;
    for field in INTERNED_FIELDS {
        if let Some(Value::String(key)) = object.remove(&format!("{field}_ref")) {
            let value = interned
                .get(&key)
                .context("detail row refers to a missing interned value")?;
            object.insert(field.into(), value.clone());
        }
    }
    Ok(row)
}

pub(super) fn interned_keys_from_sessions(
    sessions: &ViewerSessions,
) -> Result<std::collections::BTreeSet<String>> {
    let mut keys = std::collections::BTreeSet::new();
    for session in sessions.iter() {
        keys.extend(row(session)?.interned.into_keys());
    }
    Ok(keys)
}

pub(super) fn is_summary(session: &ViewerSession) -> bool {
    // Match viewer.js isDashboardSession: operation-owned transitions remain
    // detail rows, except a plain checkpoint, which leaves the conversation
    // visible. Starting and suspending lifecycle categories are never settled.
    let transitioning = session.transitioning
        || session
            .operation
            .as_ref()
            .is_some_and(|operation| operation.kind != super::ViewerOperationKind::Checkpoint);
    session.lifecycle == ViewerLifecycleCategory::Suspended
        && !transitioning
        && !session.capabilities.open
}

fn is_empty_default(value: &Value) -> bool {
    match value {
        Value::Array(values) => values.is_empty(),
        Value::Object(values) => values.is_empty(),
        Value::Null => true,
        _ => false,
    }
}

fn intern_key(value: &Value) -> Result<String> {
    // serde_json::Map stores object keys in lexical order in this workspace,
    // making this serialization canonical while preserving array order.
    let bytes = serde_json::to_vec(value).context("serialize viewer interned value")?;
    let digest = Sha256::digest(bytes);
    let key = mj_core::hex::lower_hex(digest);
    Ok(key[..16].to_owned())
}
