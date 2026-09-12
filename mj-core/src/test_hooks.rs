//! Crash-boundary hooks for the isolated reliability laboratory.
//!
//! The default build contains only the no-op function. Environment lookup,
//! marker files, and waiting are compiled in solely with `test-hooks`.

use anyhow::Result;

#[cfg(not(feature = "test-hooks"))]
#[inline]
pub fn reach_test_hook(_name: &'static str) -> Result<()> {
    Ok(())
}

#[cfg(feature = "test-hooks")]
pub fn reach_test_hook(name: &'static str) -> Result<()> {
    use std::fs::OpenOptions;
    use std::io::Write as _;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use anyhow::{Context as _, bail, ensure};

    if std::env::var_os("MJ_TEST_HOOK").as_deref() != Some(std::ffi::OsStr::new(name)) {
        return Ok(());
    }
    ensure!(
        std::env::var_os("MJ_CHAOS_ISOLATED").as_deref() == Some(std::ffi::OsStr::new("1")),
        "test hook {name} requires MJ_CHAOS_ISOLATED=1"
    );
    ensure!(
        name.bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_'),
        "invalid test hook name {name:?}"
    );
    let directory = PathBuf::from(
        std::env::var_os("MJ_TEST_HOOK_DIR")
            .context("active test hook requires MJ_TEST_HOOK_DIR")?,
    );
    ensure!(
        directory.is_dir(),
        "test hook directory {} does not exist",
        directory.display()
    );
    let reached = directory.join(format!("{name}.reached"));
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&reached)
    {
        Ok(mut marker) => {
            marker
                .write_all(format!("pid={}\n", std::process::id()).as_bytes())
                .with_context(|| format!("write test hook marker {}", reached.display()))?;
            marker
                .sync_all()
                .with_context(|| format!("sync test hook marker {}", reached.display()))?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("create test hook marker {}", reached.display()));
        }
    }

    let continuation = directory.join(format!("{name}.continue"));
    let deadline = Instant::now() + Duration::from_secs(120);
    while !continuation.is_file() {
        if Instant::now() >= deadline {
            bail!(
                "test hook {name} timed out waiting for {}",
                continuation.display()
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

#[cfg(all(test, feature = "test-hooks"))]
mod tests {
    use super::*;

    #[test]
    fn inactive_hook_does_not_require_isolation_environment() {
        reach_test_hook("not_selected").unwrap();
    }
}
