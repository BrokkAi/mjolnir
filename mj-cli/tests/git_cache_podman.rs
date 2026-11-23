#![cfg(target_os = "linux")]

mj_core::enable_test_fixtures_on_startup!();

use std::fs;
use std::net::TcpListener;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use mj_core::subprocess::{ProcessGroupGuard, run_with_input};
use serde_json::Value;

fn output(command: &mut Command) -> Result<String> {
    let result = run_with_input(command, &[])?;
    ensure!(
        result.status.success(),
        "{:?} {:?} failed: {}{}",
        command.get_program(),
        command.get_args(),
        String::from_utf8_lossy(&result.stderr),
        String::from_utf8_lossy(&result.stdout)
    );
    Ok(String::from_utf8(result.stdout)?.trim().to_owned())
}

fn fixture_git(directory: &Path, args: &[&str]) -> Result<String> {
    output(
        Command::new("git")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args([
                "-c",
                "user.name=Cache Test",
                "-c",
                "user.email=cache@example.invalid",
            ])
            .args(args)
            .current_dir(directory),
    )
}

// Hard-won: 8d8e2db1: a Podman cutoff checkout fetched a host-only source from inside the container.
// Hard-won: a90bd7e75: an ordinary host-only checkout borrowed objects from the snapshot mount.
// Hard-won: a90bd7e75: an exact host-only checkout needed to fetch an unadvertised commit inside the container.
#[test]
#[ignore = "requires Podman, a locally available buildpack-deps:noble image, and a matching portable MJ_WORKER_BINARY"]
fn podman_bundle_uses_a_host_only_git_source_without_later_history() -> Result<()> {
    let root = tempfile::tempdir()?;
    let repository = root.path().join("project");
    let profile = root.path().join("profile");
    fs::create_dir(&repository)?;
    fs::create_dir(&profile)?;
    // Start the real adapter without using a real account or API.
    fs::write(
        profile.join("auth.json"),
        r#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk-test"}"#,
    )?;
    let git = |args: &[&str]| fixture_git(&repository, args);
    git(&["init", "--quiet", "--initial-branch=main"])?;
    let commit = |message: &str| {
        git(&["commit", "--quiet", "--allow-empty", "-m", message])?;
        git(&["rev-parse", "HEAD"])
    };
    commit("common ancestor")?;
    git(&["switch", "--quiet", "-c", "base"])?;
    let base = commit("base on another branch")?;
    git(&["switch", "--quiet", "main"])?;
    let at = commit("start here")?;
    let later = commit("later")?;
    git(&["tag", "later"])?;

    let port = TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
    let source = format!("git://127.0.0.1:{port}/project");
    git(&["remote", "add", "origin", &source])?;
    let mut listener = Command::new("git")
        .args(["daemon", "--listen=127.0.0.1", "--export-all"])
        .arg(format!("--port={port}"))
        .arg(format!("--base-path={}", root.path().display()))
        .arg(&repository)
        .process_group(0)
        .stdin(Stdio::null())
        .spawn()?;
    let listener_group = ProcessGroupGuard::new(Some(listener.id()));

    let result = (|| -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            ensure!(
                listener.try_wait()?.is_none(),
                "the fixture Git listener exited"
            );
            match git(&["ls-remote", &source, "HEAD"]) {
                Ok(_) => break,
                Err(error) if Instant::now() >= deadline => return Err(error),
                Err(_) => thread::sleep(Duration::from_millis(25)),
            }
        }
        run_checkout_test(
            root.path(),
            &profile,
            &repository,
            &source,
            &at,
            &base,
            &later,
        )
    })();
    drop(listener_group);
    listener.wait()?;
    result
}

