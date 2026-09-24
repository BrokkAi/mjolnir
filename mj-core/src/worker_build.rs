//! Build identity shared by the controller and its separately compiled workers.

use std::path::Path;

use anyhow::{Context, Result, bail, ensure};

pub const BUILD_ID: &str = env!("MJ_BUILD_ID");
/// Referenced by the worker entry point so stripping cannot discard it.
pub const WORKER_BUILD_STAMP: &str = concat!("\0MJ-WORKER-BUILD:", env!("MJ_BUILD_ID"), "\0");
const PREFIX: &[u8] = b"\0MJ-WORKER-BUILD:";

pub fn worker_build_from_bytes(bytes: &[u8]) -> Result<&str> {
    let mut found = None;
    for offset in memchr::memmem::find_iter(bytes, PREFIX) {
        let tail = &bytes[offset + PREFIX.len()..];
        let Some(end) = tail.iter().take(192).position(|byte| *byte == 0) else {
            continue;
        };
        let Ok(build) = std::str::from_utf8(&tail[..end]) else {
            continue;
        };
        let Some((version, revision)) = build.rsplit_once('+') else {
            continue;
        };
        if version.is_empty()
            || revision.len() != 40
            || !revision.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            continue;
        }
        ensure!(
            found.is_none_or(|previous| previous == build),
            "conflicting worker build stamps"
        );
        found = Some(build);
    }
    found.context("missing worker build stamp (legacy or invalid worker)")
}

pub fn verify_worker_build(path: &Path) -> Result<()> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("read worker build from {}", path.display()))?;
    let found = worker_build_from_bytes(&bytes)
        .with_context(|| format!("worker {}; expected build {BUILD_ID}", path.display()))?;
    if found != BUILD_ID {
        bail!(
            "worker {} has build {found}; expected build {BUILD_ID}",
            path.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_foreign_binary_without_executing_it() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("linux-worker");
        let mut bytes = vec![0xff; 150_000];
        bytes.extend_from_slice(WORKER_BUILD_STAMP.as_bytes());
        bytes.extend_from_slice(&[0xff; 1000]);
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(worker_build_from_bytes(&bytes).unwrap(), BUILD_ID);
        verify_worker_build(&path).unwrap();
    }

    #[test]
    fn rejects_missing_truncated_stale_and_conflicting_stamps() {
        assert!(worker_build_from_bytes(b"legacy worker").is_err());
        assert!(
            worker_build_from_bytes(WORKER_BUILD_STAMP.trim_end_matches('\0').as_bytes()).is_err()
        );
        let stale = format!("\0MJ-WORKER-BUILD:0.1.0+{}\0", "a".repeat(40));
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("stale-worker");
        std::fs::write(&path, &stale).unwrap();
        let error = verify_worker_build(&path).unwrap_err().to_string();
        assert!(
            error.contains("0.1.0+") && error.contains(BUILD_ID) && error.contains("stale-worker")
        );
        let conflicting = format!("{WORKER_BUILD_STAMP}{stale}");
        assert!(
            worker_build_from_bytes(conflicting.as_bytes())
                .unwrap_err()
                .to_string()
                .contains("conflicting")
        );
    }
}
