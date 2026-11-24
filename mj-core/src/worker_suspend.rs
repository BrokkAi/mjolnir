//! Target-owned process quiescence requests and receipts for an accepted suspend.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const ACCEPTED: &str = "suspend-operation.json";
pub const CAPABILITY: &str = "suspend-worker.json";
pub const REQUEST: &str = "suspend-request.json";
pub const RECEIPT: &str = "suspend-receipt.json";
pub const PROGRESS: &str = "suspend-progress.json";
pub const RESTART: &str = "suspend-restart.json";

/// The checkout and process scope of an accepted checkpoint-only suspend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SuspendCapture {
    pub operation: String,
    pub checkout: PathBuf,
    pub child_sessions: Vec<String>,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerIdentity {
    pub pid: u32,
    pub start: u64,
}

impl WorkerIdentity {
    pub fn is_alive(self) -> Result<bool> {
        #[cfg(target_os = "linux")]
        {
            let stat = match std::fs::read_to_string(format!("/proc/{}/stat", self.pid)) {
                Ok(stat) => stat,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                Err(error) => return Err(error.into()),
            };
            let (_, fields) = stat.rsplit_once(')').context("malformed worker stat")?;
            let mut fields = fields.split_whitespace();
            let state = fields
                .next()
                .context("worker stat is missing its process state")?;
            let start: u64 = fields
                .nth(18)
                .context("worker stat is missing its birth identity")?
                .parse()?;
            Ok(start == self.start && !matches!(state, "Z" | "X"))
        }
        #[cfg(target_os = "macos")]
        {
            let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
            let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
            // SAFETY: the buffer has exactly the size passed to libproc.
            let written = unsafe {
                libc::proc_pidinfo(
                    self.pid as libc::c_int,
                    libc::PROC_PIDTBSDINFO,
                    0,
                    info.as_mut_ptr().cast(),
                    size,
                )
            };
            if written == size {
                // SAFETY: libproc wrote the entire buffer.
                let info = unsafe { info.assume_init() };
                let start = info
                    .pbi_start_tvsec
                    .saturating_mul(1_000_000)
                    .saturating_add(info.pbi_start_tvusec);
                Ok(start == self.start && info.pbi_status != libc::SZOMB)
            } else {
                Ok(false)
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        anyhow::bail!("worker process identity requires Linux or macOS")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuspendRequest {
    pub operation: String,
    pub worker: WorkerIdentity,
    pub checkout: PathBuf,
    #[serde(default)]
    pub child_sessions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuspendReceipt {
    pub operation: String,
    pub error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SuspendProgress {
    pub operation: String,
    pub remaining_pids: Vec<u32>,
}

pub fn read<T: serde::de::DeserializeOwned>(root: &Path, name: &str) -> Result<Option<T>> {
    let path = root.join(name);
    match std::fs::read(&path) {
        Ok(bytes) => Ok(Some(
            serde_json::from_slice(&bytes).with_context(|| format!("decode {}", path.display()))?,
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

pub fn write<T: Serialize>(root: &Path, name: &str, value: &T) -> Result<()> {
    crate::config::atomic_write_existing(&root.join(name), &serde_json::to_vec(value)?)
}
