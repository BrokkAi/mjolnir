//! Daemon and worker compatibility identities shared by the workspace.

use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};

/// The digest part of the identity a test binary reports: no commit has it.
/// Keep it equal to `TEST_REVISION` in `build.rs`, which stamps the fake
/// worker fixture with it.
#[cfg(any(test, feature = "test-identity"))]
const TEST_REVISION: &str = "0000000000000000000000000000000000000000";
/// The worker stamp of the test identity, as a constant so that its bytes are
/// in every test executable (see `installed`).
#[cfg(any(test, feature = "test-identity"))]
const TEST_WORKER_BUILD_STAMP: &str = concat!(
    "\0MJ-WORKER-BUILD:",
    env!("CARGO_PKG_VERSION"),
    "+0000000000000000000000000000000000000000\0"
);

/// The identities of the running program's build.
///
/// A program installs this first thing in `main` ([`install`]) and library
/// code reads it through [`build_id`], [`worker_build_id`] and
/// [`commit_time`]. Library crates do not compile the values in: a value that
/// changes with every commit would make each crate that names it, and every
/// crate above that one, stale after every commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildIdentity {
    /// Package version, `+`, and the commit: see [`build_id`].
    pub build_id: &'static str,
    /// Package version, `+`, and the worker input digest: see [`worker_build_id`].
    pub worker_build_id: &'static str,
    /// The commit's time in Unix seconds, or empty when unknown.
    pub commit_time: &'static str,
    /// See [`release_image_carries_worker`].
    pub release_image_carries_worker: bool,
}

#[cfg(any(test, feature = "test-identity"))]
impl BuildIdentity {
    /// What a test binary reports. It names no commit, so it is the same
    /// after every commit and a test build stays fresh; its commit time is
    /// unknown, as for a build without Git.
    fn for_tests() -> Self {
        Self {
            build_id: concat!(
                env!("CARGO_PKG_VERSION"),
                "+0000000000000000000000000000000000000000"
            ),
            worker_build_id: concat!(
                env!("CARGO_PKG_VERSION"),
                "+0000000000000000000000000000000000000000"
            ),
            commit_time: "",
            release_image_carries_worker: false,
        }
    }
}

static INSTALLED: std::sync::OnceLock<BuildIdentity> = std::sync::OnceLock::new();

/// Record the identity of the running program. Call it once, first thing in
/// `main`.
///
/// # Panics
///
/// When a different identity is already installed: a process has one build.
pub fn install(identity: BuildIdentity) {
    let installed = INSTALLED.get_or_init(|| identity);
    assert_eq!(
        *installed, identity,
        "a process cannot install two build identities"
    );
}

fn installed() -> &'static BuildIdentity {
    // A test binary has no `main` of ours to install one. Some tests hand
    // their own executable to the code that verifies a worker binary, so the
    // stamp has to be in that file, as it is in a real worker.
    #[cfg(any(test, feature = "test-identity"))]
    return INSTALLED.get_or_init(|| {
        debug_assert!(TEST_WORKER_BUILD_STAMP.contains(TEST_REVISION));
        std::hint::black_box(TEST_WORKER_BUILD_STAMP);
        BuildIdentity::for_tests()
    });
    #[cfg(not(any(test, feature = "test-identity")))]
    INSTALLED
        .get()
        .expect("the program did not install its build identity before using it")
}

/// Identity of this daemon build: package version plus commit. It orders
/// daemon replacements and remains visible to users and clients.
#[must_use]
pub fn build_id() -> &'static str {
    installed().build_id
}

/// Compatibility identity of workers built from the same Cargo inputs: the
/// package version plus a digest of the worker's inputs at the commit.
#[must_use]
pub fn worker_build_id() -> &'static str {
    installed().worker_build_id
}

/// The committer time, in Unix seconds, of the revision in [`build_id`], or
/// empty when the build had neither Git nor `MJ_BUILD_COMMIT_TIME`.
#[must_use]
pub fn commit_time() -> &'static str {
    installed().commit_time
}

/// Whether this is a release build whose container image
/// (`config::RELEASE_CONTAINER_IMAGE`) carries its own worker. The release
/// workflow publishes that image with the worker built from the same commit,
/// whose identity equals this build's, so a container on it needs no worker
/// from the host. Neither a development build nor a crates.io package, whose
/// identity falls back to the commit, is.
#[must_use]
pub fn release_image_carries_worker() -> bool {
    installed().release_image_carries_worker
}

