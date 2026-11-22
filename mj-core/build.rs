use std::path::{Path, PathBuf};

/// The registry repository for release images. Keep it
/// equal to `mj_core::config::CONTAINER_IMAGE_REPOSITORY`, which
/// `the_baked_release_image_agrees_with_the_channel_rule` checks.
const AGENT_DEV_IMAGE_REPOSITORY: &str = "ghcr.io/brokkai/mjolnir/agent-dev";

/// The digest part of the identity a test binary reports. Keep it equal to
/// `TEST_REVISION` in `src/worker_build.rs`.
const TEST_REVISION: &str = "0000000000000000000000000000000000000000";

fn tracked_read(path: &Path) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(value) => {
            println!("cargo:rerun-if-changed={}", path.display());
            Some(value)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => panic!("read build revision from {}: {error}", path.display()),
    }
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // This script must not read Git. Every crate depends on this one, and
    // Cargo rebuilds a crate and its dependents whenever its build script
    // reruns, so a script that followed `HEAD` would recompile the workspace
    // after every commit. The commit and the worker input digest are compiled
    // into `mj-build-id`, which only the programs depend on.
    let root = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    // Which container image a session uses when its target names none. A
    // release runs the immutable image published for its own version; a
    // development build runs the floating image published from master. The
    // release workflow sets MJ_BUILD_CHANNEL, and a crates.io source package
    // carries .cargo_vcs_info.json, so both count as releases. Keep this rule
    // equal to the one in mj-build-id/build.rs.
    println!("cargo:rerun-if-env-changed=MJ_BUILD_CHANNEL");
    println!("cargo:rerun-if-env-changed=MJ_AGENT_DEV_IMAGE");
    let release = std::env::var("MJ_BUILD_CHANNEL").as_deref() == Ok("release")
        || root.join(".cargo_vcs_info.json").exists();
    let version = std::env::var("CARGO_PKG_VERSION").unwrap();
    let image = std::env::var("MJ_AGENT_DEV_IMAGE").unwrap_or_else(|_| {
        if release {
            format!("{AGENT_DEV_IMAGE_REPOSITORY}:{version}")
        } else {
            format!("{AGENT_DEV_IMAGE_REPOSITORY}:latest")
        }
    });
    println!("cargo:rustc-env=MJ_AGENT_DEV_IMAGE={image}");
    println!(
        "cargo:rustc-env=MJ_AGENT_DEV_IMAGE_RELEASE={}",
        u8::from(release)
    );
    // Generate this fixture at build time: writing an executable during
    // parallel tests can leave inherited writable descriptors (ETXTBSY).
    // It carries the identity a test binary has (`worker_build`'s test
    // identity), not this build's: `mj-build-id` makes the one stamped
    // with the real identity.
    let fixture = root.join("tests/fixtures/fake-command.sh");
    let script = tracked_read(&fixture).expect("fake command fixture");
    let stamped = format!(
        "{script}\n#MJ-BUILD-ID:{version}+{TEST_REVISION}\n#\0MJ-WORKER-BUILD:{version}+{TEST_REVISION}\0\n",
    );
    let destination = PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("fake-worker.sh");
    std::fs::write(&destination, stamped).expect("write stamped fake worker");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(destination, std::fs::Permissions::from_mode(0o755))
            .expect("make fake worker executable");
    }
}
