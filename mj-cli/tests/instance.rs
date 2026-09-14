#![cfg(unix)]
//! `mj --instance` keeps configuration, database, daemon, and logs in an
//! isolated `instances/<name>` subtree. These tests run the built binary with
//! its home directories pointed at a temporary root, so they never touch real
//! state. `daemon status` is used because it only reads daemon metadata and
//! never starts a daemon.

use std::path::{Path, PathBuf};
use std::process::Command;

fn isolated_command(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_mj"));
    // If a daemon were ever started by accident, tie its lifetime to this test process.
    command.env("MJ_DAEMON_OWNER_PID", std::process::id().to_string());
    // `dirs` prefers XDG locations on Linux and falls back to HOME elsewhere;
    // pin both so the test is hermetic on either platform.
    command
        .env("HOME", root)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env_remove("MJ_CONFIG_DIR")
        .env_remove("MJ_DATA_DIR")
        .env_remove("MJ_INSTANCE");
    command
}

/// Whether a discovered path is the instance tree itself, one of its ancestors
/// (created as parents by `create_dir_all`), or something inside it. Anything
/// else under `mjolnir/` is state that leaked out of the instance.
fn under_instance(path: &str, instance: &str) -> bool {
    let Some((_, rest)) = path.split_once("mjolnir/") else {
        return true;
    };
    let expected = format!("instances/{instance}");
    rest.starts_with(&expected) || expected.starts_with(rest)
}

fn all_paths(root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries {
            let path: PathBuf = entry.unwrap().path();
            found.push(path.to_string_lossy().replace('\\', "/"));
            if path.is_dir() {
                stack.push(path);
            }
        }
    }
    found
}

#[test]
fn instance_flag_isolates_state_under_instances_subdirectory() {
    let root = tempfile::tempdir().unwrap();
    let output = isolated_command(root.path())
        .args(["-i", "dev", "daemon", "status"])
        .output()
        .unwrap();

    // No daemon runs for this instance, but the lookup itself must point there.
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    for needle in ["instances", "dev", "daemon.json"] {
        assert!(
            stderr.contains(needle),
            "daemon lookup does not mention the instance path; stderr: {stderr}"
        );
    }

    // Even the run's own log file lands inside the instance; the default
    // locations stay untouched.
    let paths = all_paths(root.path());
    assert!(
        paths.iter().any(|path| path.contains("instances/dev/logs")),
        "no instance log directory was created; found: {paths:?}"
    );
    for path in &paths {
        assert!(
            under_instance(path, "dev"),
            "state escaped the instance directory: {path}"
        );
    }
}

#[test]
fn instance_environment_variable_isolates_without_the_flag() {
    let root = tempfile::tempdir().unwrap();
    let output = isolated_command(root.path())
        .env("MJ_INSTANCE", "envdev")
        .args(["daemon", "status"])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("instances") && stderr.contains("envdev") && stderr.contains("daemon.json"),
        "daemon lookup does not mention the instance path; stderr: {stderr}"
    );
    let paths = all_paths(root.path());
    assert!(
        paths
            .iter()
            .any(|path| path.contains("instances/envdev/logs")),
        "no instance log directory was created; found: {paths:?}"
    );
    for path in &paths {
        assert!(
            under_instance(path, "envdev"),
            "state escaped the instance directory: {path}"
        );
    }
}

#[test]
fn invalid_instance_name_fails_before_touching_any_state() {
    let root = tempfile::tempdir().unwrap();
    let output = isolated_command(root.path())
        .args(["-i", "../evil", "daemon", "status"])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("invalid instance id"),
        "expected an instance validation error; stderr: {stderr}"
    );
    // Validation runs before logging starts, so nothing may be created.
    assert_eq!(all_paths(root.path()), Vec::<String>::new());
}
