//! Real worker entrypoints must clean inherited state before invoking Git.
#![cfg(unix)]

use hel::hel_targets::{BoundedProcessExecutor, CommandExecutor, CommandSpec};
use std::time::Duration;

#[test]
fn worker_git_uses_the_requested_repository_despite_a_polluted_launcher() {
    let root = tempfile::tempdir().unwrap();
    let executor = BoundedProcessExecutor::new(Duration::from_secs(15));
    let git = |args: &[&str]| {
        let mut command = CommandSpec::new("git", args.iter().copied());
        command.cwd = Some(root.path().into());
        command.clear_env = true;
        command.env.extend([
            ("PATH".into(), "/usr/bin:/bin".into()),
            ("HOME".into(), root.path().to_str().unwrap().into()),
            ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
            ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
        ]);
        let output = executor.execute(&command).unwrap();
        assert_eq!(
            output.status,
            0,
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "-q"]);
    std::fs::write(root.path().join("file"), "before\n").unwrap();
    git(&["add", "file"]);
    git(&[
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.invalid",
        "-c",
        "commit.gpgsign=false",
        "commit",
        "-qm",
        "initial",
    ]);
    std::fs::write(root.path().join("file"), "after\n").unwrap();

    let mut command = CommandSpec::new(
        env!("CARGO_BIN_EXE_mj-worker"),
        [
            "worker",
            "diff",
            "--repository",
            root.path().to_str().unwrap(),
            "--base",
            "HEAD",
        ],
    );
    command.env.extend([
        (
            "HOME".into(),
            root.path().join("wrong-home").to_str().unwrap().into(),
        ),
        ("SHELL".into(), "/bin/false".into()),
        ("PATH".into(), "/nonexistent-parent-path".into()),
        ("GIT_DIR".into(), "/nonexistent-parent-repository".into()),
        (
            "GIT_WORK_TREE".into(),
            "/nonexistent-parent-worktree".into(),
        ),
        ("CARGO_TARGET_DIR".into(), "/parent/build".into()),
        ("RUSTFLAGS".into(), "parent flags".into()),
    ]);
    let output = executor.execute(&command).unwrap();
    assert_eq!(
        output.status,
        0,
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let diff = String::from_utf8(output.stdout).unwrap();
    assert!(diff.contains("-before\n+after"), "expected repository diff");
}

#[test]
fn checkpoint_worker_remains_visible_and_stoppable_after_clean_reexec() {
    use anyhow::{Context, Result, ensure};
    use std::time::Instant;
    let root = tempfile::tempdir().unwrap();
    let worker_root = root.path().join("worker");
    drop(
        hel::hel_worker::DurableRelay::open(
            &worker_root,
            "018f9dd2-a3b4-7c8d-9000-123456789abc",
            "test",
        )
        .unwrap(),
    );
    let executable = root.path().join("hel");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_mj-worker"), &executable).unwrap();
    let config = root.path().join("launch.json");
    std::fs::write(
        &config,
        serde_json::to_vec(&serde_json::json!({
            "run_mode": "checkpoint_only", "session_id": "018f9dd2-a3b4-7c8d-9000-123456789abc", "harness": "codex",
            "bridge_command": "/missing-harness", "bridge_args": [],
            "environment": {"CODEX_HOME": root.path().join("profile")},
            "cwd": root.path(), "execution_policy": "configured_approvals"
        }))
        .unwrap(),
    )
    .unwrap();
    let mut command = CommandSpec::new(
        executable.to_str().unwrap(),
        [
            "worker",
            "run",
            "--root",
            worker_root.to_str().unwrap(),
            "--config",
            config.to_str().unwrap(),
        ],
    );
    command
        .env
        .insert("SHELL".into(), "/missing-parent-shell".into());
    let worker = std::thread::spawn(move || {
        BoundedProcessExecutor::new(Duration::from_secs(15)).execute(&command)
    });
    let executor = BoundedProcessExecutor::new(Duration::from_secs(8));
    let observation = (|| -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !worker_root.join("control.sock").exists() {
            ensure!(Instant::now() < deadline, "checkpoint worker did not start");
            std::thread::sleep(Duration::from_millis(25));
        }
        let script = hel::hel_targets::worker_daemon_liveness_script(worker_root.to_str().unwrap());
        let output = executor.execute(&CommandSpec::new("sh", ["-c", &script]))?;
        ensure!(
            output.status == 0 && output.stdout == b"alive\n",
            "re-executed worker lost its lifecycle identity"
        );
        Ok(())
    })();
    // Always stop/join before dropping the worker's files, even if observation
    // failed. The bounded owner also cleans up if the identity check regresses.
    let script = hel::hel_targets::stop_worker_daemon_script(worker_root.to_str().unwrap());
    let stopped = executor.execute(&CommandSpec::new("sh", ["-c", &script]));
    let output = worker.join().expect("worker owner panicked");
    observation.unwrap();
    assert_eq!(stopped.unwrap().status, 0);
    output
        .context("lifecycle stop did not terminate the worker before its deadline")
        .unwrap();
}
