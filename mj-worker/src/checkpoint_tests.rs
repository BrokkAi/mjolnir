use crate::test_support::git;
use mj_checkpoint::archive::*;
use mj_checkpoint::checkpoint::*;
use mj_core::config::HarnessKind;
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};
const SESSION: &str = "018f9dd2-a3b4-7c8d-9000-123456789abc";
const NATIVE: &str = "0190aabb-ccdd-7eef-9000-abcdef012345";
#[cfg(unix)]
#[tokio::test]
#[ignore = "requires pinned MJ_MUSE_ACP_TEST_BINARY, MJ_MUSE_TEST_BINARY and authenticated MJ_MUSE_TEST_HOME; uses two provider turns"]
async fn live_muse_checkpoint_restore_relocates_native_context_and_preserves_queued_work() {
    let required_path = |name| PathBuf::from(std::env::var_os(name).expect(name));
    let adapter = required_path("MJ_MUSE_ACP_TEST_BINARY");
    let muse = required_path("MJ_MUSE_TEST_BINARY");
    let account = required_path("MJ_MUSE_TEST_HOME");
    let temp = tempfile::tempdir().unwrap();
    let (mut spec, _) = fixture(temp.path());
    spec.session.harness_kind = HarnessKind::Muse;
    spec.session.profile_id = "muse-smoke".into();
    spec.harness_home = temp.path().join("original/muse");
    fs::create_dir_all(&spec.harness_home).unwrap();
    fs::copy(
        account.join("auth.json"),
        spec.harness_home.join("auth.json"),
    )
    .unwrap();
    let cwd = spec.workspace_root.join("app");
    let token = format!(
        "MUSE_{}",
        temp.path().file_name().unwrap().to_string_lossy()
    );
    let prompt = format!(
        "Remember this exact token for the next turn: {token}. Reply with only that token. Do not use tools."
    );
    let (session_id, reply) = crate::acp::muse_tests::native_muse_turn(
        &adapter,
        &muse,
        &spec.harness_home,
        &cwd,
        None,
        &prompt,
    )
    .await;
    assert!(
        reply.contains(&token),
        "provider did not acknowledge the smoke token"
    );
    spec.session.native_session_id = session_id.clone();
    spec.canonical_session.transcript[0].body = CanonicalTranscriptBody::User {
        content: vec![json!({"type":"text", "text":prompt})],
    };
    export_checkpoint(&spec).unwrap();
    let archive = read_archive_verified(&spec.output_path).unwrap();
    assert!(
        archive
            .manifest
            .payloads
            .iter()
            .all(|payload| !payload.path.ends_with("auth.json"))
    );
    let restored_home = temp.path().join("restored/muse");
    let restored_relay = temp.path().join("restored-relay");
    let restored_workspace = temp.path().join("restored-workspace");
    fs::create_dir_all(&restored_workspace).unwrap();
    git(
        &restored_workspace,
        &["clone", "--quiet", cwd.to_str().unwrap(), "app"],
    );
    restore_checkpoint(
        &CheckpointRestoreSpec {
            report_root: None,
            archive_path: spec.output_path.clone(),
            workspace_root: restored_workspace.clone(),
            relay_root: restored_relay.clone(),
            harness_home: restored_home.clone(),
            restore_repositories: true,
            restore_native: true,
            queue_policy: mj_checkpoint::checkpoint::QueueRestorePolicy::Restore,
            primary_repository_root: None,
        },
        &SystemGit,
    )
    .unwrap();
    assert!(
        !restored_home.join("auth.json").exists(),
        "credentials entered the archive"
    );
    assert_eq!(
        restored_seed(&restored_relay).queued_prompts,
        spec.canonical_session.queued_prompts
    );
    fs::copy(
        spec.harness_home.join("auth.json"),
        restored_home.join("auth.json"),
    )
    .unwrap();
    let (resumed_id, reply) = crate::acp::muse_tests::native_muse_turn(
            &adapter, &muse, &restored_home, &restored_workspace.join("app"), Some(session_id.clone()),
            "What exact token did I ask you to remember in the previous turn? Reply only with the token. Do not use tools.",
        ).await;
    assert_eq!(resumed_id, session_id);
    assert!(reply.contains(&token), "native context was not restored");
}
pub(crate) fn fixture(temp: &Path) -> (CheckpointExportSpec, PathBuf) {
    let worker_root = temp.join("worker");
    fs::create_dir_all(&worker_root).unwrap();
    let harness_home = temp.join("codex");
    let native = harness_home.join("sessions/2026/08/09");
    fs::create_dir_all(&native).unwrap();
    fs::write(native.join(format!("rollout-{NATIVE}.jsonl")), b"native").unwrap();
    let workspace = temp.join("workspace");
    let repository = workspace.join("app");
    fs::create_dir_all(&repository).unwrap();
    git(&repository, &["init"]);
    git(&repository, &["config", "user.email", "hel@example.test"]);
    git(&repository, &["config", "user.name", "Hel Test"]);
    fs::write(repository.join("README.md"), b"hello").unwrap();
    git(&repository, &["add", "."]);
    git(&repository, &["commit", "-m", "base"]);
    git(
        &repository,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/example/app.git",
        ],
    );
    let base = git(&repository, &["rev-parse", "HEAD"]);
    let output = worker_root.join("source.hel.zip");
    (
        CheckpointExportSpec {
            ignore_nested_repositories: false,
            report_directories: None,
            protocol_version: CHECKPOINT_EXPORT_PROTOCOL_VERSION,
            session: SessionManifest {
                id: SESSION.into(),
                title: "test".into(),
                harness_kind: HarnessKind::Codex,
                profile_id: "codex-1".into(),
                native_session_id: NATIVE.into(),
                created_at: "2026-08-09T00:00:00Z".into(),
                checkpointed_at: "2026-08-09T00:01:00Z".into(),
                hel_version: "0.1.0".into(),
                relay_version: "0.1.0".into(),
                adapter_version: "test".into(),
            },
            target: TargetManifest {
                template_id: "local".into(),
                target_kind: "podman".into(),
                details: Default::default(),
            },
            bundle: BundleManifest {
                id: "bundle".into(),
                primary_repository: "app".into(),
            },
            relay_root: worker_root,
            harness_home,
            workspace_root: workspace,
            repositories: vec![CheckpointRepositorySpec {
                id: "app".into(),
                relative_destination: "app".into(),
                capture: CheckpointRepositoryCapture::DeltaFrom { base_commit: base },
                origin_override: None,
            }],
            canonical_session: CanonicalSessionSnapshot {
                command_ledger: None,
                assessment_state: None,
                notification_stack: None,
                event_frontier: 1,
                event_frontier_digest: "a".repeat(64),
                session: CanonicalSessionState {
                    execution: CanonicalExecutionState::Idle,
                    last_activity_at_ms: Some(1),
                    session_title: Some("test".into()),
                    configuration: Default::default(),
                },
                transcript: vec![CanonicalTranscriptItem {
                    stable_id: "user-1".into(),
                    position: 1,
                    latest_content_event_ordinal: None,
                    created_at_ms: 1,
                    last_changed_at_ms: 1,
                    body: CanonicalTranscriptBody::User {
                        content: vec![json!({"type": "text", "text": "hello"})],
                    },
                }],
                queued_prompts: vec![CanonicalQueuedPrompt {
                    command_id: "queued-1".into(),
                    kind: CanonicalQueuedCommandKind::Prompt,
                    content: vec![json!({"type": "text", "text": "next"})],
                    queued_at_ms: 2,
                }],
            },
            output_path: output.clone(),
        },
        output,
    )
}
fn restored_seed(relay_root: &Path) -> mj_core::relay::RestoredRelaySeed {
    serde_json::from_slice(&fs::read(mj_core::relay::restored_relay_seed_path(relay_root)).unwrap())
        .unwrap()
}

