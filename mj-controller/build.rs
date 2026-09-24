//! Record the git revision this crate is built from as `MJ_BUILD_GIT_SHA`,
//! which `mj_core::worker_build_marker!` compiles into the worker build stamp.
//! mj-worker/build.rs is the same script: the controller and the worker
//! must derive their stamps the same way.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=MJ_BUILD_GIT_SHA");
    // A packager building from a source archive can supply the revision.
    let sha = match std::env::var("MJ_BUILD_GIT_SHA") {
        Ok(sha) => sha.trim().to_owned(),
        Err(_) => git_head().unwrap_or_default(),
    };
    println!("cargo:rustc-env=MJ_BUILD_GIT_SHA={sha}");
}

/// The checked-out commit, or `None` outside a git checkout, such as a crate
/// built from crates.io. Rebuild when HEAD moves: a commit changes the branch
/// ref, a checkout changes HEAD, and `git gc` moves refs into packed-refs.
fn git_head() -> Option<String> {
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR")?);
    let sha = git(&manifest, &["rev-parse", "HEAD"])?;
    for tracked in ["HEAD", "refs/heads", "packed-refs"] {
        if let Some(path) = git(&manifest, &["rev-parse", "--git-path", tracked]) {
            let path = manifest.join(path);
            if path.exists() {
                println!("cargo:rerun-if-changed={}", path.display());
            }
        }
    }
    sha.bytes()
        .all(|byte| byte.is_ascii_hexdigit())
        .then_some(sha)
}

fn git(directory: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(directory)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|text| !text.is_empty())
}
