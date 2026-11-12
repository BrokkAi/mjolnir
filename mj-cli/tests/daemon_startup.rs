//! A client that launches the daemon must explain why the daemon failed, not
//! that the endpoint the daemon never published is missing.
//!
//! The incident: an auto-upgraded release `mj` launched its daemon against a
//! store already migrated by a newer build. The daemon exited at once with the
//! schema error in `daemon.log`, while the client dialed the absent
//! `daemon.json` for its whole startup budget and then blamed that file.

mod common;

use std::{fs, process::Command};

fn upgrade_storage() -> common::DaemonStorage {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    let config = root.path().join("config");
    fs::create_dir_all(&config).unwrap();
    fs::write(
        config.join("config.toml"),
        "version = 1\n[phone]\nenabled = false\n",
    )
    .unwrap();
    common::DaemonStorage::new(root, config, data)
}

fn upgrade_command(storage: &common::DaemonStorage) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_mj"));
    common::own_test_daemons(&mut command)
        .args(["--instance", "upgrade-test"])
        .env("MJ_DATA_DIR", storage.path().join("data"))
        .env("MJ_CONFIG_DIR", storage.path().join("config"))
        .env("MJOLNIR_NO_UPDATE_CHECK", "1");
    command
}

/// A configured Apple runtime is started by the real daemon startup path.
/// The fake CLI holds its start operation until this test observes a
/// successful management request, proving that the daemon does not wait for
/// the external service before it begins serving.
#[cfg(target_os = "macos")]
#[test]
fn daemon_starts_a_stopped_apple_container_runtime_without_delaying_serving() {
    use std::time::{Duration, Instant};

    let storage = upgrade_storage();
    let data = storage.path().join("data");
    let config = storage.path().join("config");
    fs::create_dir_all(&data).unwrap();
    let tools = storage.path().join("bin");
    fs::create_dir(&tools).unwrap();
    let log = storage.path().join("container-calls.log");
    let started = storage.path().join("container-started");
    let ready = storage.path().join("container-ready");
    let release = storage.path().join("container-release");
    mj_core::test_hooks::install_fake_command(
        &tools,
        "container",
        r#"printf '%s\n' "$*" >> "$MJ_TEST_CONTAINER_LOG"
if [ "$1 $2" = "system status" ]; then
  if [ -f "$MJ_TEST_CONTAINER_READY" ]; then
    exit 0
  fi
  printf 'service is stopped\n' >&2
  exit 1
fi
if [ "$1 $2" = "system start" ] && [ "$3" = "--disable-kernel-install" ]; then
  : > "$MJ_TEST_CONTAINER_STARTED"
  while [ ! -f "$MJ_TEST_CONTAINER_RELEASE" ]; do
    /bin/sleep 0.05
  done
  : > "$MJ_TEST_CONTAINER_READY"
  exit 0
fi
exit 64
"#,
    );
    let path = format!("{}:/usr/bin:/bin:/usr/sbin:/sbin", tools.display());
    let mut daemon_command = Command::new(env!("CARGO_BIN_EXE_mj"));
    common::own_test_daemons(&mut daemon_command)
        .arg("daemon-run")
        .env("MJ_DATA_DIR", &data)
        .env("MJ_CONFIG_DIR", &config)
        .env("PATH", path)
        .env("MJ_TEST_CONTAINER_LOG", &log)
        .env("MJ_TEST_CONTAINER_STARTED", &started)
        .env("MJ_TEST_CONTAINER_READY", &ready)
        .env("MJ_TEST_CONTAINER_RELEASE", &release)
        .env("MJOLNIR_NO_UPDATE_CHECK", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let mut daemon = daemon_command.spawn().unwrap();

    let deadline = Instant::now() + Duration::from_secs(30);
    while !started.exists() {
        assert!(
            daemon.try_wait().unwrap().is_none(),
            "the daemon exited before starting Apple container"
        );
        assert!(
            Instant::now() < deadline,
            "the daemon never asked to start Apple container"
        );
        std::thread::sleep(Duration::from_millis(25));
    }

    // The fake start remains blocked. A management response therefore proves
    // the real daemon startup task ran beside, not inside, its serving path.
    loop {
        let mut status = Command::new(env!("CARGO_BIN_EXE_mj"));
        let output = common::own_test_daemons(&mut status)
            .args(["daemon", "status"])
            .env("MJ_DATA_DIR", &data)
            .env("MJ_CONFIG_DIR", &config)
            .env("MJOLNIR_NO_UPDATE_CHECK", "1")
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        if output.status.success() && String::from_utf8_lossy(&output.stdout).contains(" started ")
        {
            break;
        }
        assert!(
            daemon.try_wait().unwrap().is_none(),
            "the daemon exited before it began serving"
        );
        assert!(
            Instant::now() < deadline,
            "the daemon never began serving while Apple container started"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        !ready.exists(),
        "the fake container service completed before the serving check"
    );

    fs::write(&release, b"").unwrap();
    while !ready.exists() {
        assert!(
            daemon.try_wait().unwrap().is_none(),
            "the daemon exited while starting Apple container"
        );
        assert!(
            Instant::now() < deadline,
            "the daemon never completed Apple container startup"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    // `start` creates `ready` before it exits. The status probe that verifies
    // the started service is a third CLI invocation after that, so wait for it
    // rather than reading concurrently with the daemon's next step.
    loop {
        let calls = fs::read_to_string(&log).unwrap_or_default();
        if calls.lines().count() >= 3 {
            break;
        }
        assert!(
            daemon.try_wait().unwrap().is_none(),
            "the daemon exited before verifying Apple container"
        );
        assert!(
            Instant::now() < deadline,
            "the daemon never verified Apple container after starting it"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    let calls = fs::read_to_string(&log).unwrap();
    assert!(
        calls.starts_with("system status\nsystem start --disable-kernel-install\nsystem status\n"),
        "unexpected container CLI sequence:\n{calls}"
    );

    let stopped = common::own_test_daemons(&mut Command::new(env!("CARGO_BIN_EXE_mj")))
        .args(["daemon", "stop"])
        .env("MJ_DATA_DIR", &data)
        .env("MJ_CONFIG_DIR", &config)
        .env("MJOLNIR_NO_UPDATE_CHECK", "1")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(
        stopped.status.success(),
        "daemon stop failed: {}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    let exit_deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = daemon.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= exit_deadline {
            let _ = daemon.kill();
            let _ = daemon.wait();
            panic!("the daemon did not exit after daemon stop");
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    assert!(status.success(), "daemon exited with {status}");
    drop(storage);
}

/// Run one concurrent upgrade client with its output in a file and a deadline.
///
/// A client whose output pipe a detached daemon inherited never reaches EOF,
/// so waiting on pipes could outlast the whole CI job. Files and a bounded
/// wait turn any such hang into a prompt failure that shows what the client
/// printed.
fn run_client_with_deadline(
    command: &mut Command,
    log: &std::path::Path,
) -> (std::process::ExitStatus, String) {
    use std::time::{Duration, Instant};
    let file = fs::File::create(log).unwrap();
    let mut child = command
        .stdin(std::process::Stdio::null())
        .stdout(file.try_clone().unwrap())
        .stderr(file)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return (status, fs::read_to_string(log).unwrap_or_default());
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "upgrade client did not exit within 180s: {}",
                fs::read_to_string(log).unwrap_or_default()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn old_store(storage: &common::DaemonStorage) -> std::path::PathBuf {
    let path = current_store(storage);
    let connection = rusqlite::Connection::open(&path).unwrap();
    // 2.15.0's actual shape, not only a changed PRAGMA: migration 44 must
    // create the missing cache and advance the compatibility floor.
    // Hard-won: 803c9fe1: rewound upgrade fixtures retained the later mailbox failure column.
    // Hard-won: #1302: the old-store fixture must also remove the notification epoch.
    // Hard-won: 5f5e74082: kept current GitHub watch tables, so migration 85 found no v84 columns to drop.
    // Hard-won: a58ad7eb8: kept migration 116's prompt_history triggers, which name sessions, so migration 68's table rebuild failed.
    // Restore the singleton preference table used before schema 101.
    // Restore the review table before migrations 104 and 106 add these columns.
    // Restore prompt_history before migration 116 adds its triggers, search table and command column.
    // Hard-won: #1187: revision-121 cleanup triggers referenced tables removed by this legacy fixture.
    connection
        .execute_batch(
            "ALTER TABLE session_moves DROP COLUMN source_environment_id;
         ALTER TABLE retained_move_sources DROP COLUMN environment_id;
         DROP TRIGGER IF EXISTS environment_report_registered;
         DROP TRIGGER IF EXISTS environment_report_updated;
         DROP TRIGGER IF EXISTS reclaimed_environment_owner_deleted;
         DROP TRIGGER IF EXISTS reclaimed_environment_binding_deleted;
         DROP TABLE IF EXISTS environment_reports;
         DROP TABLE IF EXISTS environment_bindings;
         DROP TABLE IF EXISTS environment_workers;
         DROP TABLE IF EXISTS environments;
         DROP TRIGGER IF EXISTS session_rows_deleted;
         DROP TRIGGER IF EXISTS session_rows_destroyed;
         DROP TRIGGER IF EXISTS mailbox_outbox_live_target;
         DROP TRIGGER IF EXISTS github_watch_items_live_creator;
         DROP TRIGGER IF EXISTS github_watch_classifications_live_session;
         DROP TRIGGER IF EXISTS github_watch_classification_failures_live_session;
         DROP TRIGGER prompt_history_top_level_only;
         DROP TRIGGER prompt_history_child_registered;
         DROP TRIGGER prompt_history_session_destroyed;
         DROP TRIGGER prompt_history_text_insert;
         DROP TRIGGER prompt_history_text_delete;
         DROP TRIGGER prompt_history_text_update;
         DROP TABLE prompt_history_text;
         DROP INDEX prompt_history_command;
         ALTER TABLE sessions DROP COLUMN no_github_credentials;
         ALTER TABLE prompt_history DROP COLUMN command_id;
         ALTER TABLE turn_review_state DROP COLUMN active_group;
         ALTER TABLE turn_review_state DROP COLUMN push_findings;
         ALTER TABLE turn_review_state DROP COLUMN pending_followup;
         ALTER TABLE turn_review_state DROP COLUMN forward_attempts;
         ALTER TABLE turn_review_state DROP COLUMN declined_findings;
         ALTER TABLE subagent_sessions DROP COLUMN notification_epoch;
         ALTER TABLE mailbox_outbox DROP COLUMN failure;
         DROP TABLE quota_reset_cache;
         DROP TABLE subagent_accounting;
         DROP TABLE session_turn_selections;
         DROP TABLE github_watch_classification_failures;
         DROP TABLE github_seen_items;
         DROP TABLE github_watch_items;
         DROP TABLE github_watch_classifications;
         DROP TABLE github_watch_cursors;
         ALTER TABLE subagent_preference RENAME TO subagent_preference_profiled;
         CREATE TABLE subagent_preference (singleton INTEGER PRIMARY KEY CHECK(singleton = 1), policy TEXT NOT NULL CHECK(json_valid(policy)));
         INSERT INTO subagent_preference(singleton, policy)
             SELECT 1, policy FROM subagent_preference_profiled WHERE profile_id='';
         DROP TABLE subagent_preference_profiled;
         PRAGMA writable_schema=ON;
         UPDATE sqlite_schema SET sql=replace(sql, '''startup-cleanup'',', '')
             WHERE type='table' AND name='sessions';
         PRAGMA writable_schema=RESET;
         DELETE FROM schema_migrations WHERE version > 43;
         UPDATE schema_compatibility SET minimum_compatible_version = 43;
         PRAGMA user_version = 43;",
        )
        .unwrap();
    path
}

/// The `build_version` a daemon of this release publishes when it was built
/// from `revision`, committed `commit_offset` seconds after this build's
/// commit. `None` publishes no commit time.
fn same_release_build(revision: char, commit_offset: Option<i64>) -> String {
    let mut published = format!(
        "{}+{}",
        env!("CARGO_PKG_VERSION"),
        revision.to_string().repeat(40)
    );
    if let Some(offset) = commit_offset {
        let commit: i64 = mj_core::worker_build::BUILD_COMMIT_TIME
            .parse()
            .expect("this test build knows its commit time");
        published.push_str(&format!(".c{}", commit + offset));
    }
    published
}

fn current_store(storage: &common::DaemonStorage) -> std::path::PathBuf {
    let path = storage.path().join("data/mj.sqlite3");
    mj_controller::database::save_state_to(&path, &Default::default()).unwrap();
    mj_controller::database::create_workspace_at(&path, "Keep my workspace").unwrap();
    path
}

struct OldDaemon(std::process::Child);

impl Drop for OldDaemon {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            if let Err(error) = self.0.kill() {
                eprintln!("stop old fixture daemon: {error}");
            }
            if let Err(error) = self.0.wait() {
                eprintln!("reap old fixture daemon: {error}");
            }
        }
    }
}

fn assert_upgraded(path: &std::path::Path) {
    let connection = rusqlite::Connection::open(path).unwrap();
    let revision: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert!(revision >= 44);
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM quota_reset_cache", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT count(*) FROM workspaces WHERE name = 'Keep my workspace'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
}

/// In `ps` the daemon was listed as `/proc/self/exe daemon-run`, so a person
/// could not find it by the name `mj` (launch finding R13-9). It is started
/// through `/proc/self/exe` so that it runs the client's own inode, and now
/// carries `mj` as its program name.
#[cfg(target_os = "linux")]
// Hard-won: 3374ef7ddf: Launch finding R13-9 found `/proc/self/exe daemon-run` made the daemon impossible to find as `mj`; the test checks its published process argv.
#[test]
fn the_daemon_is_listed_as_mj_daemon_run() {
    let storage = upgrade_storage();
    let output = mj_core::subprocess::run_with_input(&mut upgrade_command(&storage), &[]).unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: mj_client::daemon::DaemonMetadata =
        serde_json::from_slice(&fs::read(storage.path().join("data/daemon.json")).unwrap())
            .unwrap();
    let command_line = fs::read(format!("/proc/{}/cmdline", metadata.pid)).unwrap();
    let arguments = command_line
        .split(|byte| *byte == 0)
        .take(2)
        .map(|argument| String::from_utf8_lossy(argument).into_owned())
        .collect::<Vec<_>>();
    assert_eq!(arguments, ["mj", "daemon-run"]);
}

/// A replacement daemon is a new start, and a client proves that from the
/// daemon alone: no PID, no `/proc`, and the same answer on every platform.
#[test]
fn a_daemon_reports_a_fresh_identity_for_each_start() {
    fn identity(storage: &common::DaemonStorage) -> mj_client::daemon::DaemonIdentity {
        let metadata: mj_client::daemon::DaemonMetadata =
            serde_json::from_slice(&fs::read(storage.path().join("data/daemon.json")).unwrap())
                .unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let mut client = mj_client::daemon::DaemonClient::connect(metadata.clone())
                .await
                .unwrap();
            let identity = client.identity().await.unwrap();
            assert_eq!(identity.build_version, metadata.build_version);
            assert_eq!(identity.placement, mj_client::daemon::DaemonPlacement::Host);
            assert!(!identity.instance_id.is_empty());
            // Stored settings come from the daemon, not the file.
            assert!(!client.config().await.unwrap().phone.enabled);
            identity
        })
    }

    let storage = upgrade_storage();
    let output = mj_core::subprocess::run_with_input(&mut upgrade_command(&storage), &[]).unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let before = identity(&storage);

    let output = mj_core::subprocess::run_with_input(
        upgrade_command(&storage).args(["daemon", "restart"]),
        &[],
    )
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let after = identity(&storage);
    assert_ne!(
        before.instance_id, after.instance_id,
        "a restarted daemon must report a new start identity"
    );
}

#[cfg(target_os = "linux")]
// Hard-won: 92b39c9da: suspending the launcher's session deleted the daemon's working directory.
#[test]
fn daemon_working_directory_survives_removal_of_the_launching_checkout() {
    let storage = upgrade_storage();
    let home = storage.path().join("home");
    let checkout = storage.path().join("checkout");
    fs::create_dir(&home).unwrap();
    fs::create_dir(&checkout).unwrap();
    fs::write(home.join(".gitconfig"), "[user]\nname = Test User\n").unwrap();
    let output = mj_core::subprocess::run_with_input(
        upgrade_command(&storage)
            .env("HOME", &home)
            .current_dir(&checkout),
        &[],
    )
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: mj_client::daemon::DaemonMetadata =
        serde_json::from_slice(&fs::read(storage.path().join("data/daemon.json")).unwrap())
            .unwrap();
    let daemon_cwd = std::path::PathBuf::from(format!("/proc/{}/cwd", metadata.pid));
    fs::remove_dir(&checkout).unwrap();
    let output = mj_core::subprocess::run_with_input(
        Command::new("git")
            .args(["config", "--global", "--includes", "--null", "--list"])
            .env("HOME", &home)
            .current_dir(&daemon_cwd),
        &[],
    )
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read_link(daemon_cwd).unwrap(), home);
}

#[test]
fn ordinary_startup_migrates_an_old_store_without_a_daemon() {
    let storage = upgrade_storage();
    let path = old_store(&storage);
    let output = mj_core::subprocess::run_with_input(&mut upgrade_command(&storage), &[]).unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_upgraded(&path);
}

#[test]
fn daemon_cleans_legacy_activity_history_without_removing_session_transcripts() {
    use std::time::{Duration, Instant};

    let storage = upgrade_storage();
    let path = current_store(&storage);
    let mut connection = rusqlite::Connection::open(&path).unwrap();
    connection.execute_batch(
        "PRAGMA foreign_keys=ON;
         INSERT INTO session_contexts(session_id,bundle_id,created_at) VALUES ('kept','project','now');
         INSERT INTO sessions(session_id,title,harness_kind,last_profile,target_template_id,state,updated_at)
             VALUES ('kept','Kept transcript','codex','codex','podman','stopped','now');
         INSERT INTO materialized_sessions(session_id) VALUES ('kept');
         INSERT INTO materialized_transcript_items(session_id,stable_id,position,created_at_ms,last_changed_at_ms,body_json)
             VALUES ('kept','user:1',1,1,1,'{\"kind\":\"user\",\"content\":[]}');"
    ).unwrap();
    let tx = connection.transaction().unwrap();
    for _ in 0..2_003 {
        tx.execute("INSERT INTO api_events(session_id,recorded_at_ms,body) VALUES ('kept',1,?1)",
            [r#"{"type":"activity_changed","data":{"activity":{"state":"running","details":null,"is_idle":false,"waiting_for_input":false,"capacity_retry":false}}}"#]).unwrap();
    }
    let durable = r#"{"type":"session_fault","data":{"reason":"startup_failed","message":"keep this","command_id":null}}"#;
    tx.execute(
        "INSERT INTO api_events(session_id,recorded_at_ms,body) VALUES ('kept',1,?1)",
        [durable],
    )
    .unwrap();
    tx.commit().unwrap();

    let output = mj_core::subprocess::run_with_input(&mut upgrade_command(&storage), &[]).unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let activity: u64 = connection.query_row(
            "SELECT count(*) FROM api_events WHERE json_extract(body,'$.type')='activity_changed'",
            [], |row| row.get(0),
        ).unwrap();
        if activity == 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "daemon left {activity} historical activity events"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    let kept: String = connection
        .query_row("SELECT body FROM api_events WHERE seq=2004", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(kept, durable);
    let transcript: String = connection.query_row(
        "SELECT body_json FROM materialized_transcript_items WHERE session_id='kept' AND stable_id='user:1'",
        [], |row| row.get(0),
    ).unwrap();
    assert_eq!(transcript, r#"{"kind":"user","content":[]}"#);
    let frontier: u64 = connection
        .query_row(
            "SELECT seq FROM sqlite_sequence WHERE name='api_events'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(frontier, 2_004);
}

/// A real process holding the old writer lock, speaking only the frozen
/// management protocol. It deliberately cannot migrate its store. Running
/// the new CLI must retire it before opening a current reader.
#[test]
fn old_daemon_fixture() {
    use mj_client::daemon::*;
    let Ok(version) = std::env::var("MJ_TEST_OLD_DAEMON_VERSION") else {
        return;
    };
    let _guard = mj_controller::controller::ControllerStoreGuard::acquire().unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let metadata = DaemonMetadata {
            protocol_version: std::env::var("MJ_TEST_OLD_PROTOCOL").unwrap().parse().unwrap(),
            pid: std::process::id(),
            address: listener.local_addr().unwrap(),
            token: "isolated-upgrade-fixture".into(),
            started_at: chrono::Utc::now().to_rfc3339(),
            build_version: version,
            instance_id: String::new(),
            placement: DaemonPlacement::Host,
        };
        fs::write(metadata_path(), serde_json::to_vec(&metadata).unwrap()).unwrap();
        let stop = tokio_util::sync::CancellationToken::new();
        let mut connections = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                _ = stop.cancelled() => break,
                Some(result) = connections.join_next(), if !connections.is_empty() => result.unwrap(),
                accepted = listener.accept() => {
                    let (mut stream, _) = accepted.unwrap();
                    let stop = stop.clone();
                    connections.spawn(async move {
                        while let Ok(request) = read_frame::<RequestEnvelope>(&mut stream).await {
                            let busy = std::env::var_os("MJ_TEST_UPGRADE_BUSY_FILE")
                                .is_some_and(|path| std::path::Path::new(&path).exists());
                            if matches!(request.action, DaemonAction::PrepareUpgrade) && busy {
                                let path = std::path::PathBuf::from(std::env::var_os("MJ_TEST_UPGRADE_BUSY_FILE").unwrap());
                                fs::write(path.with_extension("observed"), "waiting").unwrap();
                            }
                            // A daemon that predates `UpgradeBlockers` cannot
                            // decode the frame: it fails the request and drops
                            // the connection. The client must fall back to the
                            // notice that names no work.
                            // A current daemon names its blockers.
                            let blockers = std::env::var("MJ_TEST_UPGRADE_BLOCKERS").ok();
                            if matches!(request.action, DaemonAction::UpgradeBlockers) && blockers.is_none() { break; }
                            let stopping = matches!(request.action, DaemonAction::Stop)
                                || (matches!(request.action, DaemonAction::PrepareUpgrade) && !busy);
                            let reply = match request.action {
                                DaemonAction::Ping => DaemonReply::Pong,
                                DaemonAction::UpgradeBlockers => DaemonReply::UpgradeBlockers(
                                    blockers.iter().flat_map(|named| named.split('|')).map(str::to_owned).collect(),
                                ),
                                DaemonAction::Stop => {
                                    assert!(!busy, "automatic upgrade cancelled accepted work");
                                    DaemonReply::Done
                                }
                                DaemonAction::PrepareUpgrade => if busy { DaemonReply::UpgradePending } else { DaemonReply::Done },
                                // A daemon from before `Identity` or
                                // `Configuration` cannot decode the frame: it
                                // drops the connection. Clients must treat
                                // that as an unknown answer, not as a reason to
                                // keep asking.
                                DaemonAction::Identity | DaemonAction::Configuration => break,
                                other => panic!("new client reused the old daemon: {other:?}"),
                            };
                            write_frame(&mut stream, &ResponseEnvelope {
                                protocol_version: request.protocol_version,
                                request_id: request.request_id,
                                result: Ok(reply),
                            }).await.unwrap();
                            if stopping { stop.cancel(); break; }
                        }
                    });
                }
            }
        }
        connections.abort_all();
        while let Some(result) = connections.join_next().await {
            if let Err(error) = result { assert!(error.is_cancelled(), "{error}"); }
        }
        fs::remove_file(metadata_path()).unwrap();
    });
}

#[test]
fn automatic_upgrade_waits_for_work_then_migrates_without_another_invocation() {
    use std::time::{Duration, Instant};
    let storage = upgrade_storage();
    let path = old_store(&storage);
    let busy = storage.path().join("work-in-flight");
    fs::write(&busy, "accepted work").unwrap();
    let mut old = OldDaemon(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "old_daemon_fixture", "--nocapture"])
            // The database fixture below still has the 2.15.0 schema.
            .env("MJ_TEST_OLD_DAEMON_VERSION", "2.21.0")
            .env(
                "MJ_TEST_OLD_PROTOCOL",
                mj_client::daemon::PROTOCOL_VERSION.to_string(),
            )
            .env("MJ_TEST_UPGRADE_BUSY_FILE", &busy)
            .env("MJ_INSTANCE", "upgrade-test")
            .env("MJ_DATA_DIR", storage.path().join("data"))
            .env("MJ_CONFIG_DIR", storage.path().join("config"))
            .stdin(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    while !storage.path().join("data/daemon.json").exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::scope(|scope| {
        let upgrade = scope.spawn(|| {
            mj_core::subprocess::run_with_input(&mut upgrade_command(&storage), &[]).unwrap()
        });
        while !busy.with_extension("observed").exists() {
            assert!(
                Instant::now() < deadline,
                "client did not ask for safe handoff"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(old.0.try_wait().unwrap().is_none());
        let db = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(
            db.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            43,
            "migration must wait for the old writer's work"
        );
        drop(db);
        fs::remove_file(&busy).unwrap();
        let output = upgrade.join().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    });
    assert!(old.0.wait().unwrap().success());
    assert_upgraded(&path);
}

// Hard-won: #1457: a `daemon.json` left by a daemon that died with its host made the client wait forever for a handoff nothing could accept.
#[test]
fn a_record_whose_daemon_died_does_not_block_the_upgrade() {
    let current = mj_client::daemon::PROTOCOL_VERSION;
    // An older release, an older protocol, and a protocol this client cannot
    // speak: none of them has a process behind it.
    for (version, protocol) in [
        ("2.21.0", current),
        ("2.21.0", current - 1),
        ("999.0.0", current + 1),
    ] {
        let storage = upgrade_storage();
        let path = current_store(&storage);
        // An address that refused connections a moment ago, and the id of a
        // process that has exited.
        let address = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let mut gone = Command::new(env!("CARGO_BIN_EXE_mj"))
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        gone.wait().unwrap();
        let stale = mj_client::daemon::DaemonMetadata {
            protocol_version: protocol,
            pid: gone.id(),
            address,
            token: "died-with-the-host".into(),
            started_at: chrono::Utc::now().to_rfc3339(),
            build_version: version.into(),
            instance_id: String::new(),
            placement: mj_client::daemon::DaemonPlacement::Host,
        };
        let metadata_path = storage.path().join("data/daemon.json");
        fs::write(&metadata_path, serde_json::to_vec(&stale).unwrap()).unwrap();
        let log = storage.path().join("client.log");
        let (status, output) = run_client_with_deadline(&mut upgrade_command(&storage), &log);
        assert!(status.success(), "{version}/{protocol}: {output}");
        let started: mj_client::daemon::DaemonMetadata =
            serde_json::from_slice(&fs::read(&metadata_path).unwrap()).unwrap();
        assert_ne!(started.token, stale.token, "{version}/{protocol}");
        assert!(
            started
                .build_version
                .starts_with(mj_core::worker_build::BUILD_ID),
            "{}",
            started.build_version
        );
        assert!(path.exists());
    }
}

// Hard-won: #1457: a daemon that died while the client waited for it to accept the handoff left the client waiting for ever.
#[test]
fn a_daemon_that_dies_during_the_handoff_wait_is_replaced() {
    use std::time::{Duration, Instant};
    let storage = upgrade_storage();
    current_store(&storage);
    let busy = storage.path().join("work-in-flight");
    fs::write(&busy, "accepted work").unwrap();
    let old = OldDaemon(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "old_daemon_fixture", "--nocapture"])
            .env("MJ_TEST_OLD_DAEMON_VERSION", "2.21.0")
            .env(
                "MJ_TEST_OLD_PROTOCOL",
                mj_client::daemon::PROTOCOL_VERSION.to_string(),
            )
            .env("MJ_TEST_UPGRADE_BUSY_FILE", &busy)
            .env("MJ_INSTANCE", "upgrade-test")
            .env("MJ_DATA_DIR", storage.path().join("data"))
            .env("MJ_CONFIG_DIR", storage.path().join("config"))
            .stdin(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    let metadata_path = storage.path().join("data/daemon.json");
    while !metadata_path.exists() {
        assert!(Instant::now() < deadline, "fixture did not become ready");
        std::thread::sleep(Duration::from_millis(10));
    }
    let stale: mj_client::daemon::DaemonMetadata =
        serde_json::from_slice(&fs::read(&metadata_path).unwrap()).unwrap();
    let log = storage.path().join("client.log");
    let (status, output) = std::thread::scope(|scope| {
        let mut command = upgrade_command(&storage);
        let log = &log;
        let client = scope.spawn(move || run_client_with_deadline(&mut command, log));
        while !busy.with_extension("observed").exists() {
            assert!(
                Instant::now() < deadline,
                "client did not ask for safe handoff"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        // Killed, not stopped: the record stays and the store guard is
        // released by the operating system.
        drop(old);
        client.join().unwrap()
    });
    assert!(status.success(), "{output}");
    let started: mj_client::daemon::DaemonMetadata =
        serde_json::from_slice(&fs::read(&metadata_path).unwrap()).unwrap();
    assert_ne!(started.token, stale.token);
}

// Hard-won: #1457: a container daemon's store guard is in its volume, so a free host guard and an unanswered ping made a live, busy container daemon look gone.
#[test]
fn a_container_daemon_that_does_not_answer_is_not_replaced() {
    use std::time::{Duration, Instant};
    let storage = upgrade_storage();
    current_store(&storage);
    // A busy container daemon: its endpoint takes connections and answers none.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let connections = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    {
        let connections = connections.clone();
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for stream in listener.incoming().flatten() {
                connections.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                held.push(stream);
            }
        });
    }
    // A container daemon's record: PID 0, an older build, and no host store
    // guard, which lives in the container's volume.
    let stale = mj_client::daemon::DaemonMetadata {
        protocol_version: mj_client::daemon::PROTOCOL_VERSION,
        pid: 0,
        address,
        token: "busy-in-its-container".into(),
        started_at: chrono::Utc::now().to_rfc3339(),
        build_version: "2.21.0".into(),
        instance_id: String::new(),
        placement: mj_client::daemon::DaemonPlacement::Host,
    };
    let metadata_path = storage.path().join("data/daemon.json");
    fs::write(&metadata_path, serde_json::to_vec(&stale).unwrap()).unwrap();
    let log = storage.path().join("client.log");
    let mut command = upgrade_command(&storage);
    command
        .stdin(std::process::Stdio::null())
        .stdout(fs::File::create(&log).unwrap())
        .stderr(fs::File::create(&log).unwrap());
    let mut client = command.spawn().unwrap();
    // The handoff asks again each round; several asks with the record in place
    // show that the client waits for the daemon instead of replacing it.
    let deadline = Instant::now() + Duration::from_secs(60);
    while connections.load(std::sync::atomic::Ordering::SeqCst) < 3 {
        assert!(
            client.try_wait().unwrap().is_none(),
            "the client gave up: {}",
            fs::read_to_string(&log).unwrap_or_default()
        );
        assert!(
            Instant::now() < deadline,
            "the client did not ask the daemon"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let kept: mj_client::daemon::DaemonMetadata =
        serde_json::from_slice(&fs::read(&metadata_path).unwrap()).unwrap();
    assert_eq!(kept.token, stale.token, "the record was replaced");
    client.kill().unwrap();
    client.wait().unwrap();
}

#[test]
fn concurrent_clients_replace_obsolete_daemons_once_and_reuse_the_winner() {
    use std::time::{Duration, Instant};
    // Same protocol, changed protocol, and a development build whose version
    // did not change: all must converge without a restart command or stdin.
    let older_build = same_release_build('a', Some(-86_400));
    for (version, protocol, needs_migration) in [
        ("2.21.0", mj_client::daemon::PROTOCOL_VERSION, true),
        ("2.21.0", mj_client::daemon::PROTOCOL_VERSION - 1, true),
        (
            env!("CARGO_PKG_VERSION"),
            mj_client::daemon::PROTOCOL_VERSION,
            true,
        ),
        // Identical release, wire protocol and schema, from a build that
        // published no revision: every such daemon predates build ordering,
        // so it is the older build.
        (
            env!("CARGO_PKG_VERSION"),
            mj_client::daemon::PROTOCOL_VERSION,
            false,
        ),
        // Identical release, wire protocol and schema, built from a commit a
        // day older than this client's.
        (
            older_build.as_str(),
            mj_client::daemon::PROTOCOL_VERSION,
            false,
        ),
        ("2.21.0", mj_client::daemon::PROTOCOL_VERSION, false),
    ] {
        let storage = upgrade_storage();
        let path = if needs_migration {
            old_store(&storage)
        } else {
            current_store(&storage)
        };
        let mut old = OldDaemon(
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "old_daemon_fixture", "--nocapture"])
                .env("MJ_TEST_OLD_DAEMON_VERSION", version)
                .env("MJ_TEST_OLD_PROTOCOL", protocol.to_string())
                .env("MJ_INSTANCE", "upgrade-test")
                .env("MJ_DATA_DIR", storage.path().join("data"))
                .env("MJ_CONFIG_DIR", storage.path().join("config"))
                .stdin(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
        let metadata_path = storage.path().join("data/daemon.json");
        let deadline = Instant::now() + Duration::from_secs(15);
        while !metadata_path.exists() {
            assert!(old.0.try_wait().unwrap().is_none(), "fixture exited");
            assert!(Instant::now() < deadline, "fixture did not become ready");
            std::thread::sleep(Duration::from_millis(20));
        }
        let outputs = std::thread::scope(|scope| {
            let clients: Vec<_> = (0..3)
                .map(|index| {
                    let mut command = upgrade_command(&storage);
                    let log = storage.path().join(format!("client-{index}.log"));
                    scope.spawn(move || run_client_with_deadline(&mut command, &log))
                })
                .collect();
            clients
                .into_iter()
                .map(|client| client.join().unwrap())
                .collect::<Vec<_>>()
        });
        for (status, log) in outputs {
            assert!(status.success(), "{version}/{protocol}: {log}");
        }
        assert!(old.0.wait().unwrap().success());
        assert_upgraded(&path);
        let metadata: mj_client::daemon::DaemonMetadata =
            serde_json::from_slice(&fs::read(&metadata_path).unwrap()).unwrap();
        assert_ne!(metadata.pid, old.0.id());
        assert!(
            metadata
                .build_version
                .starts_with(mj_core::worker_build::BUILD_ID),
            "{}",
            metadata.build_version
        );
        let output =
            mj_core::subprocess::run_with_input(&mut upgrade_command(&storage), &[]).unwrap();
        assert!(output.status.success());
        let reused: mj_client::daemon::DaemonMetadata =
            serde_json::from_slice(&fs::read(metadata_path).unwrap()).unwrap();
        assert_eq!(reused.pid, metadata.pid, "a ready daemon must be reused");
    }
}

#[test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn a_different_same_version_build_cannot_replace_an_incompatible_store_owner() {
    use std::time::{Duration, Instant};
    let storage = upgrade_storage();
    let path = current_store(&storage);
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch(
        "UPDATE schema_compatibility SET minimum_compatible_version = 900;
         INSERT INTO schema_migrations VALUES (900, 'test');
         PRAGMA user_version = 900;",
    )
    .unwrap();
    drop(db);
    let mut old = OldDaemon(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "old_daemon_fixture", "--nocapture"])
            .env("MJ_TEST_OLD_DAEMON_VERSION", env!("CARGO_PKG_VERSION"))
            .env(
                "MJ_TEST_OLD_PROTOCOL",
                mj_client::daemon::PROTOCOL_VERSION.to_string(),
            )
            .env("MJ_INSTANCE", "upgrade-test")
            .env("MJ_DATA_DIR", storage.path().join("data"))
            .env("MJ_CONFIG_DIR", storage.path().join("config"))
            .stdin(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let metadata_path = storage.path().join("data/daemon.json");
    let deadline = Instant::now() + Duration::from_secs(15);
    while !metadata_path.exists() {
        assert!(old.0.try_wait().unwrap().is_none(), "fixture exited");
        assert!(Instant::now() < deadline, "fixture did not become ready");
        std::thread::sleep(Duration::from_millis(20));
    }
    let (status, log) = run_client_with_deadline(
        &mut upgrade_command(&storage),
        &storage.path().join("refusal.log"),
    );
    assert!(!status.success(), "{log}");
    assert!(log.contains("schema 900"), "{log}");
    assert!(old.0.try_wait().unwrap().is_none(), "owner was stopped");
    let retained: mj_client::daemon::DaemonMetadata =
        serde_json::from_slice(&fs::read(metadata_path).unwrap()).unwrap();
    assert_eq!(retained.pid, old.0.id());
    // Release the fixture owner before DaemonStorage tries a real stop command.
    drop(old);
}

/// An older build of the same release never replaces a newer daemon, nor one
/// whose order it cannot establish (campaign finding U-1: alternating builds
/// each replaced the other). Concurrent older clients all use the daemon and
/// say why, and an explicit restart from the older build refuses the
/// downgrade.
// Hard-won: d7250ce227: Finding U-1 found alternating same-release clients replacing each other indefinitely; this checks an older or unordered build leaves the newer daemon running.
#[test]
fn older_same_release_clients_keep_a_newer_or_unordered_daemon() {
    for (daemon_build, expected) in [
        (
            same_release_build('b', Some(86_400)),
            "The daemon's build is newer, so it keeps running and this client uses it.",
        ),
        // A different revision without a published commit time.
        (
            same_release_build('b', None),
            "Mjolnir cannot tell which build is newer, so the daemon keeps running",
        ),
    ] {
        let storage = upgrade_storage();
        current_store(&storage);
        let output =
            mj_core::subprocess::run_with_input(&mut upgrade_command(&storage), &[]).unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let metadata_path = storage.path().join("data/daemon.json");
        let mut metadata: mj_client::daemon::DaemonMetadata =
            serde_json::from_slice(&fs::read(&metadata_path).unwrap()).unwrap();
        // The running daemon now presents itself as the other build.
        metadata.build_version = daemon_build.clone();
        mj_core::config::atomic_write(&metadata_path, &serde_json::to_vec(&metadata).unwrap())
            .unwrap();

        let outputs = std::thread::scope(|scope| {
            let clients: Vec<_> = (0..3)
                .map(|index| {
                    let mut command = upgrade_command(&storage);
                    let log = storage.path().join(format!("older-client-{index}.log"));
                    scope.spawn(move || run_client_with_deadline(&mut command, &log))
                })
                .collect();
            clients
                .into_iter()
                .map(|client| client.join().unwrap())
                .collect::<Vec<_>>()
        });
        for (status, log) in outputs {
            assert!(status.success(), "{daemon_build}: {log}");
            assert!(log.contains(expected), "{daemon_build}: {log}");
            // Both builds are named.
            assert!(log.contains(&daemon_build[..15]), "{log}");
            assert!(
                log.contains(&mj_core::worker_build::BUILD_ID[..15]),
                "{log}"
            );
        }
        let kept: mj_client::daemon::DaemonMetadata =
            serde_json::from_slice(&fs::read(&metadata_path).unwrap()).unwrap();
        assert_eq!(kept.pid, metadata.pid, "{daemon_build} was replaced");
        assert_eq!(kept.build_version, daemon_build);

        let (status, log) = run_client_with_deadline(
            upgrade_command(&storage).args(["daemon", "restart"]),
            &storage.path().join("restart.log"),
        );
        let restarted: mj_client::daemon::DaemonMetadata =
            serde_json::from_slice(&fs::read(&metadata_path).unwrap()).unwrap();
        if expected.starts_with("The daemon's build is newer") {
            assert!(!status.success(), "{log}");
            assert!(log.contains("so it was not restarted"), "{log}");
            assert_eq!(restarted.pid, metadata.pid, "restart downgraded the daemon");
        } else {
            // An explicit restart is how a person settles an unknown order.
            assert!(status.success(), "{log}");
            assert_ne!(restarted.pid, metadata.pid);
        }
    }
}

#[test]
fn a_newer_compatible_daemon_and_store_are_reused_without_downgrading() {
    let storage = upgrade_storage();
    let path = current_store(&storage);
    let output = mj_core::subprocess::run_with_input(&mut upgrade_command(&storage), &[]).unwrap();
    assert!(output.status.success());
    let metadata_path = storage.path().join("data/daemon.json");
    let mut metadata: mj_client::daemon::DaemonMetadata =
        serde_json::from_slice(&fs::read(&metadata_path).unwrap()).unwrap();
    metadata.build_version = "999.0.0".into();
    mj_core::config::atomic_write(&metadata_path, &serde_json::to_vec(&metadata).unwrap()).unwrap();
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection
        .busy_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    let revision: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    connection
        .execute_batch(&format!(
            "BEGIN IMMEDIATE;
        CREATE TABLE future_feature(value TEXT);
        INSERT INTO future_feature VALUES ('preserved');
        INSERT INTO schema_migrations VALUES ({}, 'test');
        PRAGMA user_version = {};
        COMMIT;",
            revision + 1,
            revision + 1
        ))
        .unwrap();
    let output = mj_core::subprocess::run_with_input(&mut upgrade_command(&storage), &[]).unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let reused: mj_client::daemon::DaemonMetadata =
        serde_json::from_slice(&fs::read(&metadata_path).unwrap()).unwrap();
    assert_eq!(reused.pid, metadata.pid);
    assert_eq!(
        connection
            .query_row("SELECT value FROM future_feature", [], |row| row
                .get::<_, String>(0))
            .unwrap(),
        "preserved"
    );
    assert_eq!(
        connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        revision + 1
    );
}

// Hard-won: 2a577807ef: An auto-upgraded daemon exited on a newer store while the CLI waited on absent metadata and blamed `daemon.json`; this checks the daemon’s actual startup error reaches the client.
#[test]
fn client_reports_the_daemon_startup_failure_instead_of_a_missing_endpoint() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    let config = root.path().join("config");
    fs::create_dir_all(&data).unwrap();
    fs::create_dir_all(&config).unwrap();
    fs::write(
        config.join("config.toml"),
        r#"version = 1

[phone]
enabled = false

[profiles.codex]
kind = "codex"
home = "/profiles/codex"
environment = { PATH = "/mjolnir-daemon-startup-test-no-executables" }

[targets.podman]
kind = "local-podman"
image = "ubuntu:24.04"
"#,
    )
    .unwrap();
    let storage = common::DaemonStorage::new(root, config.clone(), data.clone());

    // A store whose compatibility floor lies beyond anything this build knows.
    let connection = rusqlite::Connection::open(data.join("mj.sqlite3")).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE schema_migrations (
                 version INTEGER PRIMARY KEY CHECK(version > 0),
                 applied_at TEXT NOT NULL
             ) STRICT;
             CREATE TABLE schema_compatibility (
                 singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
                 minimum_compatible_version INTEGER NOT NULL
             ) STRICT;
             INSERT INTO schema_migrations(version, applied_at) VALUES (900, 'test');
             INSERT INTO schema_compatibility(singleton, minimum_compatible_version) VALUES (1, 900);
             PRAGMA user_version = 900;",
        )
        .unwrap();
    drop(connection);

    let mut command = Command::new(env!("CARGO_BIN_EXE_mj"));
    common::own_test_daemons(&mut command)
        .args(["checkpoint", "--session", "any"])
        .env("MJ_DATA_DIR", &data)
        .env("MJ_CONFIG_DIR", &config);
    let output = mj_core::subprocess::run_with_input(&mut command, &[]).unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!output.status.success());
    assert!(
        stderr.contains("exited before it was ready"),
        "the client did not notice the daemon exiting:\n{stderr}"
    );
    assert!(
        stderr.contains("Mjolnir database schema 900"),
        "the client did not relay the daemon's own explanation:\n{stderr}"
    );
    assert!(
        stderr.contains("isolated data directory"),
        "the client did not relay how to run this build anyway:\n{stderr}"
    );
    assert!(
        !stderr.contains("daemon.json"),
        "the client blamed the endpoint the daemon never published:\n{stderr}"
    );
    assert!(!data.join("daemon.json").exists());
    assert!(
        stderr.contains(&data.join("logs").display().to_string()) && stderr.contains("mj-daemon-"),
        "the client did not name the actual daemon log:\n{stderr}"
    );
    drop(storage);
}

/// Launch finding R2-14: `mj new` without `--workspace` started the daemon
/// (1.6 s) only to refuse, while `mj acp` refused without starting one. With
/// no daemon running it now refuses at once, still says the instance has no
/// workspace yet, and leaves no daemon behind.
// Hard-won: af2007c7b0: Launch finding R2-14 found `mj new` start a daemon only to reject a missing workspace; this checks refusal happens before daemon startup.
#[test]
fn new_without_a_workspace_refuses_without_starting_a_daemon() {
    let storage = upgrade_storage();
    let data = storage.path().join("data");
    let project = storage.path().join("project");
    fs::create_dir_all(&project).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_mj"));
    let output = common::own_test_daemons(&mut command)
        .args(["new", "--profile", "fake", "--project-directory"])
        .arg(&project)
        .env("MJ_DATA_DIR", &data)
        .env("MJ_CONFIG_DIR", storage.path().join("config"))
        .env("MJOLNIR_NO_UPDATE_CHECK", "1")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("mj new needs --workspace NAME"), "{stderr}");
    assert!(stderr.contains("this instance has none yet"), "{stderr}");
    assert!(stderr.contains("mj workspaces create NAME"), "{stderr}");
    assert!(
        !data.join("daemon.json").exists(),
        "refusing must not start the daemon"
    );
}

/// Launch finding R5-9: `mj new --workspace nosuch` started a stopped daemon
/// (3.4 s) only to refuse the name. Like a missing `--workspace` (R2-14), the
/// refusal now comes from the store, and no daemon is left behind.
// Hard-won: 28f472889b: Launch finding R5-9 found `mj new` start a daemon before refusing an unknown workspace; this checks early refusal and no daemon metadata.
#[test]
fn new_with_an_unknown_workspace_refuses_without_starting_a_daemon() {
    let storage = upgrade_storage();
    let data = storage.path().join("data");
    let project = storage.path().join("project");
    fs::create_dir_all(&project).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_mj"));
    let output = common::own_test_daemons(&mut command)
        .args([
            "new",
            "--workspace",
            "nosuch",
            "--profile",
            "fake",
            "--project-directory",
        ])
        .arg(&project)
        .env("MJ_DATA_DIR", &data)
        .env("MJ_CONFIG_DIR", storage.path().join("config"))
        .env("MJOLNIR_NO_UPDATE_CHECK", "1")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unknown workspace \"nosuch\""), "{stderr}");
    assert!(stderr.contains("this instance has none yet"), "{stderr}");
    assert!(
        !data.join("daemon.json").exists(),
        "refusing must not start the daemon"
    );
    drop(storage);
}

/// Launch finding R5-9: with a daemon running, `mj acp --workspace nosuch`
/// served, found its input closed, and exited 0 without a word. It now checks
/// the name against the running daemon first and exits 1 with the refusal.
// Hard-won: 28f472889b: Launch finding R5-9 found ACP accept/serve an unknown workspace until session creation; this checks the running-daemon path refuses at startup.
#[test]
fn acp_with_an_unknown_workspace_refuses_at_start_when_a_daemon_is_running() {
    let storage = upgrade_storage();
    let data = storage.path().join("data");
    let config = storage.path().join("config");
    fs::create_dir_all(&data).unwrap();
    mj_controller::database::create_workspace_at(&data.join("mj.sqlite3"), "alpha").unwrap();
    let mut daemon = common::own_test_daemons(&mut Command::new(env!("CARGO_BIN_EXE_mj")))
        .arg("daemon-run")
        .env("MJ_DATA_DIR", &data)
        .env("MJ_CONFIG_DIR", &config)
        .env("MJOLNIR_NO_UPDATE_CHECK", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    // Ready once a management round trip succeeds, not merely once the
    // endpoint file exists.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        assert!(
            daemon.try_wait().unwrap().is_none(),
            "the daemon exited before it was ready"
        );
        let mut status = Command::new(env!("CARGO_BIN_EXE_mj"));
        let ready = common::own_test_daemons(&mut status)
            .args(["daemon", "status"])
            .env("MJ_DATA_DIR", &data)
            .env("MJ_CONFIG_DIR", &config)
            .env("MJOLNIR_NO_UPDATE_CHECK", "1")
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        if ready.status.success() && String::from_utf8_lossy(&ready.stdout).contains(" started ") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the daemon never answered"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    let mut acp = Command::new(env!("CARGO_BIN_EXE_mj"));
    let output = common::own_test_daemons(&mut acp)
        .args(["acp", "--workspace", "nosuch"])
        .env("MJ_DATA_DIR", &data)
        .env("MJ_CONFIG_DIR", &config)
        .env("MJOLNIR_NO_UPDATE_CHECK", "1")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    // Stop and reap the daemon this test started before the storage is
    // removed; an unreaped child would look to the storage's cleanup like a
    // daemon that outlived its stop.
    let mut stop = Command::new(env!("CARGO_BIN_EXE_mj"));
    let _ = common::own_test_daemons(&mut stop)
        .args(["daemon", "stop"])
        .env("MJ_DATA_DIR", &data)
        .env("MJ_CONFIG_DIR", &config)
        .env("MJOLNIR_NO_UPDATE_CHECK", "1")
        .stdin(std::process::Stdio::null())
        .output();
    let _ = daemon.kill();
    let _ = daemon.wait();
    drop(storage);
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains("unknown workspace \"nosuch\""), "{stderr}");
    assert!(stderr.contains("alpha"), "{stderr}");
}

/// A daemon handoff waits only for daemon-owned work. Live, a SessionWiki sync
/// held admission while it walked every native session store, and an ordinary
/// startup of a newer build waited minutes for it. A sync is safe to stop and
/// every daemon runs one at startup, so the handoff must go ahead while one is
/// parked mid-pass, and the old daemon must exit without finishing it.
// Hard-won: c87e5e887f: A live handoff took over 23 seconds because SessionWiki sync held admission; this checks the deferrable sync no longer delays handoff.
// Hard-won: 3b15d8840: store release can precede the old process's final shutdown.
#[test]
fn a_handoff_does_not_wait_for_a_sessionwiki_sync_in_flight() {
    use std::time::{Duration, Instant, SystemTime};
    let storage = upgrade_storage();
    // The web viewer's runtime is what starts the startup sync.
    fs::write(
        storage.path().join("config/config.toml"),
        "version = 1\n[phone]\nenabled = true\nbind = \"127.0.0.1:0\"\ntailscale_detect = false\n",
    )
    .unwrap();
    let hooks = storage.path().join("hooks");
    // The sync walks the native stores under HOME; keep it off the real ones.
    let home = storage.path().join("home");
    fs::create_dir_all(&hooks).unwrap();
    fs::create_dir_all(&home).unwrap();
    // The same build an hour older on disk: same release and commit, so only
    // the executable time orders the two, and this test's client is newer.
    let old_binary = storage.path().join("mj-old");
    fs::copy(env!("CARGO_BIN_EXE_mj"), &old_binary).unwrap();
    fs::File::options()
        .write(true)
        .open(&old_binary)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(3600))
        .unwrap();
    let mut old_client = Command::new(&old_binary);
    common::own_test_daemons(&mut old_client)
        .args(["--instance", "upgrade-test"])
        .env("MJ_DATA_DIR", storage.path().join("data"))
        .env("MJ_CONFIG_DIR", storage.path().join("config"))
        .env("MJOLNIR_NO_UPDATE_CHECK", "1")
        .env("HOME", &home)
        .env("MJ_CHAOS_ISOLATED", "1")
        .env("MJ_TEST_HOOK", "sessionwiki_sync_pass")
        .env("MJ_TEST_HOOK_DIR", &hooks);
    let (status, log) = run_client_with_deadline(&mut old_client, &storage.path().join("old.log"));
    assert!(status.success(), "{log}");
    let deadline = Instant::now() + Duration::from_secs(60);
    while !hooks.join("sessionwiki_sync_pass.reached").exists() {
        assert!(
            Instant::now() < deadline,
            "the old daemon did not start its SessionWiki sync"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let metadata_path = storage.path().join("data/daemon.json");
    let old: mj_client::daemon::DaemonMetadata =
        serde_json::from_slice(&fs::read(&metadata_path).unwrap()).unwrap();

    let started = Instant::now();
    let mut new_client = upgrade_command(&storage);
    new_client.env("HOME", &home);
    let (status, log) = run_client_with_deadline(&mut new_client, &storage.path().join("new.log"));
    // Store release admits the new daemon before the old runtime finishes
    // shutting down. Keep the sync parked until that process has exited.
    let old_exited = loop {
        if !common::process_exists(old.pid) {
            break true;
        }
        if started.elapsed() >= Duration::from_secs(30) {
            break false;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let took = started.elapsed();
    // Release the parked pass whatever happened, so nothing waits on it.
    fs::write(hooks.join("sessionwiki_sync_pass.continue"), "").unwrap();
    assert!(status.success(), "{log}");
    assert!(
        log.contains("replacing the daemon"),
        "the newer client replaced the daemon: {log}"
    );
    assert!(
        took < Duration::from_secs(30),
        "the handoff waited {took:?} for a SessionWiki sync: {log}"
    );
    let new: mj_client::daemon::DaemonMetadata =
        serde_json::from_slice(&fs::read(&metadata_path).unwrap()).unwrap();
    assert_ne!(new.pid, old.pid);
    assert!(
        old_exited,
        "the old daemon exited without finishing its sync"
    );
}

/// While a handoff waits, the client replacing the daemon and a client queued
/// behind it both say what the old daemon is still finishing, with its age,
/// instead of a line that names nothing.
// Hard-won: c87e5e887f: The same handoff stall hid its blocker from clients; this checks each waiting client reports the outstanding SessionWiki sync.
#[test]
fn every_client_waiting_on_a_handoff_names_what_it_waits_for() {
    use std::time::{Duration, Instant};
    let storage = upgrade_storage();
    current_store(&storage);
    let busy = storage.path().join("work-in-flight");
    fs::write(&busy, "accepted work").unwrap();
    let mut old = OldDaemon(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "old_daemon_fixture", "--nocapture"])
            .env("MJ_TEST_OLD_DAEMON_VERSION", "2.21.0")
            .env(
                "MJ_TEST_OLD_PROTOCOL",
                mj_client::daemon::PROTOCOL_VERSION.to_string(),
            )
            .env("MJ_TEST_UPGRADE_BUSY_FILE", &busy)
            .env("MJ_TEST_UPGRADE_BLOCKERS", "session lifecycle (1m 05s)")
            .env("MJ_INSTANCE", "upgrade-test")
            .env("MJ_DATA_DIR", storage.path().join("data"))
            .env("MJ_CONFIG_DIR", storage.path().join("config"))
            .stdin(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    while !storage.path().join("data/daemon.json").exists() {
        assert!(Instant::now() < deadline, "fixture did not become ready");
        std::thread::sleep(Duration::from_millis(10));
    }
    let named = "session lifecycle (1m 05s)";
    let replacing_log = storage.path().join("replacing.log");
    let queued_log = storage.path().join("queued.log");
    let logs = std::thread::scope(|scope| {
        let replacing = scope
            .spawn(|| run_client_with_deadline(&mut upgrade_command(&storage), &replacing_log));
        while !busy.with_extension("observed").exists() {
            assert!(
                Instant::now() < deadline,
                "client did not ask for safe handoff"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let queued =
            scope.spawn(|| run_client_with_deadline(&mut upgrade_command(&storage), &queued_log));
        // Both print their first notice after the startup notice delay.
        let deadline = Instant::now() + Duration::from_secs(60);
        while ![&replacing_log, &queued_log]
            .iter()
            .all(|log| fs::read_to_string(log).is_ok_and(|text| text.contains(named)))
        {
            assert!(
                Instant::now() < deadline,
                "a waiting client did not name the blocker: replacing={:?} queued={:?}",
                fs::read_to_string(&replacing_log),
                fs::read_to_string(&queued_log)
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        fs::remove_file(&busy).unwrap();
        [replacing.join().unwrap(), queued.join().unwrap()]
    });
    for (status, log) in logs {
        assert!(status.success(), "{log}");
    }
    let [replacing, queued] = ["replacing.log", "queued.log"]
        .map(|name| fs::read_to_string(storage.path().join(name)).unwrap());
    assert!(
        replacing.contains(&format!("Mjolnir upgrade is waiting for: {named}")),
        "{replacing}"
    );
    assert!(
        queued.contains(&format!(
            "waiting for another client to finish the daemon handoff; the running daemon is finishing: {named}"
        )),
        "{queued}"
    );
    assert!(old.0.wait().unwrap().success());
}

/// `mj review start` reports what the daemon decided. The daemon used to
/// answer 202 as soon as it admitted the action, so the command printed
/// "review started" and exited 0, and the review host's refusal reached only
/// the daemon log. Here the refusal comes from the review host through the
/// daemon's real action loop: a stopped session has no live session for a
/// review to attach to.
// Hard-won: DSH campaign 2026-10-23 (D-9): review start answered accepted before the daemon refused it
#[test]
fn review_start_fails_with_the_reason_the_daemon_refused_it() {
    let storage = upgrade_storage();
    fs::write(
        storage.path().join("config/config.toml"),
        "version = 1\n[phone]\nenabled = true\nbind = \"127.0.0.1:0\"\ntailscale_detect = false\n",
    )
    .unwrap();
    let database = storage.path().join("data/mj.sqlite3");
    fs::create_dir_all(database.parent().unwrap()).unwrap();
    let workspace = mj_controller::database::create_workspace_at(&database, "Review").unwrap();
    let session: mj_core::state::SessionRecord = serde_json::from_value(serde_json::json!({
        "id": "stopped-session", "workspace_id": workspace.id,
        "title": "Stopped session", "harness_kind": "codex",
        "last_profile": "codex", "bundle_id": "review", "target_template_id": "localhost",
        "state": "stopped", "created_at": "2026-10-23T00:00:00Z", "updated_at": "2026-10-23T00:00:00Z"
    }))
    .unwrap();
    let mut state = mj_core::state::State::default();
    state.sessions.insert(session.id.clone(), session);
    mj_controller::database::save_state_to(&database, &state).unwrap();

    let mut client = upgrade_command(&storage);
    common::AgentIsolation::under(storage.path()).apply(&mut client);
    client.args(["review", "start", "--session", "stopped-session"]);
    let (status, log) = run_client_with_deadline(&mut client, &storage.path().join("review.log"));
    assert!(!status.success(), "{log}");
    assert!(
        log.contains("409 Conflict: session stopped-session is not managed; resume the session before requesting review"),
        "{log}"
    );
    assert!(!log.contains("review started"), "{log}");
}

/// Real daemon/control/PTY/harness boundary, including a disconnected login.
#[cfg(unix)]
#[test]
fn daemon_login_terminal_writes_daemon_profile_and_disconnect_kills_harness_group() {
    use mj_client::daemon::login_terminal::{TerminalInput, TerminalOutput};
    use mj_client::daemon::{DaemonClient, DaemonMetadata, read_frame, write_frame};
    use mj_core::config::{Config, HarnessKind, HarnessProfile};
    use std::time::{Duration, Instant};
    let storage = upgrade_storage();
    let user = storage.path().join("user");
    let tools = storage.path().join("tools");
    let home = user.join(".config/opencode");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&tools).unwrap();
    mj_core::test_hooks::install_fake_command(
        &tools,
        "opencode",
        r#"
if [ "$1 $2" != 'auth login' ]; then exit 1; fi
login_home="$XDG_CONFIG_HOME/opencode"
if [ -e "$login_home/cancel" ]; then
  sleep 600 & child=$!
  printf '%s' "$child" > "$login_home/child"
  printf 'waiting'
  wait
else
  head -c 131072 /dev/zero | tr '\000' x
  printf '\nCode: '
  read -r code
  stty size > "$login_home/size"
  printf '%s' "$code" > "$login_home/credential"
  printf 'saved\n'
fi
"#,
    );
    let path = format!("{}:/usr/bin:/bin", tools.display());
    let mut config = Config::default();
    config.phone.enabled = false;
    let mut profile = HarnessProfile::new(HarnessKind::OpenCode, home.clone());
    profile.environment.insert("PATH".into(), path.clone());
    config.profiles.insert("fixture".into(), profile);
    config
        .save_to(&storage.path().join("config/config.toml"))
        .unwrap();
    let mut command = upgrade_command(&storage);
    common::AgentIsolation::under(storage.path()).apply(&mut command);
    command
        .arg("daemon-run")
        .env("HOME", &user)
        .env("XDG_CONFIG_HOME", user.join(".config"))
        .env("XDG_DATA_HOME", user.join(".local/share"))
        .env("PATH", path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(fs::File::create(storage.path().join("login-daemon.log")).unwrap());
    let mut daemon = command.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let metadata = loop {
        if let Ok(bytes) = fs::read(storage.path().join("data/daemon.json"))
            && let Ok(metadata) = serde_json::from_slice::<DaemonMetadata>(&bytes)
        {
            break metadata;
        }
        assert!(
            daemon.try_wait().unwrap().is_none(),
            "login test daemon exited"
        );
        assert!(
            Instant::now() < deadline,
            "login test daemon failed to publish metadata"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        tokio::time::timeout(Duration::from_secs(30), async {
            for mode in 0..3 {
                let disconnect = mode > 0;
                if disconnect {
                    fs::write(home.join("cancel"), "").unwrap();
                }
                let client = DaemonClient::connect(metadata.clone()).await.unwrap();
                let mut terminal = client
                    .login_terminal("fixture".into(), 80, 24)
                    .await
                    .unwrap();
                let mut transcript = Vec::new();
                loop {
                    match read_frame::<TerminalOutput>(&mut terminal.stream)
                        .await
                        .unwrap()
                    {
                        TerminalOutput::Bytes(bytes) => transcript.extend(bytes),
                        other => panic!(
                            "login failed before prompt: {other:?}; {}",
                            String::from_utf8_lossy(&transcript)
                        ),
                    }
                    if transcript.ends_with(if disconnect { b"waiting" } else { b"Code: " }) {
                        break;
                    }
                }
                if disconnect {
                    let child: i32 = fs::read_to_string(home.join("child"))
                        .unwrap()
                        .parse()
                        .unwrap();
                    if mode == 1 {
                        // Fill the PTY input while the harness is not reading;
                        // disconnect must still reach the process-group owner.
                        write_frame(
                            &mut terminal.stream,
                            &TerminalInput::Bytes(vec![b'x'; 131072]),
                        )
                        .await
                        .unwrap();
                    }
                    if mode == 2 {
                        let reply = DaemonClient::connect(metadata.clone())
                            .await
                            .unwrap()
                            .request(mj_client::daemon::DaemonAction::PrepareUpgrade)
                            .await
                            .unwrap();
                        assert!(matches!(reply, mj_client::daemon::DaemonReply::Done));
                        loop {
                            match read_frame::<TerminalOutput>(&mut terminal.stream).await {
                                Ok(TerminalOutput::Failed(message)) => {
                                    assert!(message.contains("retry Login"));
                                    break;
                                }
                                Ok(TerminalOutput::Bytes(_)) => {}
                                // The daemon aborts disposable connection tasks on
                                // handoff. The pane renders EOF as a retry notice.
                                Err(error) => {
                                    let error = error
                                        .root_cause()
                                        .downcast_ref::<std::io::Error>()
                                        .expect("handoff connection I/O error");
                                    assert!(matches!(
                                        error.kind(),
                                        std::io::ErrorKind::UnexpectedEof
                                            | std::io::ErrorKind::ConnectionReset
                                    ));
                                    break;
                                }
                                other => panic!("handoff did not end terminal: {other:?}"),
                            }
                        }
                    }
                    drop(terminal);
                    loop {
                        #[cfg(target_os = "linux")]
                        let alive = fs::read_to_string(format!("/proc/{child}/stat"))
                            .is_ok_and(|state| state.split_whitespace().nth(2) != Some("Z"));
                        #[cfg(not(target_os = "linux"))]
                        let alive = unsafe { libc::kill(child, 0) == 0 };
                        if !alive {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                } else {
                    assert!(transcript.len() > 131072);
                    write_frame(
                        &mut terminal.stream,
                        &TerminalInput::Resize {
                            cols: 100,
                            rows: 40,
                        },
                    )
                    .await
                    .unwrap();
                    write_frame(
                        &mut terminal.stream,
                        &TerminalInput::Bytes(b"daemon-code\n".to_vec()),
                    )
                    .await
                    .unwrap();
                    loop {
                        match read_frame::<TerminalOutput>(&mut terminal.stream)
                            .await
                            .unwrap()
                        {
                            TerminalOutput::Bytes(bytes) => transcript.extend(bytes),
                            TerminalOutput::Exited { code } => {
                                assert_eq!(code, 0, "{}", String::from_utf8_lossy(&transcript));
                                break;
                            }
                            TerminalOutput::Failed(error) => panic!("{error}"),
                        }
                    }
                    assert_eq!(
                        fs::read_to_string(home.join("credential")).unwrap(),
                        "daemon-code"
                    );
                    assert_eq!(
                        fs::read_to_string(home.join("size")).unwrap().trim(),
                        "40 100"
                    );
                    assert!(String::from_utf8_lossy(&transcript).contains("saved"));
                }
            }
        })
        .await
        .expect("daemon login integration timed out");
    });
    daemon.wait().unwrap();
}
