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
    connection
        .execute_batch(
            "DROP TABLE quota_reset_cache;
         DROP TABLE subagent_accounting;
         DROP TABLE session_turn_selections;
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
        !common::process_exists(old.pid),
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
