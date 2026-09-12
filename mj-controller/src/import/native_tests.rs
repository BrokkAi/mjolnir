use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use mj_core::archive::{PayloadRole, verify_archive_streaming};
use mj_core::checkpoint::{CheckpointRestoreSpec, restore_checkpoint};

use mj_core::config::{Config, HarnessKind, ProjectBundle, ProjectRepository, TargetTemplate};
use mj_core::state::{SessionState, State};

use mj_core::relay::WorkerEvent;
use serde_json::{Value, json};

use super::tests::initialize_repository;
use super::*;

const MUSE_ID: &str = "01a08120-6536-7721-8ddf-e0df0e921c2c";
const DSH_ID: &str = "dsh-native-import-1";

#[derive(Debug)]
struct NativeFixture {
    kind: HarnessKind,
    home: PathBuf,
    source_path: PathBuf,
    native_session_id: String,
    cwd: PathBuf,
    transcript: ClaudeTranscript,
}

fn test_config() -> Config {
    Config {
        bundles: BTreeMap::from([(
            "app".to_owned(),
            ProjectBundle {
                primary_repo: "app".to_owned(),
                repositories: vec![ProjectRepository {
                    id: "app".to_owned(),
                    github: Some("example/app".to_owned()),
                    local: None,
                    destination: PathBuf::from("app"),
                    git_ref: None,
                }],
            },
        )]),
        targets: BTreeMap::from([
            ("localhost".to_owned(), TargetTemplate::LocalBare),
            (
                "podman".to_owned(),
                TargetTemplate::LocalPodman {
                    container: mj_core::config::ContainerTemplate {
                        image: "agent-dev:latest".to_owned(),
                        pull_policy: Default::default(),
                        platform: None,
                        cpus: None,
                        memory: None,
                        environment: BTreeMap::new(),
                        workspace_storage: Default::default(),
                    },
                },
            ),
        ]),
        ..Config::default()
    }
}

fn fixture(root: &Path, kind: HarnessKind) -> NativeFixture {
    let cwd = root.join("app");
    initialize_repository(&cwd, "app");
    let (home, source_path, native_session_id) = match kind {
        HarnessKind::Muse => {
            let id = MUSE_ID.to_owned();
            let path = root
                .join("muse")
                .join(".data/muse/sessions/2026/09/08")
                .join(&id)
                .join("session.jsonl");
            (root.join("muse"), path, id)
        }
        HarnessKind::Deepseek => {
            let id = DSH_ID.to_owned();
            let home = root.join("dsh");
            let project = mj_core::native::deepseek::project_key(&cwd).unwrap();
            let encoded_id = mj_core::native::deepseek::encode_segment(&id).unwrap();
            let path = home
                .join("sessions")
                .join(project)
                .join(encoded_id)
                .join("session.jsonl");
            (home, path, id)
        }
        other => panic!("unsupported native test harness {other:?}"),
    };
    fs::create_dir_all(source_path.parent().unwrap()).unwrap();
    match kind {
        HarnessKind::Muse => write_muse_session(&source_path, &native_session_id, &cwd),
        HarnessKind::Deepseek => write_dsh_session(&source_path, &native_session_id, &cwd),
        _ => unreachable!(),
    }
    let transcript = read_native_transcript(kind, &source_path).unwrap();
    NativeFixture {
        kind,
        home,
        source_path,
        native_session_id,
        cwd,
        transcript,
    }
}

fn muse_record(id: &str, sequence: u64, payload_type: &str, payload: Value) -> Value {
    json!({
        "schema_version": 1,
        "id": id,
        "stream": {"kind": "session", "id": MUSE_ID},
        "sequence": sequence,
        "recorded_at": 1_788_872_000_000_000_i64 + sequence as i64,
        "record_type": "event",
        "durability": "durable",
        "payload_type": payload_type,
        "payload_schema_version": 1,
        "payload": payload,
    })
}

fn write_muse_session(path: &Path, id: &str, cwd: &Path) {
    let records = [
        muse_record(
            "metadata-1",
            1,
            "runtime.session.metadata",
            json!({"kind":"metadata","record":{"workspace_root":cwd,"provider_id":"test"}}),
        ),
        muse_record(
            "metadata-2",
            2,
            "runtime.session.metadata",
            json!({"kind":"metadata","record":{"workspace_root":cwd,"provider_id":"test"}}),
        ),
        muse_record(
            "intent-1",
            3,
            "runtime.user_intent.accepted",
            json!({"intent_id":"run-1","model_messages":[{"content":[{"kind":"text","text":"remember Muse import"}]}]}),
        ),
        muse_record(
            "started-1",
            4,
            "runtime.session",
            json!({"kind":"run","run_id":"run-1","event":{"kind":"started"}}),
        ),
        muse_record(
            "tools-1",
            5,
            "runtime.session",
            json!({"kind":"run","run_id":"run-1","event":{"kind":"assistant_tool_calls_committed","tool_calls":[{"id":"call-1","name":"write_file","args":"{\"path\":\"native-marker.txt\",\"content\":\"ok\"}"}]}}),
        ),
        muse_record(
            "assistant-1",
            6,
            "runtime.session",
            json!({"kind":"run","run_id":"run-1","event":{"kind":"assistant_message_committed","message_id":"message-1","text":"hello back"}}),
        ),
        muse_record(
            "terminal-1",
            7,
            "runtime.session",
            json!({"kind":"run","run_id":"run-1","event":{"kind":"terminal","terminal":"completed"}}),
        ),
    ];
    let body = records
        .into_iter()
        .map(|record| serde_json::to_string(&record).unwrap() + "\n")
        .collect::<String>();
    assert_eq!(id, MUSE_ID);
    fs::write(path, body).unwrap();
}

