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

#[test]
fn setup_reruns_without_questions_and_preserves_existing_accounts() {
    let root = tempfile::tempdir().unwrap();
    let config_directory = root.path().join("setup-config");
    let data_directory = root.path().join("setup-data");
    let tools = root.path().join("tools");
    std::fs::create_dir_all(&tools).unwrap();
    std::fs::create_dir_all(root.path().join(".codex")).unwrap();
    mj_core::test_hooks::install_fake_command(&tools, "codex", "#!/bin/sh\nexit 0\n");
    mj_core::test_hooks::install_fake_command(&tools, "claude", "#!/bin/sh\nexit 1\n");
    for run in 0..2 {
        if run == 1 {
            std::fs::create_dir_all(root.path().join(".claude")).unwrap();
        }
        let output = isolated_command(root.path())
            .args(["--instance", "setup-rerun", "setup"])
            .env("MJ_CONFIG_DIR", &config_directory)
            .env("MJ_DATA_DIR", &data_directory)
            .env("PATH", &tools)
            .env("CODEX_HOME", root.path().join(".codex"))
            .env("CLAUDE_CONFIG_DIR", root.path().join(".claude"))
            .env_remove("KIMI_CODE_HOME")
            .env_remove("GROK_HOME")
            .env_remove("MUSE_HOME")
            .current_dir(root.path())
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("Welcome to Mjolnir"), "{stdout}");
        assert!(stdout.contains("mj login"), "{stdout}");
        assert!(!stdout.contains("Write this configuration"), "{stdout}");
        assert!(!stdout.contains("Container image ["), "{stdout}");
        let config =
            mj_core::config::Config::load_from(&config_directory.join("config.toml")).unwrap();
        assert_eq!(config.profiles.len(), run + 1);
        assert!(config.profiles.contains_key("codex"));
        assert_eq!(
            std::fs::read(data_directory.join("setup-state")).unwrap(),
            b"complete\n"
        );
    }
    assert!(
        !data_directory.join("daemon.json").exists(),
        "explicit discovery starts no daemon"
    );
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

    // No daemon runs for this instance: that is the ordinary stopped state,
    // and the lookup itself must point there.
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.starts_with("Mjolnir daemon is stopped"), "{stdout}");
    for needle in ["instances", "dev", "daemon.json"] {
        assert!(
            stdout.contains(needle),
            "daemon lookup does not mention the instance path; stdout: {stdout}"
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

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("instances") && stdout.contains("envdev") && stdout.contains("daemon.json"),
        "daemon lookup does not mention the instance path; stdout: {stdout}"
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