fn run_checkout_test(
    root: &Path,
    profile: &Path,
    repository: &Path,
    source: &str,
    at: &str,
    base: &str,
    later: &str,
) -> Result<()> {
    let instance = format!(
        "gitcache-local-test-{}",
        &mj_core::state::new_session_id()?[..8]
    );
    let config = root.join("config");
    fs::create_dir(&config)?;
    fs::write(
        config.join("config.toml"),
        format!(
            r#"version = {}
[phone]
enabled = true
bind = "127.0.0.1:0"
tailscale_detect = false
[profiles.fixture]
kind = "codex"
home = {}
environment = {{ OPENAI_BASE_URL = "http://127.0.0.1:9/v1" }}
[bundles.fixture]
primary_repo = "project"
[[bundles.fixture.repositories]]
id = "project"
local = {}
destination = "project"
[targets.podman]
kind = "podman"
image = "docker.io/library/buildpack-deps:noble"
pull_policy = "never"
[machines.local]
kind = "local"
build_cache = {{ enabled = false }}
[github.watch]
enabled = false
"#,
            mj_core::config::CONFIG_VERSION,
            serde_json::to_string(profile)?,
            serde_json::to_string(repository)?,
        ),
    )?;
    let worker = fs::canonicalize(
        std::env::var_os("MJ_WORKER_BINARY")
            .context("set MJ_WORKER_BINARY to the portable worker")?,
    )?;
    let host_home = PathBuf::from(std::env::var_os("HOME").context("HOME is unset")?);
    // Keep Podman's installed image store while isolating the Git cache,
    // Mjolnir instance, and agent profiles in the fixture directory.
    let engine_data = std::env::var_os("XDG_DATA_HOME")
        .unwrap_or_else(|| host_home.join(".local/share").into_os_string());
    let engine_storage =
        output(Command::new("podman").args(["info", "--format", "{{.Store.ConfigFile}}"]))?;
    let cli = |args: &[&str]| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_mj"));
        let passthrough = ["USER", "PATH", "XDG_RUNTIME_DIR"];
        command
            .env_clear()
            .envs(
                passthrough
                    .into_iter()
                    .filter_map(|name| std::env::var_os(name).map(|value| (name, value))),
            )
            .env("HOME", root)
            .env("XDG_CONFIG_HOME", root.join("xdg-config"))
            .env("XDG_DATA_HOME", &engine_data)
            .env("MJ_CONFIG_DIR", &config)
            .env("MJ_DATA_DIR", root.join("data"))
            .env("MJ_DAEMON_OWNER_PID", std::process::id().to_string())
            .env("MJ_WORKER_BINARY", &worker)
            .args(["--instance", &instance])
            .args(args)
            .current_dir(root);
        if !engine_storage.is_empty() {
            command.env("CONTAINERS_STORAGE_CONF", &engine_storage);
        }
        output(&mut command)
    };

    let result = (|| -> Result<()> {
        cli(&["workspaces", "create", "Cache regression"])?;
        let start = |selection: &[&str]| -> Result<String> {
            let mut args = vec![
                "--workspace",
                "Cache regression",
                "new",
                "--profile",
                "fixture",
                "--target",
                "podman",
                "--bundle",
                "fixture",
                "--no-github-credentials",
                "--json",
            ];
            args.extend_from_slice(selection);
            let response: Value = serde_json::from_str(&cli(&args)?)?;
            let session = response["session_id"]
                .as_str()
                .context("missing session ID")?;
            cli(&["wait", "--session", session, "--timeout", "60"])?;
            Ok(session.to_owned())
        };
        let session = start(&["--at", at, "--base", base, "--no-later-history"])?;
        let container = mj_core::targets::resource_name(&session)?;
        let checkout = mj_core::targets::new_container_workspace(&session)?.join("project");
        let mounts: Vec<Value> = serde_json::from_str(&output(Command::new("podman").args([
            "inspect",
            "--format",
            "{{json .Mounts}}",
            &container,
        ]))?)?;
        ensure!(
            mounts.iter().any(|mount| {
                mount["Destination"] == "/run/hel/git-cache" && mount["RW"] == false
            }),
            "the session should mount its Git bundle read-only"
        );
        output(
            Command::new("podman")
                .args(["exec", "--workdir"])
                .arg(checkout)
                .args([
                    &container,
                    "sh",
                    "-ceu",
                    r#"
test "$(git rev-parse HEAD)" = "$1"
test "$(git rev-parse refs/mj/base)" = "$2"
test "$(git for-each-ref --format='%(refname)')" = refs/mj/base
test -z "$(git remote)"
test ! -e .git/objects/info/alternates
if git cat-file -e "$3^{commit}" 2>/dev/null; then exit 1; fi
set -- /run/hel/git-cache/*
test "$#" = 1
case "$1" in *.bundle) test -f "$1";; *) exit 1;; esac
"#,
                    "verify-cutoff",
                    at,
                    base,
                    later,
                ]),
        )?;
        let probe = run_with_input(
            Command::new("podman").args(["exec", &container, "git", "ls-remote", "--", source]),
            &[],
        )?;
        ensure!(
            probe.status.code() == Some(128),
            "the host's Git listener must be unreachable inside the container"
        );
        cli(&["destroy", "--session", &session])?;

        // Warm the cache with an advertised default branch, then launch at a
        // commit the server serves by SHA but no advertised ref reaches.
        fixture_git(repository, &["switch", "--quiet", "--detach", at])?;
        fixture_git(
            repository,
            &[
                "commit",
                "--quiet",
                "--allow-empty",
                "-m",
                "SHA-only commit",
            ],
        )?;
        let hidden = fixture_git(repository, &["rev-parse", "HEAD"])?;
        fixture_git(repository, &["switch", "--quiet", "base"])?;
        fixture_git(
            repository,
            &["config", "uploadpack.allowAnySHA1InWant", "true"],
        )?;
        let advertised = fixture_git(repository, &["ls-remote", source])?;
        ensure!(
            !advertised.contains(&hidden),
            "the exact commit must be absent from advertised refs"
        );
        let session = start(&["--at", &hidden])?;
        let container = mj_core::targets::resource_name(&session)?;
        let checkout = mj_core::targets::new_container_workspace(&session)?.join("project");
        output(
            Command::new("podman")
                .args(["exec", "--workdir"])
                .arg(checkout)
                .args([
                    &container,
                    "sh",
                    "-ceu",
                    r#"
test "$(git rev-parse HEAD)" = "$1"
test -z "$(git branch --show-current)"
test "$(git symbolic-ref refs/remotes/origin/HEAD)" = refs/remotes/origin/base
test "$(git remote get-url origin)" = "$2"
test "$(git config --get-all remote.origin.fetch)" = '+refs/heads/*:refs/remotes/origin/*'
test ! -e .git/objects/info/alternates
"#,
                    "verify-clone",
                    &hidden,
                    source,
                ]),
        )?;
        cli(&["destroy", "--session", &session])?;
        Ok(())
    })();
    let stopped = cli(&["daemon", "stop"]);
    let cleanup = (|| -> Result<()> {
        let owned = output(Command::new("podman").args([
            "ps",
            "--all",
            "--quiet",
            "--filter",
            &format!("label=dev.mj.instance={instance}"),
        ]))?;
        if !owned.is_empty() {
            output(
                Command::new("podman")
                    .args(["rm", "--force"])
                    .args(owned.lines()),
            )?;
        }
        Ok(())
    })();
    result.and(stopped.map(|_| ())).and(cleanup)
}
