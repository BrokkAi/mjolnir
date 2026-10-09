//! Daemon and worker compatibility identities shared by the workspace.

use std::path::Path;

use anyhow::{Context, Result, bail, ensure};

/// Identity of this daemon build: package version plus commit. It orders
/// daemon replacements and remains visible to users and clients.
pub const BUILD_ID: &str = env!("MJ_BUILD_ID");
/// Compatibility identity of workers built from the same Cargo inputs. The
/// digest is based on Git trees at `HEAD`, so uncommitted changes are not
/// reflected, matching [`BUILD_ID`].
pub const WORKER_BUILD_ID: &str = env!("MJ_WORKER_BUILD_ID");
/// The committer time, in Unix seconds, of the revision in [`BUILD_ID`], or
/// empty when the build had neither Git nor `MJ_BUILD_COMMIT_TIME`.
pub const BUILD_COMMIT_TIME: &str = env!("MJ_BUILD_COMMIT_TIME");
/// Referenced by the worker entry point so stripping cannot discard it.
pub const WORKER_BUILD_STAMP: &str =
    concat!("\0MJ-WORKER-BUILD:", env!("MJ_WORKER_BUILD_ID"), "\0");
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
    let found = worker_build_from_bytes(&bytes).with_context(|| {
        format!(
            "worker {}; expected build {WORKER_BUILD_ID}",
            path.display()
        )
    })?;
    if found != WORKER_BUILD_ID {
        bail!(
            "worker {} has build {found}; expected build {WORKER_BUILD_ID}",
            path.display()
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

    #[test]
    fn reads_a_foreign_binary_without_executing_it() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("linux-worker");
        let mut bytes = vec![0xff; 150_000];
        bytes.extend_from_slice(WORKER_BUILD_STAMP.as_bytes());
        bytes.extend_from_slice(&[0xff; 1000]);
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(worker_build_from_bytes(&bytes).unwrap(), WORKER_BUILD_ID);
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
            error.contains("0.1.0+")
                && error.contains(WORKER_BUILD_ID)
                && error.contains("stale-worker")
        );
        let conflicting = format!("{WORKER_BUILD_STAMP}{stale}");
        assert!(
            worker_build_from_bytes(conflicting.as_bytes())
                .unwrap_err()
                .to_string()
                .contains("conflicting")
        );
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
        command
            .current_dir(&workspace)
            .args(["metadata", "--offline", "--format-version", "1"]);
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
