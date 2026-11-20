//! The live worker owns the stop and reports its result even across daemon
//! replacement. A receipt is published only after the harness and every
//! attributable descendant have exited.

use std::io;
#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use mj_core::worker_suspend::{self as wire, SuspendReceipt, SuspendRequest, WorkerIdentity};
use tokio::signal::unix::{Signal, SignalKind};

use super::process_table::{ProcessEntry, lookup, table};
#[cfg(target_os = "linux")]
use crate::work_facts::Marker;

#[cfg(any(target_os = "macos", test))]
mod darwin_environment;

pub struct SuspendSignal {
    root: PathBuf,
    worker: WorkerIdentity,
    signal: Signal,
}

impl SuspendSignal {
    pub fn install(root: &Path) -> Result<Option<Self>> {
        #[cfg(target_os = "linux")]
        if !mj_core::subprocess::orphans::active() {
            return Ok(None);
        }
        let signal = tokio::signal::unix::signal(SignalKind::user_defined1())?;
        let pid = std::process::id();
        let start = lookup(pid)?
            .context("read this worker's process identity")?
            .start;
        let worker = WorkerIdentity { pid, start };
        wire::write(root, wire::CAPABILITY, &worker)?;
        Ok(Some(Self {
            root: root.to_path_buf(),
            worker,
            signal,
        }))
    }

    pub async fn wait(&mut self) -> Result<SuspendRequest> {
        loop {
            if let Some(request) = wire::read::<SuspendRequest>(&self.root, wire::REQUEST)?
                && request.worker == self.worker
            {
                return Ok(request);
            }
            // The request is durable before the sender wakes us. Poll its
            // record too so a killed sender cannot leave the worker waiting.
            tokio::select! {
                _ = self.signal.recv() => {},
                () = tokio::time::sleep(std::time::Duration::from_secs(1)) => {},
            }
        }
    }
}

