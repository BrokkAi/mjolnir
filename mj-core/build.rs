use std::path::{Path, PathBuf};

/// The registry repository every session image name is built from. Keep it
/// equal to `mj_core::config::CONTAINER_IMAGE_REPOSITORY`, which
/// `the_baked_default_image_agrees_with_the_channel_rule` checks.
const AGENT_DEV_IMAGE_REPOSITORY: &str = "ghcr.io/brokkai/mjolnir/agent-dev";

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

fn git_revision(root: &Path) -> Option<String> {
    let entry = root
        .ancestors()
        .map(|path| path.join(".git"))
        .find(|path| path.exists())?;
    let git = if entry.is_dir() {
        entry
    } else {
        let contents = tracked_read(&entry)?;
        entry
            .parent()?
            .join(contents.trim().strip_prefix("gitdir: ")?)
    };
    let head = tracked_read(&git.join("HEAD"))?;
    let Some(reference) = head.trim().strip_prefix("ref: ") else {
        return Some(head.trim().to_owned());
    };
    let common = tracked_read(&git.join("commondir"))
        .map(|path| git.join(path.trim()))
        .unwrap_or_else(|| git.clone());
    if let Some(revision) = tracked_read(&common.join(reference)) {
        return Some(revision.trim().to_owned());
    }
    // A packed ref can become a loose ref on the next commit. Watch the
    // existing refs directory, not a missing file that makes Cargo rebuild
    // on every invocation.
    println!("cargo:rerun-if-changed={}", common.join("refs").display());
    tracked_read(&common.join("packed-refs"))?
        .lines()
        .find_map(|line| {
            let (revision, name) = line.split_once(' ')?;
            (name == reference).then(|| revision.to_owned())
        })
}

/// The committer time of `revision`, in seconds since the Unix epoch.
///
/// Two builds of one release are ordered by the time of the commit they were
/// built from; a commit hash alone has no order. A source archive without Git
/// supplies it as `MJ_BUILD_COMMIT_TIME`, which is what `git log -1
/// --format=%ct` prints. Without either the time is unknown, never guessed:
/// the build still succeeds, and startup treats its order as unknown.
///
/// The revision is looked up by hash, so a registry crate unpacked inside an
/// unrelated checkout cannot pick up that checkout's time: the commit is
/// either there, with its own time, or the lookup fails. Cargo already reruns
/// this script whenever the revision changes, which is its only input.
fn commit_time(root: &Path, revision: &str) -> Option<i64> {
    if let Ok(time) = std::env::var("MJ_BUILD_COMMIT_TIME") {
        return Some(
            time.trim()
                .parse()
                .expect("MJ_BUILD_COMMIT_TIME must be the commit's Unix time in seconds"),
        );
    }
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["show", "-s", "--format=%ct"])
        .arg(format!("{revision}^{{commit}}"))
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()?.trim().parse().ok()
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=MJ_BUILD_REVISION");
    println!("cargo:rerun-if-env-changed=MJ_BUILD_COMMIT_TIME");
    let root = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let revision = std::env::var("MJ_BUILD_REVISION").ok().or_else(|| {
        // Registry installs must use the published crate's revision, even if
        // Cargo unpacked it inside another Git checkout.
        tracked_read(&root.join(".cargo_vcs_info.json")).map(|body| {
            let info: serde_json::Value = serde_json::from_str(&body)
                .expect("parse Cargo source revision metadata");
            info["git"]["sha1"].as_str().expect("Cargo source revision").to_owned()
        })
    }).or_else(|| git_revision(&root)).expect(
        "worker build identity needs Git or .cargo_vcs_info.json; for a source archive set MJ_BUILD_REVISION to its full Git commit",
    );
    assert!(
        revision.len() == 40 && revision.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "MJ_BUILD_REVISION must be a full 40-character Git commit"
    );
    println!(
        "cargo:rustc-env=MJ_BUILD_ID={}+{}",
        std::env::var("CARGO_PKG_VERSION").unwrap(),
        revision.to_ascii_lowercase()
    );
    // Empty when unknown: daemon startup then cannot order this build against
    // a different revision of the same release.
    println!(
        "cargo:rustc-env=MJ_BUILD_COMMIT_TIME={}",
        commit_time(&root, &revision).map_or_else(String::new, |time| time.to_string())
    );
    // Which container image a session uses when its target names none. A
    // release runs the immutable image published for its own version; a
    // development build runs the floating image published from master. The
    // release workflow sets MJ_BUILD_CHANNEL, and a crates.io source package
    // carries .cargo_vcs_info.json, so both count as releases.
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
    if std::env::var_os("CARGO_FEATURE_TEST_HOOKS").is_some() {
        // Generate this fixture at build time: writing an executable during
        // parallel tests can leave inherited writable descriptors (ETXTBSY).
        let fixture = root.join("tests/fixtures/fake-command.sh");
        let script = tracked_read(&fixture).expect("fake command fixture");
        let stamped = format!(
            "{script}\n#\0MJ-WORKER-BUILD:{}+{}\0\n",
            std::env::var("CARGO_PKG_VERSION").unwrap(),
            revision.to_ascii_lowercase()
        );
        let destination =
            PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("fake-worker.sh");
        std::fs::write(&destination, stamped).expect("write stamped fake worker");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(destination, std::fs::Permissions::from_mode(0o755))
                .expect("make fake worker executable");
        }
    }
}
