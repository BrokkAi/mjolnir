use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use mj_checkpoint::archive::{PayloadRole, verify_archive_streaming};
use mj_checkpoint::checkpoint::{CheckpointRestoreSpec, restore_checkpoint};

use mj_core::config::{Config, HarnessKind, ProjectBundle, ProjectRepository, TargetTemplate};
use mj_core::state::{SessionState, State};

use mj_core::transcript::TranscriptBody;

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
                    mbx_namespace: None,
                }],
            },
        )]),
        targets: BTreeMap::from([
            ("localhost".to_owned(), TargetTemplate::LocalBare),
            (
                "podman".to_owned(),
                TargetTemplate::LocalPodman {
                    container: mj_core::config::ContainerTemplate {
                        memory_hard_limit: None,
                        build_cache: None,
                        image: "agent-dev:latest".to_owned(),
                        pull_policy: Default::default(),
                        platform: None,
                        cpus: None,
                        memory: None,
                        environment: Default::default(),
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
            daemon_project_root: None,
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
            daemon_project_root: None,
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
            report_root: None,
            archive_path: imported.archive_path,
            workspace_root: restored_workspace.clone(),
            relay_root: directory.path().join("restored-relay"),
            harness_home: restored_home.clone(),
            restore_repositories: false,
            restore_native: true,
            queue_policy: mj_checkpoint::checkpoint::QueueRestorePolicy::Restore,
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
    assert!(transcript.items.iter().any(|item| matches!(
        &item.body,
        TranscriptBody::User { content } if content[0]["text"].as_str().unwrap().contains("import")
    )));
}

// Hard-won: 70c1a004a: file-backed Muse artifacts lost their captured content identity.
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
            daemon_project_root: None,
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
            daemon_project_root: None,
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

    // A write after validation must not replace the captured native bytes
    // that were paired with the preview transcript.
    let directory = tempfile::tempdir().unwrap();
    let mut source_fixture = fixture(directory.path(), kind);
    let reply = "h".repeat(128 * 1024);
    let large = fs::read_to_string(&source_fixture.source_path)
        .unwrap()
        .replace("hello back", &reply);
    fs::write(&source_fixture.source_path, large).unwrap();
    source_fixture.transcript = read_native_transcript(kind, &source_fixture.source_path).unwrap();
    let original = fs::read(&source_fixture.source_path).unwrap();
    assert!(original.len() > 64 * 1024);
    let changed = String::from_utf8(original.clone())
        .unwrap()
        .replace(&reply, "later native reply");
    let config = test_config();
    let archive_directory = directory.path().join("snapshot-archives");
    let mut state = State::default();
    let cancelled = AtomicBool::new(false);
    let report = |progress| {
        if matches!(progress, ImportArchiveProgress::WritingArchive) {
            fs::write(&source_fixture.source_path, &changed).unwrap();
        }
    };
    let control = ImportControl {
        cancelled: &cancelled,
        progress: &report,
        include_untracked: true,
    };
    let imported = import_native_session(
        &config,
        &mut state,
        NativeImportRequest {
            daemon_project_root: None,
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
    .unwrap();
    let checkpoint = state
        .sessions
        .get(&imported.session_id)
        .unwrap()
        .checkpoint
        .as_ref()
        .unwrap();
    let verified = verify_archive_streaming(&checkpoint.archive_path).unwrap();
    let native = verified
        .manifest
        .payloads
        .iter()
        .find(|payload| {
            matches!(
                &payload.role, PayloadRole::NativeArtifact { relative_path }
                    if relative_path.file_name().is_some_and(|name| name == "session.jsonl")
            )
        })
        .unwrap();
    assert_eq!(
        mj_checkpoint::archive::read_verified_checkpoint_payload(
            &checkpoint.archive_path,
            native,
            native.size,
        )
        .unwrap(),
        original
    );
    assert_eq!(
        fs::read_to_string(&source_fixture.source_path).unwrap(),
        changed
    );
}

#[test]
fn antigravity_import_recovers_workspace_from_log_when_summary_has_none() {
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path().join("agy");
    let cwd = directory.path().join("workspace with spaces");
    let workspace_uri = url::Url::from_directory_path(&cwd).unwrap().to_string();
    let root = home.join("antigravity-cli");
    let conversation = root.join("brain/session-id");
    fs::create_dir_all(&conversation).unwrap();
    fs::write(
        conversation.join("transcript.jsonl"),
        [
            json!({"type":"USER_INPUT","step_index":1,"content":"Read the workspace"}).to_string(),
            json!({"cwd":"  ","metadata":{"workspaceUri": workspace_uri}}).to_string(),
            "malformed JSONL row".to_owned(),
            json!({"type":"PLANNER_RESPONSE","step_index":2,"content":"Found it"}).to_string(),
        ]
        .join("\n"),
    )
    .unwrap();
    let databases = root.join("conversations");
    fs::create_dir(&databases).unwrap();
    rusqlite::Connection::open(databases.join("session-id.db")).unwrap();
    rusqlite::Connection::open(root.join("conversation_summaries.db"))
        .unwrap()
        .execute_batch(
            "CREATE TABLE conversation_summaries (
                conversation_id TEXT PRIMARY KEY, title TEXT, workspace_uris TEXT
             );
             INSERT INTO conversation_summaries VALUES ('session-id', 'Summary title', NULL);",
        )
        .unwrap();

    let kind = HarnessKind::Antigravity;
    let mut sessions = Vec::new();
    scan_native_sessions(kind, &home, &NativeScanCache::new(), |progress| {
        sessions.extend(progress.session);
    })
    .unwrap();
    let [listing] = sessions.as_slice() else {
        panic!("expected one Antigravity session, got {sessions:?}");
    };
    assert_eq!(listing.title, "Summary title");
    assert_eq!(listing.cwd, cwd);
    assert_eq!(listing.unavailable_reason, None);

    let located = locate_native_session(kind, &home, &ClaudeSessionSelection::Latest).unwrap();
    assert_eq!(located.native_session_id, "session-id");
    let transcript = read_native_transcript(kind, &located.source_path).unwrap();
    assert_eq!(transcript.cwd, cwd);
}

#[test]
fn antigravity_dsh_and_opencode_import_verified_native_state() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let cwd = root.join("app");
    initialize_repository(&cwd, "app");
    let raw_cwd = format!("{}{}.", cwd.display(), std::path::MAIN_SEPARATOR);
    let id = "Literal-Session_ID";
    let edited = "native-marker.txt";

    let agy = root.join("agy");
    let agy_brain = agy.join("antigravity-cli/brain").join(id);
    fs::create_dir_all(&agy_brain).unwrap();
    let agy_log = agy_brain.join("transcript.jsonl");
    fs::write(
        &agy_log,
        [
            json!({"type":"USER_INPUT","step_index":1,"cwd":raw_cwd,"content":"import my session"}),
            json!({"type":"PLANNER_RESPONSE","step_index":2,"content":"I will edit it","tool_calls":[{"name":"write_file","args":{"path":edited}}]}),
            json!({"type":"GENERIC","step_index":3,"status":"DONE"}),
        ]
        .map(|row| row.to_string())
        .join("\n"),
    ).unwrap();
    let conversations = agy.join("antigravity-cli/conversations");
    fs::create_dir_all(&conversations).unwrap();
    let agy_db = conversations.join(format!("{id}.db"));
    rusqlite::Connection::open(&agy_db)
        .unwrap()
        .execute_batch("CREATE TABLE trajectory_metadata_blob(id TEXT PRIMARY KEY, data BLOB)")
        .unwrap();
    rusqlite::Connection::open(conversations.join("other.db")).unwrap();

    let dsh = root.join("dsh");
    let dsh_relative = PathBuf::from("sessions").join(format!("{id}.json"));
    fs::create_dir_all(dsh.join("sessions")).unwrap();
    fs::write(dsh.join(&dsh_relative), json!({
        "id":id, "cwd":raw_cwd, "title":"Imported DSH",
        "created_at":"2026-11-10T00:00:00Z", "updated_at":"2026-11-10T00:01:00Z",
        "config":{"model":"deepseek-flash","mode":"agent","permission_mode":"ask"},
        "messages":[
            {"role":"user","content":"import my session"},
            {"role":"assistant","content":"I will edit it","tool_calls":[
                {"id":"edit","type":"function","function":{"name":"write_file","arguments":json!({"path":edited}).to_string()}}
            ]}
        ]
    }).to_string()).unwrap();
    fs::write(
        dsh.join("sessions/child.json"),
        json!({
            "id":"child", "cwd":raw_cwd, "parent_session_id":id,
            "messages":[{"role":"user","content":"delegate".repeat(16_000)}]
        })
        .to_string(),
    )
    .unwrap();

    let opencode = root.join("opencode");
    let sqlite_relative = PathBuf::from(".data/opencode/opencode.db");
    let db = opencode.join(&sqlite_relative);
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch("
        PRAGMA journal_mode=WAL;
        PRAGMA wal_autocheckpoint=OFF;
        CREATE TABLE session_v2(id TEXT PRIMARY KEY, directory TEXT, title TEXT, time_created INTEGER, time_updated INTEGER, parent_id TEXT);
        CREATE TABLE session_message(id TEXT PRIMARY KEY, session_id TEXT, type TEXT, time_created INTEGER, seq INTEGER, data TEXT);
        CREATE TABLE session_part(message_id TEXT, session_id TEXT, ordinal INTEGER, type TEXT, data TEXT);
        CREATE TABLE migration(id TEXT PRIMARY KEY, time_completed INTEGER);
        INSERT INTO migration VALUES ('20261029002625_session_part', 1);
        CREATE TABLE credential(id TEXT PRIMARY KEY, value TEXT);
        INSERT INTO credential VALUES ('credential', 'unrelated-provider-secret');
        CREATE TABLE private_plugin_config(value TEXT);
        INSERT INTO private_plugin_config VALUES ('unrelated-plugin-data');
    ").unwrap();
    for (sid, parent) in [(id, None), ("another", Some(id))] {
        conn.execute("INSERT INTO session_v2 VALUES (?1, ?2, 'Imported OpenCode', 1794272400000, 1794272400000, ?3)", rusqlite::params![sid, raw_cwd, parent]).unwrap();
        conn.execute(
            "INSERT INTO session_message VALUES (?1, ?2, 'user', 1794272400000, 1, ?3)",
            rusqlite::params![
                format!("{sid}-user"),
                sid,
                json!({"text":"import my session"}).to_string()
            ],
        )
        .unwrap();
        let assistant = format!("{sid}-assistant");
        conn.execute(
            r#"INSERT INTO session_message VALUES (?1, ?2, 'assistant', 1794272399999, 2, '{"content":[{"type":"text","text":"I will edit it"}]}')"#,
            rusqlite::params![assistant, sid],
        ).unwrap();
        conn.execute(
            "INSERT INTO session_part VALUES (?1, ?2, 0, 'tool', ?3)",
            rusqlite::params![
                assistant,
                sid,
                json!({"name":"write","state":{"status":"completed","input":{"filePath":edited}}})
                    .to_string()
            ],
        )
        .unwrap();
    }

    for (kind, home, artifacts) in [
        (
            HarnessKind::Antigravity,
            agy.clone(),
            vec![
                agy_log.strip_prefix(&agy).unwrap().to_owned(),
                agy_db.strip_prefix(&agy).unwrap().to_owned(),
            ],
        ),
        (HarnessKind::Dsh, dsh, vec![dsh_relative]),
        (
            HarnessKind::OpenCode,
            opencode,
            vec![sqlite_relative.clone()],
        ),
    ] {
        let cache = NativeScanCache::new();
        let scan = || {
            let mut sessions = Vec::new();
            scan_native_sessions(kind, &home, &cache, |progress| {
                sessions.extend(progress.session)
            })
            .unwrap();
            sessions
        };
        let listings = scan();
        let parsed = cache.parsed_files();
        assert_eq!(scan().len(), 1);
        assert_eq!(cache.parsed_files(), parsed);
        assert_eq!(listings.len(), 1, "{kind:?}");
        assert_eq!(listings[0].native_session_id, id);
        assert_eq!(listings[0].cwd.as_os_str(), Path::new(&raw_cwd).as_os_str());
        assert_eq!(listings[0].unavailable_reason, None);
        let located = locate_native_session(kind, &home, &ClaudeSessionSelection::Latest).unwrap();
        assert_eq!(
            located,
            locate_native_session(
                kind,
                &home,
                &ClaudeSessionSelection::NativeSessionId(id.into())
            )
            .unwrap()
        );
        assert_eq!(
            native_session_id_from_path(kind, &located.source_path).as_deref(),
            Some(id)
        );
        let transcript = read_native_transcript(kind, &located.source_path).unwrap();
        assert_eq!(transcript.cwd.as_os_str(), Path::new(&raw_cwd).as_os_str());
        assert_eq!(transcript.edited_paths, [PathBuf::from(edited)], "{kind:?}");
        assert_eq!(transcript.items[0].stable_id, "user-import-1");
        assert!(
            transcript
                .items
                .windows(2)
                .all(|pair| pair[0].created_at_ms <= pair[1].created_at_ms)
        );
        let imported = import_fixture(
            &NativeFixture {
                kind,
                home,
                cwd: cwd.clone(),
                transcript,
                source_path: located.source_path,
                native_session_id: located.native_session_id,
            },
            root,
        );
        let archive =
            mj_checkpoint::archive::read_archive_verified(&imported.archive_path).unwrap();
        assert!(mj_checkpoint::checkpoint::archive_holds_native_conversation(&archive.manifest));
        assert_eq!(archive.manifest.session.native_session_id, id);
        let mut actual = archive
            .manifest
            .payloads
            .iter()
            .filter_map(|payload| match &payload.role {
                PayloadRole::NativeArtifact { relative_path } => Some(relative_path.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        actual.sort();
        assert_eq!(actual, artifacts);
        if kind == HarnessKind::OpenCode {
            let restored_home = root.join("restored-opencode");
            let target_cwd = root.join("relocated-app");
            fs::create_dir_all(&target_cwd).unwrap();
            restore_checkpoint(
                &CheckpointRestoreSpec {
                    archive_path: imported.archive_path,
                    workspace_root: root.to_owned(),
                    relay_root: root.join("restored-opencode-relay"),
                    harness_home: restored_home.clone(),
                    restore_repositories: false,
                    restore_native: true,
                    report_root: None,
                    queue_policy: mj_checkpoint::checkpoint::QueueRestorePolicy::Restore,
                    primary_repository_root: Some(target_cwd.clone()),
                },
                &mj_checkpoint::archive::SystemGit,
            )
            .unwrap();
            let copy = rusqlite::Connection::open(restored_home.join(&sqlite_relative)).unwrap();
            let saved_cwd: String = copy
                .query_row(
                    "SELECT directory FROM session_v2 WHERE id = ?1",
                    [id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(Path::new(&saved_cwd), target_cwd);
            for (table, expected) in [
                ("session_v2", 1),
                ("session_message", 2),
                ("session_part", 1),
                ("migration", 1),
                ("credential", 0),
                ("private_plugin_config", 0),
            ] {
                let count: i64 = copy
                    .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                        row.get(0)
                    })
                    .unwrap();
                assert_eq!(count, expected, "{table}");
            }
            let source_count: i64 = conn
                .query_row("SELECT count(*) FROM session_v2", [], |row| row.get(0))
                .unwrap();
            assert_eq!(source_count, 2);
        }
    }
}
