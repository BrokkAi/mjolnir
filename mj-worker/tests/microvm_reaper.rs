#![cfg(target_os = "linux")]

//! The guest agent's PID 1 reaper owns every `waitpid`. It runs alone in this
//! test binary because it blocks SIGCHLD process-wide and collects the exit
//! status of every child of the process.

use std::process::Command;
use std::time::Duration;

use mj_worker::microvm::guest_agent::{ProcessSpawner, Reaper};

// Hard-won: a spawn whose exec fails (a missing working directory) panicked in
// std when the reaper had already collected the half-started child, and the
// panic poisoned the reaper's lock for every later exec in the VM.
#[test]
fn reaper_survives_failed_spawns_and_reports_exits_and_orphans() {
    // Orphans reparent to this process, as they do to PID 1.
    // SAFETY: prctl with PR_SET_CHILD_SUBREAPER takes one integer argument.
    assert_eq!(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) }, 0);
    let reaper = Reaper::start().unwrap();

    for _ in 0..50 {
        let mut missing_directory = Command::new("true");
        missing_directory.current_dir("/nonexistent-directory");
        let error = reaper
            .spawn(&mut missing_directory)
            .err()
            .expect("spawn must fail");
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }

    let mut exits = Command::new("sh");
    exits.args(["-c", "exit 3"]);
    let spawned = reaper.spawn(&mut exits).unwrap();
    let status = spawned
        .exit
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .unwrap();
    assert_eq!(status.code(), Some(3));

    // A grandchild outlives its parent and is reaped without anyone waiting for it.
    let mut orphans = Command::new("sh");
    orphans.args(["-c", "sleep 0.2 & echo $!"]);
    orphans.stdout(std::process::Stdio::piped());
    let mut spawned = reaper.spawn(&mut orphans).unwrap();
    let mut pid_text = String::new();
    std::io::Read::read_to_string(&mut spawned.stdout.take().unwrap(), &mut pid_text).unwrap();
    let orphan: u32 = pid_text.trim().parse().unwrap();
    for _ in 0..100 {
        if !std::path::Path::new(&format!("/proc/{orphan}")).exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("orphan {orphan} was never reaped");
}
