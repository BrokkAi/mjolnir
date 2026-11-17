//! Worker liveness and stop, decided by the lock on `<root>/worker.lock`.
//!
//! The worker holds that lock from before it writes anything until the kernel
//! closes its descriptors, so "the lock cannot be taken" is the whole answer to
//! "is a worker running for this root". It needs no process listing, and a
//! recycled pid cannot confuse it. The controller asks
//! `hel worker liveness|stop|holder` on the target, from the root's own `hel`
//! when that build has them and otherwise from a control worker it stages in
//! the root. It does not test the lock from a shell. Two measured reasons rule
//! that out (#1550): minimal images such as ubi9-minimal have neither
//! `flock(1)` nor Perl, and `/proc/locks` drops a lock's line once the process
//! that took it has exited, even while a forked child still holds the
//! descriptor (#1257), so it would report a live worker as free in any pid
//! namespace other than the initial one.
//!
//! # The control contract
//!
//! Any build may be asked to stop or probe a worker that a different build
//! started, so these commands are a frozen interface, not an implementation
//! detail. The controller decides on the target, per command, which binary in
//! the root serves it: one that exits 0 for `worker <subcommand> --help`
//! (`targets::worker_daemon`). That detection is the only version logic.
//!
//! - The names, arguments, stdout tokens and exit-status meanings of
//!   `worker liveness`, `worker holder` and `worker stop [--kill]` (all with
//!   `--root <dir>`) are frozen. `liveness` prints `dead`, `starting` or
//!   `alive`; `holder` prints the holder's pid, or nothing when no worker holds
//!   the lock; `stop` prints nothing; status 0 is success and any other status
//!   is a failure.
//! - Never change an existing control subcommand's arguments. New behavior is a
//!   new subcommand, which the same `--help` probe detects.
//! - `--help` exits 0 on each of these subcommands. That is how a build proves
//!   it has them, and a build that predates them exits 2.
//! - What the commands read in the root is frozen and may change only by
//!   addition: the exclusive flock on `worker.lock`, held for the worker's
//!   life, and the startup-record fields stop uses (`pid` and `process_birth`
//!   in the startup record). This replaces any rule that an older build must
//!   not judge a newer worker: every build that has the commands reads the
//!   same things the same way, so any of them may stop any worker.
//!
//! The golden for the control scripts and the contract test in this
//! package's `tests/` enforce this.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use crate::worker_runtime::{WORKER_STARTUP_FILE, WorkerRootOwner};

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod process_table;

/// How often a stop looks at the lock and the processes it is waiting for.
const POLL_INTERVAL: Duration = Duration::from_millis(25);
/// The graceful stop's TERM grace, which the former stop script gave the worker.
pub const TERM_GRACE: Duration = Duration::from_secs(2);
/// How long a stop waits for the tree to disappear after KILL. The former stop
/// script waited three one-second rounds.
#[cfg(any(target_os = "linux", target_os = "macos"))]
const KILL_WAIT: Duration = Duration::from_secs(3);
/// How long the holder of the lock may take to write the startup record that
/// names it. The worker writes it as its first act after taking the lock.
const RECORD_WAIT: Duration = Duration::from_secs(3);

/// What `hel worker liveness` reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Liveness {
    /// No process holds the root's lock.
    Dead,
    /// A worker holds the lock but has not bound its control socket.
    Starting,
    /// A worker holds the lock and serves its control socket.
    Alive,
}

impl Liveness {
    pub fn token(self) -> &'static str {
        match self {
            Self::Dead => "dead",
            Self::Starting => "starting",
            Self::Alive => "alive",
        }
    }
}

pub fn liveness(root: &Path) -> Result<Liveness> {
    if !WorkerRootOwner::is_held(root)? {
        return Ok(Liveness::Dead);
    }
    Ok(if control_socket_exists(root) {
        Liveness::Alive
    } else {
        Liveness::Starting
    })
}

