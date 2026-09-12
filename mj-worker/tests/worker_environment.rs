//! Real worker entrypoints must clean inherited state before invoking Git.
#![cfg(unix)]

use mj_core::targets::{BoundedProcessExecutor, CommandExecutor, CommandSpec};
use std::time::Duration;

struct PushFixture {
    directory: tempfile::TempDir,
    root: std::path::PathBuf,
    repository: std::path::PathBuf,
    remote: std::path::PathBuf,
    git: String,
}

impl PushFixture {
    fn new() -> Self {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("worker with spaces");
        let repository = directory.path().join("repository");
        let remote = directory.path().join("remote.git");
        let bin = directory.path().join("tools");
        let home = directory.path().join("home");
        for path in [&root, &repository, &bin, &home] {
            std::fs::create_dir_all(path).unwrap();
        }
        let executor = BoundedProcessExecutor::new(Duration::from_secs(15));
        let output = executor
            .execute(&CommandSpec::new("sh", ["-c", "command -v git"]))
            .unwrap();
        assert_eq!(output.status, 0);
        let git = String::from_utf8(output.stdout).unwrap().trim().to_owned();
        let fixture = Self {
            directory,
            root,
            repository,
            remote,
            git,
        };
        std::fs::write(home.join(".gitconfig"), "[user]\nname = Session User\nemail = session@example.invalid\n[commit]\ngpgsign = false\n").unwrap();
        let mut environment = std::collections::BTreeMap::from([
            ("HOME".into(), home.to_str().unwrap().into()),
            ("PATH".into(), format!("{}:/usr/bin:/bin", bin.display())),
            ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
            ("GH_TOKEN".into(), "stale-target-token".into()),
            ("GITHUB_TOKEN".into(), "other-stale-target-token".into()),
            ("GIT_CONFIG_COUNT".into(), "2".into()),
            ("GIT_CONFIG_KEY_0".into(), "user.email".into()),
            ("GIT_CONFIG_VALUE_0".into(), "target@example.invalid".into()),
            (
                "GIT_CONFIG_KEY_1".into(),
                "credential.https://github.com.helper".into(),
            ),
            ("GIT_CONFIG_VALUE_1".into(), "invalid-target-helper".into()),
        ]);
        let config: mj_core::worker_launch::WorkerLaunchConfig =
            serde_json::from_value(serde_json::json!({
                "session_id": "018f9dd2-a3b4-7c8d-9000-123456789abc", "harness": "codex",
                "bridge_command": "/unused-harness", "bridge_args": [], "environment": {},
                "target_environment": environment, "cwd": fixture.repository,
                "execution_policy": "configured_approvals"
            }))
            .unwrap();
        config.write(&fixture.root.join("launch.json")).unwrap();
        mj_worker::worker_runtime::configure_github_cli(&fixture.root, &mut environment).unwrap();
        mj_core::credentials::remove_github_token(&fixture.root.join("github-token")).unwrap();
        let gh = bin.join("gh");
        std::fs::write(
            &gh,
            r#"#!/bin/sh
set -eu
[ "$1" = auth ] && [ "$2" = git-credential ]
cat >/dev/null
if [ "${3-}" = get ] && [ -n "${GH_TOKEN:-}" ]; then
    printf 'username=x-access-token\npassword=%s\n' "$GH_TOKEN"
fi
"#,
        )
        .unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();
        fixture.git(&fixture.repository, &["init", "-q"]);
        fixture.git(
            fixture.remote.parent().unwrap(),
            &["init", "--bare", "-q", fixture.remote.to_str().unwrap()],
        );
        fixture.git(
            &fixture.repository,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--allow-empty",
                "-qm",
                "initial",
            ],
        );
        fixture.git(
            &fixture.repository,
            &["remote", "add", "origin", fixture.remote.to_str().unwrap()],
        );
        let quote = mj_core::targets::posix_quote;
        let hook = fixture.repository.join(".git/hooks/pre-push");
        let script = format!(
            r#"#!/bin/sh
set -eu
test "${{GIT_CONFIG_GLOBAL:-}}" = {config} || {{ echo 'missing session Git config' >&2; exit 1; }}
test -z "${{GH_TOKEN:-}}${{GITHUB_TOKEN:-}}"
test -z "${{GIT_CONFIG_COUNT:-}}${{GIT_CONFIG_KEY_0:-}}${{GIT_CONFIG_VALUE_0:-}}"
test "$({git} config --get user.name)" = 'Session User'
test "$({git} config --get user.email)" = 'target@example.invalid'
credentials=$(printf 'protocol=https\nhost=github.com\n\n' | {git} credential fill)
expected=$(cat {expected})
test "$credentials" = "$(printf 'protocol=https\nhost=github.com\nusername=x-access-token\npassword=%s' "$expected")"
"#,
            config = quote(fixture.root.join("gitconfig").to_str().unwrap()),
            git = quote(&fixture.git),
            expected = quote(
                fixture
                    .directory
                    .path()
                    .join("expected-token")
                    .to_str()
                    .unwrap()
            )
        );
        std::fs::write(&hook, script).unwrap();
        std::fs::set_permissions(hook, std::fs::Permissions::from_mode(0o700)).unwrap();
        fixture
    }

    fn git(&self, cwd: &std::path::Path, args: &[&str]) -> Vec<u8> {
        let mut command = CommandSpec::new(&self.git, args.iter().copied());
        command.cwd = Some(cwd.into());
        command.clear_env = true;
        command.env.extend([
            ("PATH".into(), "/usr/bin:/bin".into()),
            (
                "HOME".into(),
                self.directory.path().to_str().unwrap().into(),
            ),
            ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
            ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
        ]);
        let output = BoundedProcessExecutor::new(Duration::from_secs(15))
            .execute(&command)
            .unwrap();
        assert_eq!(
            output.status,
            0,
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    fn push(&self, branch: &str) -> mj_core::targets::CommandOutput {
        let mut command = CommandSpec::new(
            env!("CARGO_BIN_EXE_mj-worker"),
            [
                "worker",
                "push-branch",
                "--root",
                self.root.to_str().unwrap(),
                "--repository",
                self.repository.to_str().unwrap(),
                "--branch",
                branch,
            ],
        );
        command.env.extend([
            ("HOME".into(), "/wrong-launcher-home".into()),
            ("PATH".into(), "/wrong-launcher-path".into()),
            ("GIT_DIR".into(), "/wrong-launcher-repository".into()),
            ("GIT_WORK_TREE".into(), "/wrong-launcher-worktree".into()),
            ("GIT_CONFIG_GLOBAL".into(), "/wrong-launcher-config".into()),
            ("GH_TOKEN".into(), "stale-launcher-token".into()),
            ("GITHUB_TOKEN".into(), "other-stale-launcher-token".into()),
            ("GIT_CONFIG_COUNT".into(), "1".into()),
            ("GIT_CONFIG_KEY_0".into(), "credential.helper".into()),
            (
                "GIT_CONFIG_VALUE_0".into(),
                "invalid-launcher-helper".into(),
            ),
        ]);
        BoundedProcessExecutor::new(Duration::from_secs(15))
            .execute(&command)
            .unwrap()
    }
}

#[test]
fn branch_export_uses_session_auth_after_clean_reexec() {
    let fixture = PushFixture::new();
    for (token, branch) in [
        ("first-session-token", "review/first"),
        ("rotated-session-token", "review/rotated"),
    ] {
        mj_core::credentials::write_github_token(
            &fixture.root.join("github-token"),
            token.as_bytes(),
        )
        .unwrap();
        std::fs::write(fixture.directory.path().join("expected-token"), token).unwrap();
        let setup = ["gitconfig", "gitconfig-inherited", "bin/gh", "github-token"].map(|name| {
            let path = fixture.root.join(name);
            let bytes = std::fs::read(&path).unwrap();
            let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
            (path, bytes, modified)
        });
        let output = fixture.push(branch);
        assert_eq!(
            output.status,
            0,
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let pushed: mj_core::archive::PushedBranch =
            serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(pushed.branch, branch);
        assert_eq!(
            fixture.git(
                &fixture.remote,
                &["rev-parse", &format!("refs/heads/{branch}")]
            ),
            fixture.git(&fixture.repository, &["rev-parse", "HEAD"])
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains(token));
        assert!(!String::from_utf8_lossy(&output.stderr).contains(token));
        for (path, bytes, modified) in setup {
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
            assert_eq!(
                std::fs::metadata(path).unwrap().modified().unwrap(),
                modified
            );
        }
    }
    mj_core::credentials::remove_github_token(&fixture.root.join("github-token")).unwrap();
    let output = fixture.push("review/removed");
    assert_ne!(output.status, 0);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("could not read Username"), "{stderr}");
    assert!(!stderr.contains("stale-launcher-token"));
}

#[test]
fn branch_export_reports_missing_or_invalid_session_setup() {
    let fixture = PushFixture::new();
    let config = fixture.root.join("gitconfig");
    let saved = fixture.root.join("saved-gitconfig");
    std::fs::rename(&config, &saved).unwrap();
    let output = fixture.push("review/missing");
    assert_ne!(output.status, 0);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("gitconfig"), "{stderr}");
    assert!(stderr.contains("resume the session"), "{stderr}");
    assert!(!config.exists(), "export must not recreate the setup");

    std::os::unix::fs::symlink(&saved, &config).unwrap();
    let output = fixture.push("review/symlink");
    assert_ne!(output.status, 0);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("not a regular file"), "{stderr}");
    assert!(stderr.contains("resume the session"), "{stderr}");

    std::fs::remove_file(&config).unwrap();
    std::fs::rename(&saved, &config).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o000)).unwrap();
    // Privileged runners can still read mode-000 files.
    let unreadable = std::fs::File::open(&config).is_err();
    let output = fixture.push("review/unreadable");
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
    if unreadable {
        assert_ne!(output.status, 0);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("read ") && stderr.contains("gitconfig"),
            "{stderr}"
        );
        assert!(stderr.contains("resume the session"), "{stderr}");
    }
}

