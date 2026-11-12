//! Managed groups whose direct leader is never reaped before the final signal.
//! A completed stop covers this group, not descendants that changed groups.
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus};

use anyhow::{Context, Result, bail};

/// The streams may be handed to another process without transferring reaping authority.
pub struct GroupStdio {
    pub stdin: Option<ChildStdin>,
    pub stdout: Option<ChildStdout>,
    pub stderr: Option<ChildStderr>,
}

#[derive(Clone, Copy, Debug)]
pub struct GroupStopped {
    leader_status: ExitStatus,
}
impl GroupStopped {
    pub fn leader_status(&self) -> ExitStatus {
        self.leader_status
    }
}

enum State {
    Reserved(Child),
    Reaping(tokio::task::JoinHandle<std::io::Result<ExitStatus>>),
    Reaped(ExitStatus),
    Failed,
    Stopped(GroupStopped),
}

/// Spawn and use from supervised owner tasks. No caller can wait on the Child.
pub struct OwnedGroup {
    pid: u32,
    state: State,
    observed: Option<ExitStatus>,
}
impl OwnedGroup {
    /// Spawning can perform blocking I/O; call from the owner's blocking launch task.
    pub fn spawn(command: &mut Command) -> Result<Self> {
        let child = command
            .process_group(0)
            .spawn()
            .context("spawn owned group")?;
        Ok(Self {
            pid: child.id(),
            state: State::Reserved(child),
            observed: None,
        })
    }

    pub fn leader_id(&self) -> u32 {
        self.pid
    }

    pub fn take_stdio(&mut self) -> GroupStdio {
        match &mut self.state {
            State::Reserved(child) => GroupStdio {
                stdin: child.stdin.take(),
                stdout: child.stdout.take(),
                stderr: child.stderr.take(),
            },
            _ => GroupStdio {
                stdin: None,
                stdout: None,
                stderr: None,
            },
        }
    }

    fn observe(&mut self) -> Result<Option<ExitStatus>> {
        if !matches!(self.state, State::Reserved(_)) {
            bail!("owned group {} no longer reserves its leader", self.pid);
        }
        // SAFETY: zeroed siginfo is the specified WNOHANG no-event sentinel.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        loop {
            // SAFETY: pid identifies our retained, unreaped direct Child. WNOWAIT
            // observes status without releasing the PID or process-group identity.
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    self.pid as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
                )
            };
            if result == 0 {
                break;
            }
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            if error.raw_os_error() == Some(libc::ECHILD) {
                // External reaping broke ownership. Never signal this ID again.
                self.state = State::Failed;
            }
            return Err(error).context("observe owned leader without reaping");
        }
        // SAFETY: successful waitid with nonzero si_pid reports child-exit fields.
        if unsafe { info.si_pid() } == 0 {
            return Ok(None);
        }
        let status = unsafe { info.si_status() };
        let raw = match info.si_code {
            libc::CLD_EXITED => status << 8,
            libc::CLD_KILLED => status,
            libc::CLD_DUMPED => status | 0x80,
            code => bail!("unexpected owned leader exit code {code}"),
        };
        let status = ExitStatus::from_raw(raw);
        self.observed = Some(status);
        Ok(Some(status))
    }

    /// Observe exit, retaining even a zombie leader until stop releases it.
    pub async fn leader_exit(&mut self) -> Result<ExitStatus> {
        if let Some(status) = self.observed {
            return Ok(status);
        }
        // Register BEFORE checking: an exit between checking and subscribing
        // must not leave us waiting forever for a notification already delivered.
        let mut signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child())
            .context("subscribe to owned leader exit")?;
        loop {
            if let Some(status) = self.observe()? {
                return Ok(status);
            }
            signal
                .recv()
                .await
                .context("owned leader SIGCHLD stream closed")?;
        }
    }

    /// Request only TERM; the caller owns its existing grace duration.
    pub fn request_terminate(&mut self) -> Result<()> {
        if matches!(self.state, State::Stopped(_)) {
            return Ok(());
        }
        self.observe()?;
        signal_reserved(self.pid, libc::SIGTERM)
    }

    /// Final KILL while reserved, permanently disarm, reap once, then confirm
    /// absence. Cancellation retains the reap task and permits absence-only retry.
    pub async fn stop(&mut self) -> Result<GroupStopped> {
        if let State::Stopped(done) = self.state {
            return Ok(done);
        }
        if matches!(self.state, State::Reserved(_)) {
            self.observe()?;
            signal_reserved(self.pid, libc::SIGKILL)?;
            // The Child remains reserved through the last signal. No transition
            // after this one has any authority to send another signal.
            let State::Reserved(mut child) = std::mem::replace(&mut self.state, State::Failed)
            else {
                unreachable!()
            };
            self.state = State::Reaping(tokio::task::spawn_blocking(move || {
                let result = child.wait();
                if let Err(error) = &result {
                    tracing::error!(%error, "could not reap owned leader");
                }
                result
            }));
        }
        if let State::Reaping(task) = &mut self.state {
            match task.await {
                Ok(Ok(status)) => {
                    self.observed = Some(status);
                    self.state = State::Reaped(status);
                }
                Ok(Err(error)) => {
                    self.state = State::Failed;
                    return Err(error).context("reap owned group leader");
                }
                Err(error) => {
                    self.state = State::Failed;
                    return Err(error).context("owned group reap task failed");
                }
            }
        }
        let State::Reaped(status) = self.state else {
            bail!("owned group {} has unresolved leader ownership", self.pid);
        };
        // Existing teardown's post-KILL bound: three one-second confirmations.
        // Zombies may keep the numeric group present: fail conservatively rather
        // than signalling after reap, even if that ID belongs to a new group.
        for attempt in 0..=3 {
            if group_absent(self.pid)? {
                let done = GroupStopped {
                    leader_status: status,
                };
                self.state = State::Stopped(done);
                return Ok(done);
            }
            if attempt < 3 {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }
        bail!(
            "owned group {} is still present after KILL and reap",
            self.pid
        )
    }
}