fn dsh_record(sequence: u64, time: i64, kind: &str, data: Value) -> Value {
    json!({"type":kind,"seq":sequence,"time":time,"data":data})
}

fn write_dsh_session(path: &Path, id: &str, cwd: &Path) {
    let header = json!({
        "type":"session",
        "version":0,
        "id":id,
        "createdAt":1_788_872_000_000_i64,
        "delegationDepth":0,
        "cwd":cwd,
    });
    let records = [
        dsh_record(
            0,
            1_788_872_000_001,
            "user/message",
            json!({"source":{"kind":"user"},"content":[{"type":"text","text":"remember DSH import"}]}),
        ),
        dsh_record(
            1,
            1_788_872_000_002,
            "tool/call",
            json!({"callId":"call-1","name":"write","arguments":"{\"path\":\"native-marker.txt\"}"}),
        ),
        dsh_record(
            2,
            1_788_872_000_003,
            "tool/result",
            json!({"message":{"source":{"callId":"call-1"},"content":[{"type":"text","text":"ok"}],"isError":false}}),
        ),
        dsh_record(
            3,
            1_788_872_000_004,
            "assistant/message",
            json!({"message":{"id":"message-1","content":[{"type":"text","text":"hello back"}]}}),
        ),
        dsh_record(4, 1_788_872_000_005, "turn/end", json!({})),
    ];
    let mut body = serde_json::to_string(&header).unwrap();
    body.push('\n');
    for record in records {
        body.push_str(&serde_json::to_string(&record).unwrap());
        body.push('\n');
    }
    fs::write(path, body).unwrap();
}

fn progress_control<'a>(cancelled: &'a AtomicBool) -> ImportControl<'a> {
    fn report(_: ImportArchiveProgress) {}
    ImportControl {
        cancelled,
        progress: &report,
        include_untracked: true,
    }
}

fn import_fixture(fixture: &NativeFixture, root: &Path) -> ImportedClaudeSession {
    let config = test_config();
    let mut state = State::default();
    let archive_directory = root.join("archives");
    fs::create_dir_all(&archive_directory).unwrap();
    import_native_session(
        &config,
        &mut state,
        NativeImportRequest {
            harness: fixture.kind,
            harness_home: &fixture.home,
            native_session_id: &fixture.native_session_id,
            source_path: &fixture.source_path,
            transcript: &fixture.transcript,
            bundle_id: "app",
            profile_id: None,
            title: None,
            archive_directory: &archive_directory,
        },
        None,
    )
    .unwrap()
}

#[test]
fn native_dispatch_and_import_preserve_identity_and_source_for_muse_and_dsh() {
    for kind in [HarnessKind::Muse, HarnessKind::Deepseek] {
        let directory = tempfile::tempdir().unwrap();
        let fixture = fixture(directory.path(), kind);
        let original = fs::read(&fixture.source_path).unwrap();
        let located = locate_native_session(
            kind,
            &fixture.home,
            &ClaudeSessionSelection::NativeSessionId(fixture.native_session_id.clone()),
        )
        .unwrap();
        assert_eq!(located.native_session_id, fixture.native_session_id);
        assert_eq!(located.source_path, fixture.source_path);
        let transcript = read_native_transcript(kind, &located.source_path).unwrap();
        assert_eq!(transcript.cwd, fixture.cwd);
        assert_eq!(
            transcript.edited_paths,
            [PathBuf::from("native-marker.txt")]
        );

        let mut listings = Vec::new();
        scan_native_sessions(kind, &fixture.home, |progress| {
            if let Some(session) = progress.session {
                listings.push(session);
            }
        })
        .unwrap();
        assert_eq!(listings.len(), 1);
        assert_eq!(listings[0].native_session_id, fixture.native_session_id);
        assert_eq!(listings[0].unavailable_reason, None);

        let config = test_config();
        let mut state = State::default();
        let archive_directory = directory.path().join("archives");
        fs::create_dir_all(&archive_directory).unwrap();
        let imported = import_native_session(
            &config,
            &mut state,
            NativeImportRequest {
                harness: kind,
                harness_home: &fixture.home,
                native_session_id: &fixture.native_session_id,
                source_path: &fixture.source_path,
                transcript: &transcript,
                bundle_id: "app",
                profile_id: None,
                title: None,
                archive_directory: &archive_directory,
            },
            None,
        )
        .unwrap();
        let record = &state.sessions[&imported.session_id];
        assert_eq!(record.state, SessionState::Stopped);
        assert_eq!(
            record.native_session_id.as_deref(),
            Some(fixture.native_session_id.as_str())
        );
        let verified = verify_archive_streaming(&imported.archive_path).unwrap();
        assert_eq!(
            verified.manifest.session.native_session_id,
            fixture.native_session_id
        );
        assert!(
            verified
                .manifest
                .payloads
                .iter()
                .any(|payload| matches!(&payload.role, PayloadRole::NativeArtifact { .. }))
        );
        assert_eq!(fs::read(&fixture.source_path).unwrap(), original);
    }
}

