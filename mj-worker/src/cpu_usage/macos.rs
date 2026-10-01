use anyhow::{Context, Result, bail};
use std::collections::BTreeSet;
use std::time::Duration;

fn child_pids(pid: i32) -> Result<Vec<i32>> {
    // Apple's libproc wrapper returns a PID count, not bytes. Grow until the
    // result fits, since a process can spawn children between the two calls.
    let mut capacity = 64;
    loop {
        let mut children = vec![0_i32; capacity];
        let bytes = i32::try_from(children.len() * std::mem::size_of::<i32>())?;
        unsafe {
            *libc::__error() = 0;
        }
        let count = unsafe { libc::proc_listchildpids(pid, children.as_mut_ptr().cast(), bytes) };
        if count < 0 || (count == 0 && unsafe { *libc::__error() } != 0) {
            return Err(std::io::Error::last_os_error().into());
        }
        if (count as usize) < capacity {
            children.truncate(count as usize);
            return Ok(children);
        }
        capacity = capacity
            .checked_mul(2)
            .context("too many child processes")?;
    }
}

pub(super) fn process_tree_cpu_time(root: u32) -> Result<Duration> {
    let mut timebase = libc::mach_timebase_info { numer: 0, denom: 0 };
    if unsafe { libc::mach_timebase_info(&mut timebase) } != 0 || timebase.denom == 0 {
        bail!("could not read Mach time base");
    }
    let mut pending = vec![i32::try_from(root)?];
    let mut visited = BTreeSet::new();
    let mut ticks = 0_u128;
    while let Some(pid) = pending.pop() {
        if !visited.insert(pid) {
            continue;
        }
        let mut usage = std::mem::MaybeUninit::<libc::rusage_info_v2>::zeroed();
        let result =
            unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V2, usage.as_mut_ptr().cast()) };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if pid as u32 != root && error.raw_os_error() == Some(libc::ESRCH) {
                continue;
            }
            return Err(error).context("read process CPU time");
        }
        let usage = unsafe { usage.assume_init() };
        ticks += u128::from(usage.ri_user_time)
            + u128::from(usage.ri_system_time)
            + u128::from(usage.ri_child_user_time)
            + u128::from(usage.ri_child_system_time);
        match child_pids(pid) {
            Ok(children) => pending.extend(children),
            Err(error)
                if pid as u32 != root
                    && error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|error| error.raw_os_error() == Some(libc::ESRCH)) => {}
            Err(error) => return Err(error.context("list process children")),
        }
    }
    let nanos = ticks * u128::from(timebase.numer) / u128::from(timebase.denom);
    Ok(Duration::new(
        u64::try_from(nanos / 1_000_000_000)?,
        (nanos % 1_000_000_000) as u32,
    ))
}