// Hard-won: 055d8a772 review: recovery of one component must preserve newer parent state and current files.
#[test]
fn checkout_recovery_publishes_only_the_staged_checkout_and_preserves_a_new_current_directory() {
    let temp = tempfile::tempdir().unwrap();
    let (mut source, archive_path) = fixture(temp.path());
    git(
        &source.workspace_root,
        &["clone", "--quiet", "app", "second"],
    );
    source.repositories.push(CheckpointRepositorySpec {
        id: "second".into(),
        relative_destination: "second".into(),
        capture: CheckpointRepositoryCapture::DeltaFrom {
            base_commit: git(
                &source.workspace_root.join("second"),
                &["rev-parse", "HEAD"],
            )
            .trim()
            .into(),
        },
        origin_override: None,
    });
    fs::write(
        source.workspace_root.join("second/README.md"),
        b"archived second repository work",
    )
    .unwrap();
    export_checkpoint(&source).unwrap();
    let stage = temp.path().join("checkout-stage");
    fs::create_dir(&stage).unwrap();
    git(
        &stage,
        &[
            "clone",
            "--quiet",
            source.workspace_root.join("app").to_str().unwrap(),
            "app",
        ],
    );
    git(
        &stage,
        &[
            "clone",
            "--quiet",
            source.workspace_root.join("second").to_str().unwrap(),
            "second",
        ],
    );
    let parent_native = temp.path().join("current-native");
    fs::create_dir(&parent_native).unwrap();
    fs::write(parent_native.join("sentinel"), b"newer native state").unwrap();
    let parent_relay = temp.path().join("current-relay");
    fs::create_dir(&parent_relay).unwrap();
    fs::write(
        parent_relay.join(mj_core::relay::RELAY_STATE_FILE),
        b"current relay",
    )
    .unwrap();
    let destination = temp.path().join("checkout-current");
    let spec = CheckoutRecoverySpec {
        version: 1,
        checkpoint_sha256: verify_archive_streaming(&archive_path)
            .unwrap()
            .archive_sha256,
        staging_root: stage.clone(),
        staged_checkout: stage.join("app"),
        destination_checkout: destination.join("app"),
        additional_checkouts: vec![CheckoutRecoveryPublication {
            staged_checkout: stage.join("second"),
            destination_checkout: destination.join("second"),
        }],
        restore: CheckpointRestoreSpec {
            archive_path,
            workspace_root: stage.clone(),
            report_root: None,
            relay_root: parent_relay.clone(),
            harness_home: parent_native.clone(),
            restore_repositories: true,
            restore_native: false,
            queue_policy: QueueRestorePolicy::Restore,
            primary_repository_root: None,
        },
    };
    let spec_path = temp.path().join("checkout-recovery.json");
    fs::write(&spec_path, serde_json::to_vec(&spec).unwrap()).unwrap();
    crate::checkpoint::restore_checkout_from_spec_file(&spec_path).unwrap();
    assert!(destination.join("app/.git").is_dir());
    assert!(!stage.join("app").exists());
    assert!(!stage.join("second").exists());
    assert_eq!(
        fs::read(destination.join("second/README.md")).unwrap(),
        b"archived second repository work"
    );
    fs::write(destination.join("app/current-work"), b"new current files").unwrap();
    // An interrupted caller can retry after publication; it must not replay
    // the archive over the now-current checkout, even without its old staging.
    crate::checkpoint::restore_checkout_from_spec_file(&spec_path).unwrap();
    assert_eq!(
        fs::read(destination.join("app/current-work")).unwrap(),
        b"new current files"
    );
    assert_eq!(
        fs::read(parent_native.join("sentinel")).unwrap(),
        b"newer native state"
    );
    assert_eq!(
        fs::read(parent_relay.join(mj_core::relay::RELAY_STATE_FILE)).unwrap(),
        b"current relay"
    );
}
