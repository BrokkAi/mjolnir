//! The identity of the build a Mjolnir program was compiled from: its commit,
//! that commit's time, and the digest of the worker's inputs.
//!
//! Only the programs (`mj` and `mj-worker`) depend on this crate. Its build
//! script reads the current commit, so the crate is rebuilt after every
//! commit, and so is everything that depends on it. A program installs
//! [`identity`] first thing in `main`
//! ([`mj_core::worker_build::install`]) and library code reads it from there,
//! which is what keeps a commit from recompiling the libraries.

#[cfg(test)]
mj_core::enable_test_fixtures_on_startup!();

use mj_core::worker_build::BuildIdentity;

/// Identity of this build: package version plus commit. It orders daemon
/// replacements and remains visible to users and clients.
pub const BUILD_ID: &str = env!("MJ_BUILD_ID");
/// Compatibility identity of workers built from the same Cargo inputs. The
/// digest is based on Git trees at `HEAD`, so uncommitted changes are not
/// reflected, matching [`BUILD_ID`].
pub const WORKER_BUILD_ID: &str = env!("MJ_WORKER_BUILD_ID");
/// The committer time, in Unix seconds, of the revision in [`BUILD_ID`], or
/// empty when the build had neither Git nor `MJ_BUILD_COMMIT_TIME`.
pub const BUILD_COMMIT_TIME: &str = env!("MJ_BUILD_COMMIT_TIME");
/// The bytes that mark a worker executable with [`WORKER_BUILD_ID`]. The
/// worker's entry point references this so stripping cannot discard it.
pub const WORKER_BUILD_STAMP: &str =
    concat!("\0MJ-WORKER-BUILD:", env!("MJ_WORKER_BUILD_ID"), "\0");
/// Whether this is a release build whose container image carries its own
/// worker. The release workflow publishes that image with the worker built
/// from the same commit, whose identity equals this build's, so a container
/// on it needs no worker from the host. Neither a development build nor a
/// crates.io package, whose identity falls back to the commit, is.
pub const RELEASE_IMAGE_CARRIES_WORKER: bool =
    matches!(env!("MJ_RELEASE_IMAGE_CARRIES_WORKER").as_bytes(), b"1");

/// What a program installs at startup.
#[must_use]
pub const fn identity() -> BuildIdentity {
    BuildIdentity {
        build_id: BUILD_ID,
        worker_build_id: WORKER_BUILD_ID,
        commit_time: BUILD_COMMIT_TIME,
        release_image_carries_worker: RELEASE_IMAGE_CARRIES_WORKER,
    }
}

/// A worker stand-in carrying this build's worker stamp, for a test in which
/// a real program verifies the worker. Built before tests start, so executing
/// it cannot race a test's writer.
#[cfg(unix)]
#[doc(hidden)]
#[must_use]
pub fn fake_worker_dispatcher() -> std::path::PathBuf {
    std::path::Path::new(env!("OUT_DIR")).join("fake-worker.sh")
}

#[cfg(test)]
mod tests {
    use mj_core::{test_hooks, worker_build};

    #[test]
    fn test_startup_enables_the_fallback_identity_without_runtime_hooks() {
        assert!(test_hooks::test_fixtures_enabled());
        assert!(worker_build::worker_build_id().ends_with(&format!("+{:040x}", 0)));
        assert!(!test_hooks::test_hooks_enabled());
    }
}
