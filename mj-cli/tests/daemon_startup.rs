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

fn old_store(storage: &common::DaemonStorage) -> std::path::PathBuf {
    let path = current_store(storage);
    let connection = rusqlite::Connection::open(&path).unwrap();
    // 2.15.0's actual shape, not only a changed PRAGMA: migration 44 must
    // create the missing cache and advance the compatibility floor.
    connection
        .execute_batch(
            "DROP TABLE quota_reset_cache;
         DELETE FROM schema_migrations WHERE version > 43;
         UPDATE schema_compatibility SET minimum_compatible_version = 43;
         PRAGMA user_version = 43;",
        )
        .unwrap();
    path
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
                            // A daemon that has it names the work it counts.
                            let blockers = std::env::var("MJ_TEST_OLD_BLOCKERS").ok();
                            if matches!(request.action, DaemonAction::UpgradeBlockers) && blockers.is_none() { break; }
                            // 2.18–2.20 count every open HTTP request as work;
                            // stopping such a daemon cancels none of its own.
                            let only_open_requests = blockers.as_deref().is_some_and(|labels| {
                                labels.split(',').all(|label| label.starts_with("HTTP request"))
                            });
                            let stopping = matches!(request.action, DaemonAction::Stop)
                                || (matches!(request.action, DaemonAction::PrepareUpgrade) && !busy);
                            let reply = match request.action {
                                DaemonAction::Ping => DaemonReply::Pong,
                                DaemonAction::UpgradeBlockers => DaemonReply::UpgradeBlockers(
                                    if busy {
                                        blockers.unwrap().split(',').map(str::to_owned).collect()
                                    } else {
                                        Vec::new()
                                    },
                                ),
                                DaemonAction::Stop => {
                                    assert!(
                                        !busy || only_open_requests,
                                        "automatic upgrade cancelled accepted work"
                                    );
                                    DaemonReply::Done
                                }
                                DaemonAction::PrepareUpgrade => if busy { DaemonReply::UpgradePending } else { DaemonReply::Done },
                                DaemonAction::RuntimeSnapshot { .. } => DaemonReply::RuntimeSnapshot(Box::new(
                                    serde_json::from_value(serde_json::json!({
                                        "revision": 1, "config": mj_core::config::Config::default(),
                                        "records": [], "sessions": [], "lifecycles": []
                                    })).unwrap()
                                )),
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
            .env("MJ_TEST_OLD_DAEMON_VERSION", "2.18.0")
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

/// 2.18.0 through 2.20.0 count an open HTTP request, such as a long
/// `mj wait`, as upgrade work, so their `PrepareUpgrade` refuses until it
/// ends. Later daemons do not count it. The new client must replace such a
/// daemon as soon as open requests are all that remain, and must keep waiting
/// while the daemon names any other work.
#[test]
fn upgrade_from_a_daemon_counting_open_requests_does_not_wait_for_them() {
    use std::time::{Duration, Instant};
    for (version, blockers, replaced) in [
        ("2.19.0", "HTTP request x2", true),
        ("2.20.0", "HTTP request", true),
        ("2.19.0", "HTTP request,session lifecycle", false),
    ] {
        let storage = upgrade_storage();
        let path = old_store(&storage);
        let busy = storage.path().join("work-in-flight");
        fs::write(&busy, "open requests").unwrap();
        let mut old = OldDaemon(
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "old_daemon_fixture", "--nocapture"])
                .env("MJ_TEST_OLD_DAEMON_VERSION", version)
                .env(
                    "MJ_TEST_OLD_PROTOCOL",
                    mj_client::daemon::PROTOCOL_VERSION.to_string(),
                )
                .env("MJ_TEST_OLD_BLOCKERS", blockers)
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
        let mut upgrade = upgrade_command(&storage)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let started = Instant::now();
        if replaced {
            let status = loop {
                if let Some(status) = upgrade.try_wait().unwrap() {
                    break status;
                }
                if started.elapsed() > Duration::from_secs(20) {
                    upgrade.kill().unwrap();
                    upgrade.wait().unwrap();
                    panic!("{version}: the client waited on open requests");
                }
                std::thread::sleep(Duration::from_millis(50));
            };
            let mut stderr = String::new();
            std::io::Read::read_to_string(upgrade.stderr.as_mut().unwrap(), &mut stderr).unwrap();
            assert!(status.success(), "{version}: {stderr}");
            assert!(old.0.wait().unwrap().success());
            assert!(busy.exists(), "the requests were still open");
            assert_upgraded(&path);
        } else {
            while !busy.with_extension("observed").exists() {
                assert!(Instant::now() < deadline, "client did not ask for handoff");
                std::thread::sleep(Duration::from_millis(10));
            }
            std::thread::sleep(Duration::from_secs(2));
            assert!(
                old.0.try_wait().unwrap().is_none(),
                "lifecycle work must still hold the handoff"
            );
            fs::remove_file(&busy).unwrap();
            assert!(upgrade.wait().unwrap().success());
            assert!(old.0.wait().unwrap().success());
            assert_upgraded(&path);
        }
    }
}

#[test]
fn concurrent_upgrade_clients_replace_the_old_daemon_once_and_migrate() {
    use std::time::{Duration, Instant};
    // Same protocol, changed protocol, and a development build whose version
    // did not change: all must converge without a restart command or stdin.
    for (version, protocol, needs_migration) in [
        ("2.15.0", mj_client::daemon::PROTOCOL_VERSION, true),
        ("2.15.0", mj_client::daemon::PROTOCOL_VERSION - 1, true),
        (
            env!("CARGO_PKG_VERSION"),
            mj_client::daemon::PROTOCOL_VERSION,
            true,
        ),
        ("2.9.0", mj_client::daemon::PROTOCOL_VERSION, false),
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
                .map(|_| {
                    let mut command = upgrade_command(&storage);
                    scope.spawn(move || {
                        mj_core::subprocess::run_with_input(&mut command, &[]).unwrap()
                    })
                })
                .collect();
            clients
                .into_iter()
                .map(|client| client.join().unwrap())
                .collect::<Vec<_>>()
        });
        for output in outputs {
            assert!(
                output.status.success(),
                "{version}/{protocol}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        assert!(old.0.wait().unwrap().success());
        assert_upgraded(&path);
        let metadata: mj_client::daemon::DaemonMetadata =
            serde_json::from_slice(&fs::read(&metadata_path).unwrap()).unwrap();
        assert_ne!(metadata.pid, old.0.id());
        assert_eq!(metadata.build_version, env!("CARGO_PKG_VERSION"));
        let output =
            mj_core::subprocess::run_with_input(&mut upgrade_command(&storage), &[]).unwrap();
        assert!(output.status.success());
        let reused: mj_client::daemon::DaemonMetadata =
            serde_json::from_slice(&fs::read(metadata_path).unwrap()).unwrap();
        assert_eq!(reused.pid, metadata.pid, "a ready daemon must be reused");
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
    drop(storage);
}
