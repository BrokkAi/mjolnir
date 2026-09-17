use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use mj_checkpoint::archive::{PayloadRole, verify_archive_streaming};
use mj_checkpoint::checkpoint::{CheckpointRestoreSpec, restore_checkpoint};

use mj_core::config::{Config, HarnessKind, ProjectBundle, ProjectRepository, TargetTemplate};
use mj_core::state::{SessionState, State};

use mj_core::relay::WorkerEvent;

use super::tests::initialize_repository;
use super::*;

use super::test_fixtures::{MUSE_ID, write_muse_session};

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
                        build_cache: None,
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
        other => panic!("unsupported native test harness {other:?}"),
    };
    fs::create_dir_all(source_path.parent().unwrap()).unwrap();
    match kind {
        HarnessKind::Muse => write_muse_session(&source_path, &native_session_id, &cwd),
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
fn native_dispatch_and_import_preserve_identity_and_source_for_muse() {
    let kind = HarnessKind::Muse;
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
    scan_native_sessions(kind, &fixture.home, &NativeScanCache::new(), |progress| {
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

#[test]
fn native_checkpoint_restore_relocates_muse_without_changing_identity() {
    let kind = HarnessKind::Muse;
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
        &mj_checkpoint::archive::SystemGit,
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

#[test]
fn native_import_rejects_changed_preview_and_cancellation_without_publishing() {
    let kind = HarnessKind::Muse;
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
    let error = import_native_session(
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
        Some(&control),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("native session changed"));
    assert!(state.sessions.is_empty());
    assert!(
        !archive_directory.exists() || fs::read_dir(&archive_directory).unwrap().next().is_none()
    );

    let directory = tempfile::tempdir().unwrap();
    let cancelled_fixture = fixture(directory.path(), kind);
    let config = test_config();
    let archive_directory = directory.path().join("cancelled-archives");
    let mut state = State::default();
    let cancelled = AtomicBool::new(true);
    let control = progress_control(&cancelled);
    let error = import_native_session(
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
        Some(&control),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("import cancelled"));
    assert!(state.sessions.is_empty());
    assert!(
        !archive_directory.exists() || fs::read_dir(&archive_directory).unwrap().next().is_none()
    );
}