#[test]
fn native_checkpoint_restore_relocates_muse_and_dsh_without_changing_identity() {
    for kind in [HarnessKind::Muse, HarnessKind::Deepseek] {
        let directory = tempfile::tempdir().unwrap();
        let fixture = fixture(directory.path(), kind);
        let imported = import_fixture(&fixture, directory.path());
        let restored_workspace = directory.path().join("restored-workspace");
        let target_cwd = restored_workspace.join("app");
        fs::create_dir_all(&target_cwd).unwrap();
        let restored_home = directory.path().join("restored-home");
        restore_checkpoint(
            &CheckpointRestoreSpec {
                archive_path: imported.archive_path,
                workspace_root: restored_workspace.clone(),
                relay_root: directory.path().join("restored-relay"),
                harness_home: restored_home.clone(),
                restore_repositories: false,
                restore_native: true,
                discard_queued_prompts: false,
                primary_repository_root: None,
            },
            &mj_core::archive::SystemGit,
        )
        .unwrap();
        let located = locate_native_session(
            kind,
            &restored_home,
            &ClaudeSessionSelection::NativeSessionId(fixture.native_session_id.clone()),
        )
        .unwrap();
        assert_eq!(located.native_session_id, fixture.native_session_id);
        let transcript = read_native_transcript(kind, &located.source_path).unwrap();
        assert_eq!(transcript.cwd, target_cwd);
        assert_eq!(
            transcript.edited_paths,
            [PathBuf::from("native-marker.txt")]
        );
        assert!(transcript.events.iter().any(|event| matches!(
            event.event,
            WorkerEvent::PromptAccepted { ref text, .. } if text.contains("import")
        )));
    }
}

#[test]
fn native_import_rejects_changed_preview_and_cancellation_without_publishing() {
    for kind in [HarnessKind::Muse, HarnessKind::Deepseek] {
        let directory = tempfile::tempdir().unwrap();
        let source_fixture = fixture(directory.path(), kind);
        let original = fs::read_to_string(&source_fixture.source_path).unwrap();
        let changed = original.replace("hello back", "changed native reply");
        assert_ne!(changed, original);
        fs::write(&source_fixture.source_path, changed).unwrap();
        let config = test_config();
        let archive_directory = directory.path().join("stale-archives");
        let mut state = State::default();
        let cancelled = AtomicBool::new(false);
        let control = progress_control(&cancelled);
        let error = import_native_session_with_control(
            &config,
            &mut state,
            NativeImportRequest {
                harness: kind,
                harness_home: &source_fixture.home,
                native_session_id: &source_fixture.native_session_id,
                source_path: &source_fixture.source_path,
                transcript: &source_fixture.transcript,
                bundle_id: "app",
                profile_id: None,
                title: None,
                archive_directory: &archive_directory,
            },
            &control,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("native session changed"));
        assert!(state.sessions.is_empty());
        assert!(
            !archive_directory.exists()
                || fs::read_dir(&archive_directory).unwrap().next().is_none()
        );

        let directory = tempfile::tempdir().unwrap();
        let cancelled_fixture = fixture(directory.path(), kind);
        let config = test_config();
        let archive_directory = directory.path().join("cancelled-archives");
        let mut state = State::default();
        let cancelled = AtomicBool::new(true);
        let control = progress_control(&cancelled);
        let error = import_native_session_with_control(
            &config,
            &mut state,
            NativeImportRequest {
                harness: kind,
                harness_home: &cancelled_fixture.home,
                native_session_id: &cancelled_fixture.native_session_id,
                source_path: &cancelled_fixture.source_path,
                transcript: &cancelled_fixture.transcript,
                bundle_id: "app",
                profile_id: None,
                title: None,
                archive_directory: &archive_directory,
            },
            &control,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("import cancelled"));
        assert!(state.sessions.is_empty());
        assert!(
            !archive_directory.exists()
                || fs::read_dir(&archive_directory).unwrap().next().is_none()
        );
    }
}