fn control_socket_exists(root: &Path) -> bool {
    let socket = root.join("control.sock");
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        std::fs::metadata(&socket).is_ok_and(|metadata| metadata.file_type().is_socket())
    }
    #[cfg(not(unix))]
    {
        socket.exists()
    }
}

/// The pid of the worker holding the root's lock, or `None` when no worker
/// does. A record that does not name a live process of the recorded birth is
/// not an identity, so the wait goes on until the holder writes its own or
/// releases the lock.
pub fn holder(root: &Path) -> Result<Option<u32>> {
    holder_within(root, RECORD_WAIT)
}

fn holder_within(root: &Path, wait: Duration) -> Result<Option<u32>> {
    let deadline = Instant::now() + wait;
    loop {
        if !WorkerRootOwner::is_held(root)? {
            return Ok(None);
        }
        if let Some(pid) = recorded_worker(root) {
            return Ok(Some(pid));
        }
        if Instant::now() >= deadline {
            bail!(
                "a process holds the worker lock in {} but no startup record identifies it",
                root.display()
            );
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// The pid in the worker's own startup record, when the process with that pid
/// is the one that wrote it. A pid alone is not an identity.
fn recorded_worker(root: &Path) -> Option<u32> {
    let record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join(WORKER_STARTUP_FILE)).ok()?).ok()?;
    let pid = u32::try_from(record.get("pid")?.as_u64()?).ok()?;
    let birth = record.get("process_birth")?.as_str()?;
    (mj_core::subprocess::process_birth_identity(pid).ok()? == birth).then_some(pid)
}

/// Stop the worker for `root` and everything it started, then return.
/// `term_grace` is how long a TERM may take before KILL; zero kills at once.
/// A root with no worker returns at once.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn stop(root: &Path, term_grace: Duration) -> Result<()> {
    stop_within(root, term_grace, KILL_WAIT, RECORD_WAIT)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn stop(_root: &Path, _term_grace: Duration) -> Result<()> {
    bail!("workers run on Linux and macOS hosts only")
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn stop_within(
    root: &Path,
    term_grace: Duration,
    kill_wait: Duration,
    record_wait: Duration,
) -> Result<()> {
    use anyhow::Context;
    use process_table::{lookup, table};

    let Some(leader) = holder_within(root, record_wait)? else {
        return Ok(());
    };
    // The harness and its helpers lead process groups of their own, so the
    // worker's group can empty while they still write into the root. The tree
    // is therefore snapshotted before any signal: the leader, its group, and
    // every descendant of either. A stop is finished when the lock is free and
    // none of them is left.
    let mut tree = Tree::snapshot(&table().context("list processes")?, leader);
    let own_group = unsafe { libc::getpgrp() } as u32;
    let mut signal_errors: Vec<String> = Vec::new();
    let done = |tree: &Tree| -> Result<bool> {
        if WorkerRootOwner::is_held(root)? {
            return Ok(false);
        }
        for member in &tree.members {
            if lookup(member.pid)
                .with_context(|| format!("look up process {}", member.pid))?
                .is_some_and(|now| now.is_live_instance_of(member))
            {
                return Ok(false);
            }
        }
        Ok(true)
    };
    let wait = |tree: &Tree, limit: Duration| -> Result<bool> {
        let deadline = Instant::now() + limit;
        loop {
            if done(tree)? {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    };

    if !term_grace.is_zero() {
        // The worker leads its own group, so TERM takes its agent down too.
        if tree.leader_leads_group {
            signal_group(leader, libc::SIGTERM, &mut signal_errors);
        } else {
            signal_process(leader, libc::SIGTERM, &mut signal_errors);
        }
        if wait(&tree, term_grace)? {
            return Ok(());
        }
        // Whatever ignored TERM, and whatever it started since, goes next.
        tree = tree.refreshed(&table().context("list processes")?);
    }
    let mut groups = std::collections::BTreeSet::new();
    for member in &tree.members {
        signal_process(member.pid, libc::SIGKILL, &mut signal_errors);
        // Group 0 and 1 are not groups of ours to signal, and our own group
        // holds this stop.
        if member.pgid > 1 && member.pgid != own_group {
            groups.insert(member.pgid);
        }
    }
    for group in groups {
        signal_group(group, libc::SIGKILL, &mut signal_errors);
    }
    if wait(&tree, kill_wait)? {
        return Ok(());
    }
    let survivors: Vec<String> = tree
        .members
        .iter()
        .filter(|member| {
            lookup(member.pid)
                .ok()
                .flatten()
                .is_some_and(|now| now.is_live_instance_of(member))
        })
        .map(|member| member.pid.to_string())
        .collect();
    bail!(
        "worker process tree still running after stop: leader {leader}, lock held: {}, \
         live processes [{}]{}",
        WorkerRootOwner::is_held(root).unwrap_or(true),
        survivors.join(" "),
        if signal_errors.is_empty() {
            String::new()
        } else {
            format!(", signal errors: {}", signal_errors.join("; "))
        }
    )
}

/// The processes a stop must remove, as they were when it looked.
#[cfg(any(target_os = "linux", target_os = "macos"))]
struct Tree {
    leader: u32,
    leader_leads_group: bool,
    members: Vec<process_table::ProcessEntry>,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Tree {
    fn snapshot(table: &[process_table::ProcessEntry], leader: u32) -> Self {
        let leader_leads_group = table
            .iter()
            .any(|entry| entry.pid == leader && entry.pgid == leader);
        Self::collect(table, leader, leader_leads_group, |entry| {
            entry.pid == leader || (leader_leads_group && entry.pgid == leader)
        })
    }

    /// The members of this tree that are still the same processes, with
    /// everything that descends from them now.
    fn refreshed(&self, table: &[process_table::ProcessEntry]) -> Self {
        Self::collect(table, self.leader, self.leader_leads_group, |entry| {
            self.members
                .iter()
                .any(|member| entry.is_live_instance_of(member))
                || (self.leader_leads_group && entry.pgid == self.leader)
        })
    }

    fn collect(
        table: &[process_table::ProcessEntry],
        leader: u32,
        leader_leads_group: bool,
        is_seed: impl Fn(&process_table::ProcessEntry) -> bool,
    ) -> Self {
        let own = std::process::id();
        let mut children: std::collections::HashMap<u32, Vec<usize>> = Default::default();
        for (index, entry) in table.iter().enumerate() {
            children.entry(entry.ppid).or_default().push(index);
        }
        let mut included = vec![false; table.len()];
        let mut pending: Vec<usize> = Vec::new();
        for (index, entry) in table.iter().enumerate() {
            if entry.pid != own && is_seed(entry) {
                included[index] = true;
                pending.push(index);
            }
        }
        while let Some(index) = pending.pop() {
            for &child in children.get(&table[index].pid).into_iter().flatten() {
                if !included[child] && table[child].pid != own {
                    included[child] = true;
                    pending.push(child);
                }
            }
        }
        Self {
            leader,
            leader_leads_group,
            members: table
                .iter()
                .zip(included)
                .filter_map(|(entry, included)| included.then_some(*entry))
                .collect(),
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn signal_process(pid: u32, signal: i32, errors: &mut Vec<String>) {
    // SAFETY: kill takes plain integers and dereferences nothing.
    if unsafe { libc::kill(pid as libc::pid_t, signal) } != 0 {
        let error = std::io::Error::last_os_error();
        // A process that is already gone is what the signal was for.
        if error.raw_os_error() != Some(libc::ESRCH) {
            errors.push(format!("signal {signal} to {pid}: {error}"));
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn signal_group(group: u32, signal: i32, errors: &mut Vec<String>) {
    if let Err(error) = mj_core::subprocess::signal_process_group(group as i32, signal) {
        errors.push(format!("signal {signal} to group {group}: {error}"));
    }
}