fn signal_reserved(pid: u32, signal: i32) -> Result<()> {
    // SAFETY: callers hold the unreaped direct leader reserving this PGID.
    if unsafe { libc::kill(-(pid as libc::pid_t), signal) } == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(error).context("signal reserved owned group")
}
fn group_absent(pid: u32) -> Result<bool> {
    // SAFETY: signal zero only queries; it cannot harm a recycled group ID.
    if unsafe { libc::kill(-(pid as libc::pid_t), 0) } == 0 {
        return Ok(false);
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(true);
    }
    Err(error).context("confirm reaped owned group absence")
}

impl Drop for OwnedGroup {
    fn drop(&mut self) {
        if matches!(self.state, State::Stopped(_)) {
            return;
        }
        tracing::error!(pid = self.pid, "owned group dropped without completed stop");
        if !matches!(self.state, State::Reserved(_)) {
            return;
        }
        // The shared observation checks ECHILD before fallback signalling too.
        let reserved = match self.observe() {
            Ok(_) => true,
            Err(error) => {
                tracing::error!(%error, "fallback owned group identity unavailable; not signalling");
                false
            }
        };
        let State::Reserved(mut child) = std::mem::replace(&mut self.state, State::Failed) else {
            return;
        };
        if reserved && let Err(error) = signal_reserved(self.pid, libc::SIGKILL) {
            tracing::error!(%error, "fallback owned group KILL failed");
        }
        if let Err(error) = std::thread::Builder::new()
            .name(format!("mj-group-reap-{}", self.pid))
            .spawn(move || {
                if let Err(error) = child.wait() {
                    tracing::error!(%error, "fallback owned group reap failed");
                }
            })
        {
            tracing::error!(%error, "could not start fallback owned group reaper");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;

    fn quiet(command: &mut Command) -> &mut Command {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
    }

    #[tokio::test]
    async fn exited_leader_reserves_group_until_final_kill_and_one_reap() {
        let directory = tempfile::tempdir().unwrap();
        let release = directory.path().join("exit");
        let mut group = OwnedGroup::spawn(quiet(Command::new("sh").args([
            "-c",
            "while [ ! -e \"$1\" ]; do sleep .01; done; exit 7",
            "owned-leader",
            release.to_str().unwrap(),
        ])))
        .unwrap();
        // A separately parented member lets this test reap it independently:
        // it exercises a surviving group without relying on host PID1 reaping.
        let mut member = quiet(Command::new("sleep").arg("60"))
            .process_group(group.leader_id() as i32)
            .spawn()
            .unwrap();
        let member_reap = tokio::task::spawn_blocking(move || member.wait().unwrap());
        let mut sentinel = OwnedGroup::spawn(quiet(Command::new("sleep").arg("60"))).unwrap();
        std::fs::write(release, b"exit").unwrap();
        let status = group.leader_exit().await.unwrap();
        assert_eq!(status.code(), Some(7));
        assert_eq!(group.leader_exit().await.unwrap(), status);
        // WNOWAIT has not transferred reaping authority or released the PID.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe {
                libc::waitid(
                    libc::P_PID,
                    group.pid,
                    &mut info,
                    libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
                )
            },
            0
        );
        assert_eq!(unsafe { info.si_pid() }, group.pid as i32);
        let done = group.stop().await.unwrap();
        assert_eq!(done.leader_status(), status);
        assert_eq!(member_reap.await.unwrap().signal(), Some(libc::SIGKILL));
        assert_eq!(group.stop().await.unwrap().leader_status(), status);
        // Finished state must never signal, even if the old ID were recycled.
        group.pid = sentinel.pid;
        group.request_terminate().unwrap();
        group.stop().await.unwrap();
        drop(group);
        assert!(sentinel.observe().unwrap().is_none());
        sentinel.stop().await.unwrap();
    }

    #[tokio::test]
    async fn cancelled_observation_and_post_reap_retry_never_signal_recycled_group() {
        let mut group = OwnedGroup::spawn(quiet(Command::new("sleep").arg("60"))).unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), group.leader_exit())
                .await
                .is_err()
        );
        let original_pid = group.pid;
        group.stop().await.unwrap();
        let status = group.observed.unwrap();
        let mut sentinel = OwnedGroup::spawn(quiet(Command::new("sleep").arg("60"))).unwrap();
        // Exercise the actual absence-only retry state against a live different
        // group: it must conservatively refuse, never kill the reused ID.
        group.state = State::Reaped(status);
        group.pid = sentinel.pid;
        assert!(group.stop().await.is_err());
        assert!(group.request_terminate().is_err());
        assert!(sentinel.observe().unwrap().is_none());
        group.pid = original_pid;
        group.stop().await.unwrap();
        sentinel.stop().await.unwrap();
    }

    #[tokio::test]
    async fn externally_reaped_leader_is_an_ownership_failure() {
        let mut group =
            OwnedGroup::spawn(quiet(Command::new("sh").args(["-c", "exit 0"]))).unwrap();
        group.leader_exit().await.unwrap();
        let mut status = 0;
        assert_eq!(
            unsafe { libc::waitpid(group.pid as i32, &mut status, 0) },
            group.pid as i32
        );
        assert!(group.stop().await.is_err());
        assert!(group.request_terminate().is_err());
        assert!(matches!(group.state, State::Failed));
    }
}
