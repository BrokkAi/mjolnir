//! `hel worker liveness|stop|holder` against real processes: a real worker, and
//! stand-ins that hold the real root lock the way a worker does.
#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use mj_worker::worker_runtime::WorkerRootOwner;

const HEL: &str = env!("CARGO_BIN_EXE_mj-worker");

fn hel(args: &[&str], root: &Path, extra: &[&str]) -> (Output, Duration) {
    let started = Instant::now();
    let output = Command::new(HEL)
        .arg("worker")
        .args(args)
        .arg("--root")
        .arg(root)
        .args(extra)
        .output()
        .expect("run hel");
    (output, started.elapsed())
}

fn token(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn gone(pid: u32) -> bool {
    let state = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let state = String::from_utf8_lossy(&state.stdout);
    state.trim().is_empty() || state.trim().starts_with('Z')
}

fn wait_for(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Kills what a failed assertion would otherwise leave running.
struct Cleanup {
    groups: Vec<u32>,
    pid_files: Vec<PathBuf>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        for file in &self.pid_files {
            if let Ok(pid) = std::fs::read_to_string(file)
                && let Ok(pid) = pid.trim().parse::<i32>()
            {
                // SAFETY: plain signal to a pid this test started.
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
        }
        for group in &self.groups {
            // SAFETY: plain signal to a group this test created.
            unsafe { libc::kill(-(*group as i32), libc::SIGKILL) };
        }
    }
}

/// A process that takes the root's real lock as a worker does, keeps it for as
/// long as it lives, and starts a descendant that leaves its group and session
/// and ignores TERM, as a harness helper can.
fn lock_holder_with_detached_descendant(root: &Path) -> Child {
    let script = r#"
import fcntl, os, subprocess, sys, time
root = sys.argv[1]
lock = open(os.path.join(root, "worker.lock"), "w")
fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
subprocess.Popen(
    ["sh", "-c", "trap '' TERM; echo $$ > descendant; while :; do sleep 0.05; done"],
    cwd=root, start_new_session=True)
while not os.path.exists(os.path.join(root, "descendant")):
    time.sleep(0.01)
open(os.path.join(root, "ready"), "w").write("1")
while True:
    time.sleep(1)
"#;
    Command::new("python3")
        .args(["-c", script])
        .arg(root)
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start lock holder")
}

fn record_worker(root: &Path, pid: u32, birth: &str) {
    std::fs::write(
        root.join(mj_core::relay::WORKER_STARTUP_FILE),
        serde_json::to_vec(&serde_json::json!({"pid": pid, "process_birth": birth})).unwrap(),
    )
    .unwrap();
}

fn start_real_worker(root: &Path, scratch: &Path) -> Child {
    drop(
        mj_worker::relay::DurableRelay::open(root, "018f9dd2-a3b4-7c8d-9000-123456789abc", "test")
            .unwrap(),
    );
    let config = scratch.join("launch.json");
    std::fs::write(
        &config,
        serde_json::to_vec(&serde_json::json!({
            "run_mode": "checkpoint_only",
            "session_id": "018f9dd2-a3b4-7c8d-9000-123456789abc", "harness": "codex",
            "bridge_command": "/missing-harness", "bridge_args": [],
            "environment": {"CODEX_HOME": scratch.join("profile")},
            "target_environment": {"MJ_INSTANCE": "qa-worker-stop"},
            "cwd": scratch, "execution_policy": "configured_approvals"
        }))
        .unwrap(),
    )
    .unwrap();
    Command::new(HEL)
        .args(["worker", "run", "--root"])
        .arg(root)
        .arg("--config")
        .arg(&config)
        .env("MJ_INSTANCE", "qa-worker-stop")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start worker")
}

/// Stopping a real worker frees its lock, leaves no process, and a second stop
/// of the now-dead root is an immediate no-op.
#[test]
fn stopping_a_live_worker_frees_its_lock_and_leaves_no_process() {
    let scratch = tempfile::tempdir().unwrap();
    let root = scratch.path().join("worker");
    let mut worker = start_real_worker(&root, scratch.path());
    let _cleanup = Cleanup {
        groups: vec![worker.id()],
        pid_files: vec![],
    };
    wait_for(&root.join("control.sock"));
    assert_eq!(token(&hel(&["liveness"], &root, &[]).0), "alive");
    assert_eq!(
        token(&hel(&["holder"], &root, &[]).0),
        worker.id().to_string()
    );

    let (stop, took) = hel(&["stop"], &root, &[]);
    eprintln!("live worker stop took {took:?}");
    assert!(stop.status.success(), "{stop:?}");
    // The worker is a child of this test, so a reaped exit proves it is gone.
    worker.wait().expect("the stopped worker exits");
    drop(WorkerRootOwner::acquire(&root).expect("the lock is free after the stop"));
    assert_eq!(token(&hel(&["liveness"], &root, &[]).0), "dead");

    let (again, took) = hel(&["stop"], &root, &[]);
    eprintln!("dead worker stop took {took:?}");
    assert!(again.status.success(), "{again:?}");
    assert!(
        took < Duration::from_secs(1),
        "stopping a dead root took {took:?}"
    );
}

/// The harness and its helpers lead groups and sessions of their own, so the
/// worker's group can empty while they still write into the root. The stop
/// must take them down too, with or without the TERM grace, before it returns.
// Hard-won: 2f859c8f: a harness outlived its worker group and raced removal of staged state.
#[test]
fn stopping_a_worker_kills_a_descendant_that_left_its_process_group() {
    for kill in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        let mut holder = lock_holder_with_detached_descendant(root);
        let _cleanup = Cleanup {
            groups: vec![holder.id()],
            pid_files: vec![root.join("descendant")],
        };
        wait_for(&root.join("ready"));
        let descendant: u32 = std::fs::read_to_string(root.join("descendant"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        record_worker(
            root,
            holder.id(),
            &mj_core::subprocess::process_birth_identity(holder.id()).unwrap(),
        );
        assert_eq!(token(&hel(&["liveness"], root, &[]).0), "starting");

        let (stop, took) = hel(&["stop"], root, if kill { &["--kill"] } else { &[] });
        eprintln!("detached-descendant stop (kill={kill}) took {took:?}");
        assert!(stop.status.success(), "{stop:?}");
        holder.wait().unwrap();
        assert!(gone(descendant), "the descendant survived (kill={kill})");
        drop(WorkerRootOwner::acquire(root).expect("the lock is free after the stop"));
    }
}

/// A pid in a startup record is not an identity. Only the recorded birth makes
/// the record the holder's, so an unrelated live process that happens to carry
/// the recorded pid is never signalled, and the stop reports the failure.
#[test]
fn a_recycled_pid_in_the_startup_record_is_never_signalled() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let mut holder = lock_holder_with_detached_descendant(root);
    let mut unrelated = Command::new("sleep")
        .arg("60")
        .process_group(0)
        .spawn()
        .unwrap();
    let _cleanup = Cleanup {
        groups: vec![holder.id(), unrelated.id()],
        pid_files: vec![root.join("descendant")],
    };
    wait_for(&root.join("ready"));
    record_worker(root, unrelated.id(), "a-previous-process-birth");

    let (stop, _) = hel(&["stop"], root, &[]);

    assert!(!stop.status.success(), "{stop:?}");
    assert!(
        String::from_utf8_lossy(&stop.stderr).contains("no startup record identifies it"),
        "{stop:?}"
    );
    assert!(
        unrelated.try_wait().unwrap().is_none(),
        "the unrelated process was signalled"
    );
    assert!(
        holder.try_wait().unwrap().is_none(),
        "the lock holder was signalled"
    );
    unrelated.kill().unwrap();
    unrelated.wait().unwrap();
}

/// A root with no worker answers `dead` and stops at once, whether the lock
/// file never existed or a previous worker left it behind.
#[test]
fn a_root_without_a_worker_is_dead_and_stops_at_once() {
    let root = tempfile::tempdir().unwrap();
    for lock_file_exists in [false, true] {
        if lock_file_exists {
            std::fs::write(root.path().join("worker.lock"), "").unwrap();
        }
        let (liveness, _) = hel(&["liveness"], root.path(), &[]);
        assert_eq!(token(&liveness), "dead");
        assert_eq!(token(&hel(&["holder"], root.path(), &[]).0), "");
        let (stop, took) = hel(&["stop"], root.path(), &[]);
        assert!(stop.status.success(), "{stop:?}");
        assert!(took < Duration::from_secs(1), "took {took:?}");
    }
    let missing = root.path().join("never-created");
    assert_eq!(token(&hel(&["liveness"], &missing, &[]).0), "dead");
}
