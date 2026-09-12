use mj_core::archive::*;
use mj_core::checkpoint::*;
use mj_core::config::HarnessKind;
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
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
            archive_path: spec.output_path.clone(),
            workspace_root: restored_workspace.clone(),
            relay_root: restored_relay.clone(),
            harness_home: restored_home.clone(),
            restore_repositories: true,
            restore_native: true,
            discard_queued_prompts: false,
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
fn git(repository: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(repository)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}
fn fixture(temp: &Path) -> (CheckpointExportSpec, PathBuf) {
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
