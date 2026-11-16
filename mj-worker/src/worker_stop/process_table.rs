//! One process's facts, read from the kernel without running `ps`.

use std::io;

/// What a stop needs to know about a process. `start` is an opaque start-time
/// stamp: a pid whose stamp changed is a different process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ProcessEntry {
    pub pid: u32,
    pub ppid: u32,
    pub pgid: u32,
    pub start: u64,
    pub zombie: bool,
}

impl ProcessEntry {
    /// Whether `self` is the process `earlier` described, still running.
    pub(super) fn is_live_instance_of(&self, earlier: &ProcessEntry) -> bool {
        self.pid == earlier.pid && self.start == earlier.start && !self.zombie
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::*;

    pub(in super::super) fn lookup(pid: u32) -> io::Result<Option<ProcessEntry>> {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(text) => parse(pid, &text).map(Some),
            // A process can vanish between listing and reading it.
            Err(error)
                if error.kind() == io::ErrorKind::NotFound
                    || error.raw_os_error() == Some(libc::ESRCH) =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    pub(in super::super) fn table() -> io::Result<Vec<ProcessEntry>> {
        let mut entries = Vec::new();
        for entry in std::fs::read_dir("/proc")? {
            let Some(pid) = entry?
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            else {
                continue;
            };
            if let Some(entry) = lookup(pid)? {
                entries.push(entry);
            }
        }
        Ok(entries)
    }

    /// The command name may hold spaces and parentheses, so fields are counted
    /// from the last `)`. The tail starts at field 3 (state): ppid is field 4,
    /// pgrp 5 and starttime 22.
    pub(super) fn parse(pid: u32, text: &str) -> io::Result<ProcessEntry> {
        let invalid =
            || io::Error::new(io::ErrorKind::InvalidData, format!("bad /proc/{pid}/stat"));
        let (_, tail) = text.rsplit_once(')').ok_or_else(invalid)?;
        let fields: Vec<&str> = tail.split_whitespace().collect();
        let field = |index: usize| fields.get(index).copied().ok_or_else(invalid);
        Ok(ProcessEntry {
            pid,
            ppid: field(1)?.parse().map_err(|_| invalid())?,
            pgid: field(2)?.parse().map_err(|_| invalid())?,
            start: field(19)?.parse().map_err(|_| invalid())?,
            zombie: matches!(field(0)?, "Z" | "X"),
        })
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::*;

    pub(in super::super) fn lookup(pid: u32) -> io::Result<Option<ProcessEntry>> {
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        // SAFETY: the buffer is `size` bytes; the kernel fills at most that.
        let written = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                size,
            )
        };
        if written == size {
            // SAFETY: the kernel wrote the whole structure.
            let info = unsafe { info.assume_init() };
            return Ok(Some(ProcessEntry {
                pid,
                ppid: info.pbi_ppid,
                pgid: info.pbi_pgid,
                start: info
                    .pbi_start_tvsec
                    .saturating_mul(1_000_000)
                    .saturating_add(info.pbi_start_tvusec),
                zombie: info.pbi_status == libc::SZOMB,
            }));
        }
        let error = io::Error::last_os_error();
        // Libproc reports a missing process as ESRCH, or as zero bytes.
        if written == 0 || error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(None);
        }
        Err(error)
    }

    pub(in super::super) fn table() -> io::Result<Vec<ProcessEntry>> {
        // The count is in pids, not bytes. Processes can start between the two
        // calls, so leave room and retry when the list filled the buffer.
        let mut capacity = {
            // SAFETY: a null buffer asks only for the count.
            let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
            if count < 0 {
                return Err(io::Error::last_os_error());
            }
            count as usize + 64
        };
        loop {
            let mut pids = vec![0 as libc::pid_t; capacity];
            let bytes = (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
            // SAFETY: the buffer is `bytes` long.
            let count = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
            if count < 0 {
                return Err(io::Error::last_os_error());
            }
            if (count as usize) < capacity {
                pids.truncate(count as usize);
                let mut entries = Vec::new();
                for pid in pids.into_iter().filter(|pid| *pid > 0) {
                    if let Some(entry) = lookup(pid as u32)? {
                        entries.push(entry);
                    }
                }
                return Ok(entries);
            }
            capacity *= 2;
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(super) use imp::{lookup, table};

#[cfg(all(test, target_os = "linux"))]
mod tests {
    #[test]
    fn a_command_name_with_spaces_and_parentheses_does_not_shift_the_fields() {
        let entry = super::imp::parse(
            42,
            "42 (a) b (c) S 7 9 9 0 -1 4194560 0 0 0 0 0 0 0 0 20 0 1 0 123456 0 0",
        )
        .unwrap();
        assert_eq!((entry.ppid, entry.pgid, entry.start), (7, 9, 123456));
        assert!(!entry.zombie);
    }
}