/// The bytes a worker executable carries so its build can be read from the
/// file: [`worker_build_id`] between the stamp's markers.
#[must_use]
pub fn worker_build_stamp() -> &'static str {
    static STAMP: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    STAMP.get_or_init(|| format!("\0MJ-WORKER-BUILD:{}\0", worker_build_id()))
}

const PREFIX: &[u8] = b"\0MJ-WORKER-BUILD:";
// Preserve the existing stamp parser's search window, including its terminator.
const STAMP_BYTES: usize = 192;
const READ_BYTES: usize = 128 * 1024;

/// Whether `build` has the shape of a worker compatibility identity: a
/// package version, `+`, and a 40-digit hexadecimal digest. The version must
/// look like one (a digit first, then digits, letters, dots and hyphens), so
/// unrelated text the linker places between the stamp prefix constant and the
/// build id literal cannot read as a second stamp.
#[must_use]
pub fn is_worker_build(build: &str) -> bool {
    build.rsplit_once('+').is_some_and(|(version, revision)| {
        version
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_digit())
            && version
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'.' || byte == b'-')
            && revision.len() == 40
            && revision.bytes().all(|byte| byte.is_ascii_hexdigit())
    })
}

fn worker_build_stamps(bytes: &[u8]) -> impl Iterator<Item = &str> {
    memchr::memmem::find_iter(bytes, PREFIX).filter_map(|offset| {
        let tail = &bytes[offset + PREFIX.len()..];
        let end = tail.iter().take(STAMP_BYTES).position(|byte| *byte == 0)?;
        let build = std::str::from_utf8(&tail[..end]).ok()?;
        is_worker_build(build).then_some(build)
    })
}

pub fn worker_build_from_bytes(bytes: &[u8]) -> Result<&str> {
    let mut found = None;
    for build in worker_build_stamps(bytes) {
        ensure!(
            found.is_none_or(|previous| previous == build),
            "conflicting worker build stamps"
        );
        found = Some(build);
    }
    found.context("missing worker build stamp (legacy or invalid worker)")
}

fn worker_build_from_reader(input: &mut impl Read) -> Result<String> {
    // Keep only one I/O chunk and enough overlap to recognize a stamp split
    // across reads. Worker executables can be hundreds of megabytes.
    let overlap = PREFIX.len() + STAMP_BYTES;
    let mut bytes = Vec::with_capacity(READ_BYTES + overlap);
    let mut found: Option<String> = None;
    loop {
        let read = input
            .by_ref()
            .take(READ_BYTES as u64)
            .read_to_end(&mut bytes)
            .context("read worker build stamp")?;
        if read == 0 {
            break;
        }
        for build in worker_build_stamps(&bytes) {
            ensure!(
                found.as_deref().is_none_or(|previous| previous == build),
                "conflicting worker build stamps"
            );
            if found.is_none() {
                found = Some(build.to_owned());
            }
        }
        let keep_from = bytes.len().saturating_sub(overlap);
        bytes.drain(..keep_from);
    }
    found.context("missing worker build stamp (legacy or invalid worker)")
}

