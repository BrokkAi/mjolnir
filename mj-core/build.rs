use std::path::{Path, PathBuf};

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

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=MJ_BUILD_REVISION");
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