/// Start (or redeliver) a request for this exact worker process. The worker's
/// capability is published only after installing the signal receiver.
pub fn request(
    root: &Path,
    operation: &str,
    checkout: &Path,
    child_sessions: Vec<String>,
) -> Result<()> {
    if wire::read::<SuspendReceipt>(root, wire::RECEIPT)?
        .is_some_and(|receipt| receipt.operation == operation)
    {
        return Ok(());
    }
    let identity = wire::read::<WorkerIdentity>(root, wire::CAPABILITY)?
        .context("this worker does not support managed checkout quiescence")?;
    let Some(worker) = lookup(identity.pid)? else {
        anyhow::bail!("worker exited before confirming checkout quiescence")
    };
    ensure!(
        worker.start == identity.start && !worker.zombie,
        "worker exited before confirming checkout quiescence"
    );
    ensure!(
        super::holder(root)? == Some(worker.pid),
        "worker no longer owns the requested checkout"
    );
    if !wire::read::<SuspendRequest>(root, wire::REQUEST)?
        .is_some_and(|request| request.operation == operation && request.worker == identity)
    {
        let request = SuspendRequest {
            operation: operation.to_owned(),
            worker: identity,
            checkout: checkout.to_path_buf(),
            child_sessions,
        };
        wire::write(root, wire::REQUEST, &request)?;
    }
    if let Err(error) = signal_verified(&worker, libc::SIGUSR1, |_| true) {
        tracing::warn!(%error, "could not wake worker for its durable suspend request");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn signal_verified(
    expected: &ProcessEntry,
    signal_number: i32,
    still_owned: impl FnOnce(&ProcessEntry) -> bool,
) -> io::Result<()> {
    // First pin the kernel process identity. The subsequent stat and ownership
    // reads can reject a recycled PID, and signalling never uses the number.
    let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, expected.pid, 0) };
    if descriptor < 0 {
        return disappeared(io::Error::last_os_error());
    }
    // SAFETY: successful pidfd_open transferred a new descriptor.
    let descriptor = unsafe { OwnedFd::from_raw_fd(descriptor as i32) };
    if lookup(expected.pid)?
        .is_some_and(|entry| entry.is_live_instance_of(expected) && still_owned(&entry))
    {
        // SAFETY: pidfd_send_signal uses a live descriptor and an optional siginfo pointer.
        let result = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                descriptor.as_raw_fd(),
                signal_number,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        if result < 0 {
            return disappeared(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn signal_verified(
    expected: &ProcessEntry,
    signal_number: i32,
    still_owned: impl FnOnce(&ProcessEntry) -> bool,
) -> io::Result<()> {
    // macOS has no pidfd. Recheck ownership and birth immediately before kill.
    if lookup(expected.pid)?.is_some_and(|entry| entry.is_live_instance_of(expected) && still_owned(&entry))
        && lookup(expected.pid)?.is_some_and(|entry| entry.is_live_instance_of(expected))
        // SAFETY: kill takes integer arguments and does not dereference anything.
        && unsafe { libc::kill(expected.pid as libc::pid_t, signal_number) } != 0
    {
        return disappeared(io::Error::last_os_error());
    }
    Ok(())
}

fn disappeared(error: io::Error) -> io::Result<()> {
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(error)
    }
}

#[cfg(target_os = "linux")]
fn checkout_holders(checkout: &Path) -> Result<Vec<String>> {
    let checkout = checkout.canonicalize()?;
    let mut holders = Vec::new();
    for process in table()? {
        if process.zombie {
            continue;
        }
        let pid = process.pid;
        let directory = PathBuf::from(format!("/proc/{pid}"));
        let inside = |path: PathBuf| {
            std::fs::read_link(path).is_ok_and(|target| target.starts_with(&checkout))
        };
        let holds_files = inside(directory.join("cwd"))
            || inside(directory.join("root"))
            || std::fs::read_dir(directory.join("fd")).is_ok_and(|entries| {
                entries
                    .filter_map(std::result::Result::ok)
                    .any(|entry| inside(entry.path()))
            });
        if holds_files && lookup(pid)?.is_some_and(|current| current.is_live_instance_of(&process))
        {
            let command = std::fs::read(directory.join("cmdline")).unwrap_or_default();
            let command = String::from_utf8_lossy(&command).replace('\0', " ");
            holders.push(format!("{pid} {}", command.trim()));
        }
    }
    Ok(holders)
}

#[cfg(target_os = "macos")]
fn checkout_holders(checkout: &Path) -> Result<Vec<String>> {
    let checkout = checkout.canonicalize()?;
    let output = mj_core::subprocess::run_with_input(
        std::process::Command::new("/usr/sbin/lsof")
            .args(["-nP", "-Fpc", "+D"])
            .arg(&checkout),
        &[],
    )?;
    if output.status.code() == Some(1) && output.stdout.is_empty() && output.stderr.is_empty() {
        return Ok(Vec::new());
    }
    ensure!(
        output.status.success(),
        "cannot check open files in {}: {}",
        checkout.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8(output.stdout)?
        .lines()
        .map(str::to_owned)
        .collect())
}

#[cfg(target_os = "linux")]
fn marked_process(pid: u32, session_id: &str) -> bool {
    crate::work_facts::linux::session_marker(pid, format!("MJ_SESSION_ID={session_id}").as_bytes())
        == Marker::Present
}

#[cfg(target_os = "macos")]
fn marked_process(pid: u32, session_id: &str) -> bool {
    darwin_environment::has_marker(pid, format!("MJ_SESSION_ID={session_id}").as_bytes())
}

fn terminate_session_processes(
    root: &Path,
    session_id: &str,
    request: &SuspendRequest,
) -> Result<()> {
    #[cfg(target_os = "linux")]
    use mj_core::subprocess::orphans::{has_children, reap_adopted_children};

    let worker_pid = std::process::id();
    let session_ids = std::iter::once(session_id)
        .chain(request.child_sessions.iter().map(String::as_str))
        .collect::<Vec<_>>();
    let marked = |pid| session_ids.iter().any(|id| marked_process(pid, id));
    let tree_contains = |process: &ProcessEntry| {
        // Walk to this worker while validating each live parent. A descendant
        // reparented mid-scan is found on the next pass.
        let mut parent = process.ppid;
        while parent > 1 {
            if parent == worker_pid {
                return true;
            }
            let Ok(Some(entry)) = lookup(parent) else {
                return false;
            };
            if entry.start > process.start || entry.ppid == parent {
                return false;
            }
            parent = entry.ppid;
        }
        false
    };

    let mut waiting_for = Vec::new();
    loop {
        #[cfg(target_os = "linux")]
        let no_children = {
            reap_adopted_children()?;
            !has_children()?
        };
        #[cfg(target_os = "macos")]
        let no_children = true;
        let mut survivors = Vec::new();
        for entry in table()? {
            if entry.pid == worker_pid
                || entry.zombie
                || !(marked(entry.pid) || tree_contains(&entry))
            {
                continue;
            }
            signal_verified(&entry, libc::SIGKILL, |entry| {
                marked(entry.pid) || tree_contains(entry)
            })
            .with_context(|| format!("terminate session process {}", entry.pid))?;
            survivors.push(entry.pid);
        }
        if no_children && survivors.is_empty() {
            break;
        }
        survivors.sort_unstable();
        if waiting_for != survivors {
            let progress = wire::SuspendProgress {
                operation: request.operation.clone(),
                remaining_pids: survivors,
            };
            wire::write(root, wire::PROGRESS, &progress)?;
            waiting_for = progress.remaining_pids;
        }
        std::thread::sleep(super::POLL_INTERVAL);
    }
    let holders = checkout_holders(&request.checkout)?;
    ensure!(
        holders.is_empty(),
        "Mjolnir cannot prove these processes belong to this session; stop them, then retry: {}",
        holders.join("; ")
    );
    Ok(())
}

pub async fn wait(signal: &mut Option<SuspendSignal>) -> Result<SuspendRequest> {
    match signal {
        Some(signal) => signal.wait().await,
        None => std::future::pending().await,
    }
}

pub async fn finish(root: PathBuf, session_id: String, request: SuspendRequest) -> Result<()> {
    #[cfg(target_os = "linux")]
    mj_core::subprocess::orphans::seal_spawns();
    mj_core::runtime::spawn_blocking(move || {
        let outcome = terminate_session_processes(&root, &session_id, &request);
        let receipt = SuspendReceipt {
            operation: request.operation,
            error: outcome.err().map(|error| format!("{error:#}")),
        };
        wire::write(&root, wire::RECEIPT, &receipt)
    })
    .await
    .context("join worker quiescence")?
}
