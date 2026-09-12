//! The incident from issue #24, end to end: another process migrates the
//! daemon's store while the daemon is live.
//!
//! Before the fix the daemon stayed up indefinitely -- refusing every read,
//! writing through a connection whose schema check had passed once, and
//! warning twice a second. It must now notice and leave.

mod common;

use std::{
    fs,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

const METADATA_WAIT: Duration = Duration::from_secs(8);
const EXIT_WAIT: Duration = Duration::from_secs(10);

/// A daemon under test is a real process. Kill it however the test ends.
struct ReapChild(Option<Child>);

impl ReapChild {
    fn child_mut(&mut self) -> &mut Child {
        self.0.as_mut().expect("child already reaped")
    }
}

impl Drop for ReapChild {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut()
            && child.try_wait().ok().flatten().is_none()
        {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn configured_storage() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let storage = tempfile::tempdir().expect("create Mjolnir test storage");
    let config_directory = storage.path().join("config/mjolnir");
    let data_directory = storage.path().join("data/mjolnir");
    fs::create_dir_all(&config_directory).expect("create Mjolnir config directory");
    fs::write(
        config_directory.join("config.toml"),
        r#"version = 1

[phone]
enabled = false

[profiles.codex]
kind = "codex"
home = "/profiles/codex"
# Keep this test independent from any Codex installation on the host.
environment = { PATH = "/mjolnir-store-divergence-test-no-executables" }

[targets.podman]
kind = "local-podman"
image = "ubuntu:24.04"
"#,
    )
    .expect("write Mjolnir test config");
    (storage, config_directory, data_directory)
}

#[test]
fn daemon_exits_when_its_store_is_migrated_underneath_it() {
    let (_storage, config_directory, data_directory) = configured_storage();

    // No MJ_DAEMON_EXIT_WHEN_IDLE: an idle exit would end this process for a
    // reason that has nothing to do with the store.
    let mut daemon = ReapChild(Some(
        common::own_test_daemons(&mut Command::new(env!("CARGO_BIN_EXE_mj")))
            .arg("daemon-run")
            .env("MJ_CONFIG_DIR", &config_directory)
            .env("MJ_DATA_DIR", &data_directory)
            .env_remove("MJ_DAEMON_EXIT_WHEN_IDLE")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn the Mjolnir daemon"),
    ));

    // Metadata is only discovery. Readiness requires a complete management
    // round trip, which also proves the event loop is accepting requests after
    // all fallible store and runtime initialization has completed.
    let metadata = data_directory.join("daemon.json");
    let deadline = Instant::now() + METADATA_WAIT;
    while Instant::now() < deadline && !metadata.exists() {
        assert!(
            daemon
                .child_mut()
                .try_wait()
                .expect("poll the daemon")
                .is_none(),
            "the daemon exited before it was ready"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(metadata.exists(), "the daemon never published its endpoint");
    let mut readiness = ReapChild(Some(
        common::own_test_daemons(&mut Command::new(env!("CARGO_BIN_EXE_mj")))
            .args(["daemon", "status"])
            .env("MJ_CONFIG_DIR", &config_directory)
            .env("MJ_DATA_DIR", &data_directory)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("probe Mjolnir daemon readiness"),
    ));
    let deadline = Instant::now() + METADATA_WAIT;
    let readiness_status = loop {
        if let Some(status) = readiness
            .child_mut()
            .try_wait()
            .expect("poll the daemon readiness probe")
        {
            break status;
        }
        assert!(
            daemon
                .child_mut()
                .try_wait()
                .expect("poll the daemon")
                .is_none(),
            "the daemon exited before answering its readiness probe"
        );
        assert!(
            Instant::now() < deadline,
            "the daemon published metadata but did not answer within {METADATA_WAIT:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(
        readiness_status.success(),
        "the daemon rejected its readiness probe: {readiness_status:?}"
    );

    // What another Mjolnir build's migration ladder does to a store this daemon
    // has open.
    let store = data_directory.join("mj.sqlite3");
    let connection = rusqlite::Connection::open(&store).expect("open the daemon's store");
    let version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("read the store's schema version");
    connection
        .execute_batch(&format!("PRAGMA user_version = {};", version + 1))
        .expect("migrate the store underneath the daemon");
    drop(connection);

    let deadline = Instant::now() + EXIT_WAIT;
    let status = loop {
        if let Some(status) = daemon.child_mut().try_wait().expect("poll the daemon") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "the daemon is still running {EXIT_WAIT:?} after its store moved underneath it"
        );
        std::thread::sleep(Duration::from_millis(50));
    };

    assert!(
        status.success() || status.code() == Some(1),
        "the daemon left abnormally: {status:?}"
    );
    assert!(
        !metadata.exists(),
        "the daemon left its metadata behind, so clients keep dialing a dead address"
    );
}

#[cfg(feature = "test-hooks")]
#[test]
fn post_publication_error_runs_the_daemon_epilogue() {
    let (storage, config_directory, data_directory) = configured_storage();
    let invalid_hook_directory = storage.path().join("hook-path-is-a-file");
    fs::write(&invalid_hook_directory, "not a directory")
        .expect("create invalid test hook directory");
    let metadata = data_directory.join("daemon.json");
    let mut daemon = ReapChild(Some(
        common::own_test_daemons(&mut Command::new(env!("CARGO_BIN_EXE_mj")))
            .arg("daemon-run")
            .env("MJ_CONFIG_DIR", &config_directory)
            .env("MJ_DATA_DIR", &data_directory)
            .env("MJ_TEST_HOOK", "daemon_metadata_before_listening")
            .env("MJ_TEST_HOOK_DIR", &invalid_hook_directory)
            .env("MJ_CHAOS_ISOLATED", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn the Mjolnir daemon at its publication hook"),
    ));

    let deadline = Instant::now() + EXIT_WAIT;
    let status = loop {
        if let Some(status) = daemon.child_mut().try_wait().expect("poll the daemon") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "the daemon did not leave after its publication hook failed"
        );
        std::thread::sleep(Duration::from_millis(50));
    };

    assert!(!status.success(), "the failing hook reported success");
    assert!(
        !metadata.exists(),
        "the failing post-publication path left daemon metadata behind"
    );
}

/// Discovery can already be gone while the previous writer is still exiting.
/// Concurrent clients must wait for ownership, then join one replacement.
#[test]
fn concurrent_starts_wait_for_controller_ownership_before_launching() {
    let (_storage, config_directory, data_directory) = configured_storage();
    let _storage =
        common::DaemonStorage::new(_storage, config_directory.clone(), data_directory.clone());
    fs::create_dir_all(&data_directory).unwrap();
    let owner = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(data_directory.join("controller.lock"))
        .unwrap();
    owner.lock().unwrap();

    let mut clients = (0..2)
        .map(|_| {
            ReapChild(Some(
                common::own_test_daemons(&mut Command::new(env!("CARGO_BIN_EXE_mj")))
                    .args(["daemon", "restart"])
                    .env("MJ_CONFIG_DIR", &config_directory)
                    .env("MJ_DATA_DIR", &data_directory)
                    .env_remove("MJ_DEV_RESTART_STALE_DAEMON")
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .unwrap(),
            ))
        })
        .collect::<Vec<_>>();
    // Let both clients reach startup while metadata is absent. No child may
    // be launched until it can own the controller store.
    let deadline = Instant::now() + METADATA_WAIT;
    while !data_directory.join("daemon-start.lock").exists() {
        assert!(Instant::now() < deadline, "clients never entered startup");
        assert!(
            clients
                .iter_mut()
                .all(|client| client.child_mut().try_wait().unwrap().is_none())
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        !data_directory.join("daemon.log").exists(),
        "launched a child before controller ownership was available"
    );
    assert!(!data_directory.join("daemon.json").exists());
    owner.unlock().unwrap();
    drop(owner);

    for client in &mut clients {
        let deadline = Instant::now() + METADATA_WAIT;
        loop {
            if let Some(status) = client.child_mut().try_wait().unwrap() {
                assert!(status.success(), "startup client failed: {status}");
                break;
            }
            assert!(Instant::now() < deadline, "startup client did not finish");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    assert!(data_directory.join("daemon.json").exists());
    let log = fs::read_to_string(data_directory.join("daemon.log")).unwrap();
    assert!(
        !log.contains("another Mjolnir controller is already using"),
        "{log}"
    );
}

/// Fixture teardown cannot run when a test process dies without unwinding, so
/// a daemon started for a test must leave when the process it belongs to does.
#[test]
fn daemon_exits_when_its_owner_process_exits() {
    let (_storage, config_directory, data_directory) = configured_storage();

    let mut owner = ReapChild(Some(
        Command::new("sleep")
            .arg("300")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn the owner process"),
    ));
    let owner_pid = owner.child_mut().id();

    let mut daemon = ReapChild(Some(
        Command::new(env!("CARGO_BIN_EXE_mj"))
            .arg("daemon-run")
            .env("MJ_CONFIG_DIR", &config_directory)
            .env("MJ_DATA_DIR", &data_directory)
            .env("MJ_DAEMON_OWNER_PID", owner_pid.to_string())
            .env_remove("MJ_DAEMON_EXIT_WHEN_IDLE")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn the Mjolnir daemon"),
    ));

    let metadata = data_directory.join("daemon.json");
    let deadline = Instant::now() + METADATA_WAIT;
    while Instant::now() < deadline && !metadata.exists() {
        assert!(
            daemon
                .child_mut()
                .try_wait()
                .expect("poll the daemon")
                .is_none(),
            "the daemon exited before it was ready"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(metadata.exists(), "the daemon never published its endpoint");

    owner.child_mut().kill().expect("kill the owner process");
    owner.child_mut().wait().expect("reap the owner process");

    let deadline = Instant::now() + EXIT_WAIT;
    let status = loop {
        if let Some(status) = daemon.child_mut().try_wait().expect("poll the daemon") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "the daemon outlived the process that owned it"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(
        status.success(),
        "owner-triggered shutdown failed: {status}"
    );
    assert!(
        !metadata.exists(),
        "owner-triggered shutdown left daemon metadata behind"
    );
}

#[test]
fn an_unusable_owner_pid_is_a_startup_error() {
    let (_storage, config_directory, data_directory) = configured_storage();

    let output = mj_core::subprocess::run_with_input(
        Command::new(env!("CARGO_BIN_EXE_mj"))
            .arg("daemon-run")
            .env("MJ_CONFIG_DIR", &config_directory)
            .env("MJ_DATA_DIR", &data_directory)
            .env("MJ_DAEMON_OWNER_PID", "notanumber")
            .env_remove("MJ_DAEMON_EXIT_WHEN_IDLE"),
        &[],
    )
    .expect("run the daemon with a bad owner pid");

    assert!(!output.status.success(), "a bad owner pid started a daemon");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("MJ_DAEMON_OWNER_PID"),
        "the failure did not name the variable: {stderr}"
    );
    assert!(
        !data_directory.join("daemon.json").exists(),
        "the rejected daemon still published its endpoint"
    );
}
