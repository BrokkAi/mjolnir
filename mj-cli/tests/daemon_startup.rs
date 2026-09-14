//! A client that launches the daemon must explain why the daemon failed, not
//! that the endpoint the daemon never published is missing.
//!
//! The incident: an auto-upgraded release `mj` launched its daemon against a
//! store already migrated by a newer build. The daemon exited at once with the
//! schema error in `daemon.log`, while the client dialed the absent
//! `daemon.json` for its whole startup budget and then blamed that file.

mod common;

use std::{fs, process::Command};

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
