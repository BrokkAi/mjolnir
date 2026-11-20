//! A worker reaps the children the kernel reparented to it. Children spawned
//! through the subprocess helpers keep their normal std/Tokio wait owners.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::sync::{Mutex, OnceLock};

#[derive(Default)]
struct Children {
    spawns_sealed: bool,
    registered: BTreeMap<u32, u64>,
}

static CHILDREN: OnceLock<Mutex<Children>> = OnceLock::new();

pub fn active() -> bool {
    CHILDREN.get().is_some()
}

pub fn seal_spawns() {
    if let Some(children) = CHILDREN.get() {
        children
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .spawns_sealed = true;
    }
}

pub fn become_subreaper() -> io::Result<()> {
    // SAFETY: this process accepts reparenting; prctl takes integer arguments.
    if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    CHILDREN.get_or_init(Default::default);
    Ok(())
}

fn start_ticks(pid: u32) -> io::Result<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    stat.rsplit_once(')')
        .and_then(|(_, fields)| fields.split_whitespace().nth(19))
        .and_then(|ticks| ticks.parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid process start time"))
}

pub(super) fn tracked<T>(
    spawn: impl FnOnce() -> io::Result<T>,
    pid: impl FnOnce(&T) -> Option<u32>,
) -> io::Result<T> {
    let Some(children) = CHILDREN.get() else {
        return spawn();
    };
    let mut children = children
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if children.spawns_sealed {
        return Err(io::Error::other("worker is quiescing its checkout"));
    }
    let child = spawn()?;
    if let Some(pid) = pid(&child) {
        match start_ticks(pid) {
            Ok(ticks) => {
                children.registered.insert(pid, ticks);
            }
            // A wait owner already collected this process.
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            // Preserve the wait owner even if procfs is temporarily unreadable.
            Err(error) => {
                tracing::warn!(pid, %error, "could not read spawned child's start time");
                children.registered.insert(pid, 0);
            }
        }
    }
    Ok(child)
}

fn child_pids() -> io::Result<Vec<u32>> {
    let mut result = Vec::new();
    for thread in fs::read_dir("/proc/self/task")? {
        let path = thread?.path().join("children");
        let children = match fs::read_to_string(path) {
            Ok(children) => children,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        result.extend(
            children
                .split_whitespace()
                .filter_map(|pid| pid.parse::<u32>().ok()),
        );
    }
    Ok(result)
}

pub fn reap_adopted_children() -> io::Result<()> {
    let Some(children) = CHILDREN.get() else {
        return Err(io::Error::other("worker is not a subreaper"));
    };
    let mut children = children
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let direct = child_pids()?;
    for pid in direct {
        let ticks = match start_ticks(pid) {
            Ok(ticks) => ticks,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if children
            .registered
            .get(&pid)
            .is_some_and(|spawn| *spawn == ticks || *spawn == 0)
        {
            continue;
        }
        // SAFETY: waitpid checks that this is our child. WNOHANG never waits
        // for a live child, and nobody owns the wait for this adopted child.
        let reaped =
            unsafe { libc::waitpid(pid as libc::pid_t, std::ptr::null_mut(), libc::WNOHANG) };
        if reaped < 0 {
            let error = io::Error::last_os_error();
            if !matches!(error.raw_os_error(), Some(libc::ECHILD | libc::EINTR)) {
                return Err(error);
            }
        }
    }
    // A running child still needs its own wait even if a /proc task list
    // changed while it was enumerated. Remove entries only by identity.
    children
        .registered
        .retain(|pid, ticks| start_ticks(*pid).is_ok_and(|now| now == *ticks || *ticks == 0));
    Ok(())
}

pub fn has_children() -> io::Result<bool> {
    loop {
        // SAFETY: waitid fills this out pointer and WNOWAIT leaves ownership
        // with the normal waiter (or the orphan reaper).
        let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
        let result = unsafe {
            libc::waitid(
                libc::P_ALL,
                0,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result == 0 {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::ECHILD) => return Ok(false),
            Some(libc::EINTR) => {}
            _ => return Err(error),
        }
    }
}