pub fn verify_worker_build(path: &Path) -> Result<()> {
    let mut input = std::fs::File::open(path)
        .with_context(|| format!("read worker build from {}", path.display()))?;
    let found = worker_build_from_reader(&mut input).with_context(|| {
        format!(
            "worker {}; expected build {}",
            path.display(),
            worker_build_id()
        )
    })?;
    if found != worker_build_id() {
        bail!(
            "worker {} has build {found}; expected build {}",
            path.display(),
            worker_build_id()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subprocess::{run_capturing_stdout, run_inherited};
    use std::path::PathBuf;
    use std::process::Command;

    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf()
    }

    fn input_paths() -> Vec<String> {
        std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("worker-build-inputs.txt"),
        )
        .unwrap()
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
        .collect()
    }

    fn worker_inputs_repo() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let package_root = root.join("mj-core");
        std::fs::create_dir_all(&package_root).unwrap();
        for filename in ["worker_build_inputs.py", "worker-build-inputs.txt"] {
            std::fs::copy(
                Path::new(env!("CARGO_MANIFEST_DIR")).join(filename),
                package_root.join(filename),
            )
            .unwrap();
        }
        for path in input_paths() {
            let directory = root.join(path);
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(directory.join("input.txt"), "initial worker input").unwrap();
        }
        std::fs::write(root.join("README.md"), "documentation\n").unwrap();
        git(root, ["init", "--quiet"]);
        git(root, ["config", "user.name", "Worker identity test"]);
        git(
            root,
            ["config", "user.email", "worker-identity@example.invalid"],
        );
        commit(root, "initial worker inputs");
        directory
    }

    fn git<const N: usize>(root: &Path, args: [&str; N]) {
        let mut command = Command::new("git");
        command.current_dir(root).args(args);
        let status = run_inherited(&mut command).unwrap();
        assert!(status.success(), "git {:?} exited with {status}", args);
    }

    fn commit(root: &Path, message: &str) {
        git(root, ["add", "--all"]);
        let mut command = Command::new("git");
        command
            .current_dir(root)
            .args(["commit", "--quiet", "-m", message]);
        let status = run_inherited(&mut command).unwrap();
        assert!(status.success(), "git commit exited with {status}");
    }

    fn compute_worker_inputs_id(package_root: &Path, fallback_revision: &str) -> String {
        let python = if cfg!(windows) { "python" } else { "python3" };
        let mut command = Command::new(python);
        command
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("worker_build_inputs.py"))
            .args(["--package-root"])
            .arg(package_root)
            .args(["--fallback-revision", fallback_revision])
            .env_remove("MJ_WORKER_INPUTS_ID");
        let output = run_capturing_stdout(&mut command).unwrap();
        assert!(
            output.status.success(),
            "worker input identity helper exited with {}",
            output.status
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    // Hard-won: daemon heap profile: verification must handle stamps across bounded reads.
    #[test]
    fn reads_a_foreign_binary_without_executing_it() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("linux-worker");
        for offset in [
            READ_BYTES - PREFIX.len() - 10,
            READ_BYTES - PREFIX.len(),
            READ_BYTES - 2,
            READ_BYTES + 3,
            150_000,
        ] {
            let mut bytes = vec![0xff; offset];
            bytes.extend_from_slice(worker_build_stamp().as_bytes());
            bytes.extend_from_slice(&vec![0xff; READ_BYTES]);
            // Matching stamps in separate chunks are valid, too.
            bytes.extend_from_slice(worker_build_stamp().as_bytes());
            std::fs::write(&path, &bytes).unwrap();
            assert_eq!(worker_build_from_bytes(&bytes).unwrap(), worker_build_id());
            verify_worker_build(&path).unwrap();
        }
    }

    // Hard-won: 740cb30c5: a layout change put diagnostic text and the bare build id after the prefix constant, read as a conflicting stamp.
    #[test]
    fn prefix_constant_followed_by_text_and_build_id_is_not_a_stamp() {
        let mut bytes = PREFIX.to_vec();
        bytes.extend_from_slice(
            b"read worker build stampconflicting worker build stampsmissing worker build stamp (legacy or invalid worker)",
        );
        bytes.extend_from_slice(worker_build_id().as_bytes());
        bytes.extend_from_slice(worker_build_stamp().as_bytes());
        assert_eq!(worker_build_from_bytes(&bytes).unwrap(), worker_build_id());
    }

    #[test]
    fn rejects_missing_truncated_stale_and_conflicting_stamps() {
        assert!(worker_build_from_bytes(b"legacy worker").is_err());
        assert!(
            worker_build_from_bytes(worker_build_stamp().trim_end_matches('\0').as_bytes())
                .is_err()
        );
        let stale = format!("\0MJ-WORKER-BUILD:0.1.0+{}\0", "a".repeat(40));
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("stale-worker");
        std::fs::write(&path, &stale).unwrap();
        let error = verify_worker_build(&path).unwrap_err().to_string();
        assert!(
            error.contains("0.1.0+")
                && error.contains(worker_build_id())
                && error.contains("stale-worker")
        );
        let conflicting = format!("{}{stale}", worker_build_stamp());
        assert!(
            worker_build_from_bytes(conflicting.as_bytes())
                .unwrap_err()
                .to_string()
                .contains("conflicting")
        );
        let mut bytes = worker_build_stamp().as_bytes().to_vec();
        bytes.resize(2 * READ_BYTES - PREFIX.len(), 0xff);
        bytes.extend_from_slice(stale.as_bytes());
        std::fs::write(&path, &bytes).unwrap();
        let error = format!("{:#}", verify_worker_build(&path).unwrap_err());
        assert!(error.contains("conflicting") && error.contains("stale-worker"));
        bytes.truncate(READ_BYTES - 2);
        bytes[..worker_build_stamp().len()].fill(0xff);
        bytes.extend_from_slice(worker_build_stamp().trim_end_matches('\0').as_bytes());
        std::fs::write(&path, &bytes).unwrap();
        assert!(format!("{:#}", verify_worker_build(&path).unwrap_err()).contains("missing"));
    }

    #[test]
    fn worker_inputs_identity_ignores_docs_and_tracks_worker_inputs() {
        let directory = worker_inputs_repo();
        let root = directory.path();
        let fallback_revision = "a".repeat(40);
        let initial = compute_worker_inputs_id(&root.join("mj-core"), &fallback_revision);

        std::fs::write(root.join("README.md"), "documentation changed\n").unwrap();
        commit(root, "documentation only");
        assert_eq!(
            compute_worker_inputs_id(&root.join("mj-core"), &fallback_revision),
            initial
        );

        std::fs::write(root.join("mj-worker/input.txt"), "worker input changed").unwrap();
        commit(root, "worker input changed");
        assert_ne!(
            compute_worker_inputs_id(&root.join("mj-core"), &fallback_revision),
            initial
        );
    }

    #[test]
    fn worker_inputs_override_wins() {
        let directory = tempfile::tempdir().unwrap();
        let expected = "b".repeat(40);
        let python = if cfg!(windows) { "python" } else { "python3" };
        let mut command = Command::new(python);
        command
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("worker_build_inputs.py"))
            .args(["--package-root"])
            .arg(directory.path())
            .args(["--fallback-revision", &"a".repeat(40)])
            .env("MJ_WORKER_INPUTS_ID", &expected);
        let output = run_capturing_stdout(&mut command).unwrap();
        assert!(
            output.status.success(),
            "worker input identity helper exited with {}",
            output.status
        );
        assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), expected);
    }

    #[test]
    fn worker_inputs_identity_falls_back_without_git_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let expected = "c".repeat(40);
        assert_eq!(
            compute_worker_inputs_id(directory.path(), &expected),
            expected
        );
    }

    #[test]
    fn worker_input_paths_cover_the_normal_and_build_dependency_closure() {
        let workspace = workspace_root();
        let mut command = Command::new("cargo");
        // Resolving the whole workspace needs every dependency manifest, and a
        // CI lane has only downloaded the crates its own earlier steps built.
        // An offline resolve therefore cannot see the full graph; ask cargo to
        // fetch what it needs, while `--locked` keeps this test from editing
        // Cargo.lock as a side effect.
        command
            .current_dir(&workspace)
            .args(["metadata", "--locked", "--format-version", "1"]);
        let output = run_capturing_stdout(&mut command).unwrap();
        assert!(
            output.status.success(),
            "cargo metadata exited with {}",
            output.status
        );
        let metadata: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let packages = metadata["packages"].as_array().unwrap();
        let package_by_id: std::collections::HashMap<_, _> = packages
            .iter()
            .map(|package| (package["id"].as_str().unwrap(), package))
            .collect();
        let nodes = metadata["resolve"]["nodes"].as_array().unwrap();
        let node_by_id: std::collections::HashMap<_, _> = nodes
            .iter()
            .map(|node| (node["id"].as_str().unwrap(), node))
            .collect();
        let worker_id = packages
            .iter()
            .find(|package| package["name"] == "brokk-mj-worker")
            .unwrap()["id"]
            .as_str()
            .unwrap();
        let input_paths: std::collections::HashSet<_> = input_paths().into_iter().collect();
        let mut pending = vec![worker_id];
        let mut visited = std::collections::HashSet::new();
        while let Some(package_id) = pending.pop() {
            if !visited.insert(package_id) {
                continue;
            }
            let node = node_by_id[package_id];
            for dependency in node["deps"].as_array().unwrap() {
                let workspace_build_edge = dependency["dep_kinds"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|kind| kind["kind"].is_null() || kind["kind"] == "build");
                if workspace_build_edge {
                    pending.push(dependency["pkg"].as_str().unwrap());
                }
            }
            let package = package_by_id[package_id];
            if package["source"].is_null() {
                let manifest = PathBuf::from(package["manifest_path"].as_str().unwrap());
                let relative = manifest
                    .parent()
                    .unwrap()
                    .strip_prefix(&workspace)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                assert!(
                    input_paths.contains(&relative),
                    "worker dependency {} at {relative} is missing from worker-build-inputs.txt",
                    package["name"]
                );
            }
        }
    }
}
