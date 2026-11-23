use std::path::Path;

use anyhow::{Context, Result};
use mj_core::transcript::{ToolCallDetailResult, TranscriptBody};
use mj_store::database;
use mj_store::managed_archive::managed_checkpoint_archive_name;
use mj_transcript::projection::materialized_transcript_item_from_canonical;

pub fn load_tool_call_detail_with_checkpoint_fallback(
    session_id: &str,
    stable_id: &str,
) -> Result<Option<ToolCallDetailResult>> {
    load_tool_call_detail_with_checkpoint_fallback_from(
        &database::database_path(),
        &mj_core::config::sessions_dir(),
        session_id,
        stable_id,
    )
}

pub fn load_tool_call_detail_with_checkpoint_fallback_from(
    db_path: &Path,
    sessions_dir: &Path,
    session_id: &str,
    stable_id: &str,
) -> Result<Option<ToolCallDetailResult>> {
    let local = database::load_tool_call_detail_from(db_path, session_id, stable_id)?;
    if matches!(local, Some(ToolCallDetailResult::Unavailable { .. })) {
        match checkpoint_tool_detail(sessions_dir, session_id, stable_id) {
            Ok(Some(body)) => return Ok(Some(ToolCallDetailResult::Full { body })),
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(%session_id, %stable_id, %error, "could not inspect tool detail checkpoints")
            }
        }
    }
    Ok(local)
}

fn checkpoint_tool_detail(
    sessions_dir: &Path,
    session_id: &str,
    stable_id: &str,
) -> Result<Option<TranscriptBody>> {
    let entries = match std::fs::read_dir(sessions_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("list tool detail checkpoints"),
    };
    let legacy_name = format!("{session_id}.hel.zip");
    let mut candidates = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let frontier = match managed_checkpoint_archive_name(&name) {
            Some(parsed) if parsed.session_id == session_id => parsed.frontier,
            None if name == legacy_name.as_str() => 0,
            _ => continue,
        };
        if entry.file_type()?.is_file() {
            candidates.push((frontier, entry.path()));
        }
    }
    candidates.sort_unstable_by(|first, second| second.cmp(first));
    for (_, path) in candidates {
        let archive = match mj_checkpoint::archive::verify_archive_streaming(&path) {
            Ok(archive) if archive.manifest.session.id == session_id => archive,
            Ok(_) => {
                tracing::warn!(%session_id, path = %path.display(), "checkpoint belongs to a different session");
                continue;
            }
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "could not verify tool detail checkpoint");
                continue;
            }
        };
        return Ok(archive
            .canonical_session
            .transcript
            .iter()
            .find(|item| item.stable_id == stable_id)
            .map(materialized_transcript_item_from_canonical)
            .map(|item| item.body)
            .filter(|body| !body.detail_trimmed()));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_checkpoint::archive::{ArchiveInput, write_archive_atomic};
    use mj_core::archive::{BundleManifest, SessionManifest, TargetManifest};
    use mj_core::config::HarnessKind;
    use mj_core::state::MaterializedSession;
    use mj_core::transcript::{TerminalOutputRecord, TranscriptItem};
    use std::sync::Arc;

    #[test]
    fn tool_call_detail_restores_legacy_trimmed_item_from_newest_verified_checkpoint() {
        let root = tempfile::tempdir().unwrap();
        let db_path = root.path().join("hel.sqlite3");
        let sessions_dir = root.path().join("sessions");
        std::fs::create_dir(&sessions_dir).unwrap();
        let session_id = "session-legacy-detail";
        let stable_id = "terminal:legacy";
        database::save_session_to(&db_path, &database::test_session(session_id, "project"))
            .unwrap();
        let body = TranscriptBody::terminal_output(TerminalOutputRecord {
            terminal_id: "legacy".into(),
            output: "legacy output\n".repeat(8192).into(),
            truncated: false,
            exit_code: Some(1),
            signal: None,
        });
        let mut projection = MaterializedSession::empty(session_id);
        projection.applied_event_ordinal = 2;
        projection.applied_event_digest = "a".repeat(64);
        projection.last_activity_at_ms = Some(100);
        projection.transcript.push(Arc::new(TranscriptItem {
            stable_id: stable_id.into(),
            position: 1,
            latest_content_event_ordinal: None,
            created_at_ms: 100,
            last_changed_at_ms: 100,
            body: body.clone(),
        }));
        let mut input = ArchiveInput {
            session: SessionManifest {
                id: session_id.into(),
                title: "Legacy session".into(),
                harness_kind: HarnessKind::Codex,
                profile_id: "codex".into(),
                native_session_id: "native".into(),
                created_at: "2026-08-09T10:00:00Z".into(),
                checkpointed_at: "2026-08-09T10:05:00Z".into(),
                hel_version: "0.1.0".into(),
                relay_version: "0.1.0".into(),
                adapter_version: "0.1.0".into(),
            },
            target: TargetManifest {
                template_id: "local".into(),
                target_kind: "local_bare".into(),
                details: Default::default(),
            },
            bundle: BundleManifest {
                id: "project".into(),
                primary_repository: "project".into(),
            },
            canonical_session: mj_transcript::projection::canonical_session_from_materialized(
                &projection,
            )
            .unwrap(),
            native_artifacts: Vec::new(),
            repositories: Vec::new(),
        };
        let archive_path = |frontier| {
            sessions_dir.join(format!(
                "{session_id}-{frontier}-archive-{}.hel.zip",
                "1".repeat(32)
            ))
        };
        write_archive_atomic(&archive_path(2), &input).unwrap();
        std::fs::write(archive_path(4), b"corrupt newer checkpoint").unwrap();

        // Model a row already compacted when migration 152 added nullable
        // spill pointers. Its only full copy is in the canonical archive.
        let summary = &mut Arc::make_mut(&mut projection.transcript[0]).body;
        assert!(mj_transcript::transcript::compact_transcript_body_for_retention(summary));
        let summary = summary.clone();
        database::save_materialized_session_to(&db_path, &projection).unwrap();
        let lookup = || {
            load_tool_call_detail_with_checkpoint_fallback_from(
                &db_path,
                &sessions_dir,
                session_id,
                stable_id,
            )
            .unwrap()
        };
        assert_eq!(lookup(), Some(ToolCallDetailResult::Full { body }));
        // A verified newer checkpoint is authoritative even when it only
        // carries the compacted row itself.
        projection.applied_event_ordinal = 3;
        input.canonical_session =
            mj_transcript::projection::canonical_session_from_materialized(&projection).unwrap();
        write_archive_atomic(&archive_path(3), &input).unwrap();
        assert_eq!(
            lookup(),
            Some(ToolCallDetailResult::Unavailable {
                summary: Some(summary),
                reason: "full tool detail is not stored locally".into(),
            })
        );
    }
}