#[test]
fn branch_export_preserves_local_and_explicit_ssh_pushes_without_a_token() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = PushFixture::new();
    std::fs::remove_file(fixture.repository.join(".git/hooks/pre-push")).unwrap();
    let output = fixture.push("review/local");
    assert_eq!(
        output.status,
        0,
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Exercise Git's SSH transport with a local receive-pack instead of a server.
    let ssh = fixture.directory.path().join("fixture ssh");
    let quote = mj_core::targets::posix_quote;
    std::fs::write(
        &ssh,
        format!(
            "#!/bin/sh\nexec {} receive-pack {}\n",
            quote(&fixture.git),
            quote(fixture.remote.to_str().unwrap())
        ),
    )
    .unwrap();
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o700)).unwrap();
    let config_path = fixture.root.join("launch.json");
    let mut config = mj_core::worker_launch::WorkerLaunchConfig::read(&config_path).unwrap();
    config
        .target_environment
        .insert("GIT_SSH_COMMAND".into(), quote(ssh.to_str().unwrap()));
    config
        .target_environment
        .insert("GIT_SSH_VARIANT".into(), "ssh".into());
    config.write(&config_path).unwrap();
    fixture.git(
        &fixture.repository,
        &[
            "remote",
            "set-url",
            "origin",
            "ssh://fixture.invalid/session",
        ],
    );
    let output = fixture.push("review/ssh");
    assert_eq!(
        output.status,
        0,
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for branch in ["review/local", "review/ssh"] {
        assert_eq!(
            fixture.git(&fixture.remote, &["rev-parse", branch]),
            fixture.git(&fixture.repository, &["rev-parse", "HEAD"])
        );
    }
    assert!(!fixture.root.join("github-token").exists());
}

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
        mj_worker::relay::DurableRelay::open(
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
        let script =
            mj_controller::targets::worker_daemon_liveness_script(worker_root.to_str().unwrap());
        let output = executor.execute(&CommandSpec::new("sh", ["-c", &script]))?;
        ensure!(
            output.status == 0 && output.stdout == b"alive\n",
            "re-executed worker lost its lifecycle identity"
        );
        Ok(())
    })();
    // Always stop/join before dropping the worker's files, even if observation
    // failed. The bounded owner also cleans up if the identity check regresses.
    let script = mj_controller::targets::stop_worker_daemon_script(worker_root.to_str().unwrap());
    let stopped = executor.execute(&CommandSpec::new("sh", ["-c", &script]));
    let output = worker.join().expect("worker owner panicked");
    observation.unwrap();
    assert_eq!(stopped.unwrap().status, 0);
    output
        .context("lifecycle stop did not terminate the worker before its deadline")
        .unwrap();
}
