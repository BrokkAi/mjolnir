//! Deterministic project-image recipes shared by the daemon and CLI output.

use std::collections::BTreeMap;

use anyhow::{Result, ensure};
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Files mise reads at the repository root: the one list both the worker's
/// post-clone step (which checkouts have a recipe) and the image generator
/// (which files an image is built from) use. `go.mod` is deliberately absent:
/// it changes with every dependency update, which would rebuild the image.
/// Ordering is fixed by the map.
pub const RECIPE_FILES: &[&str] = &[
    "mise.toml",
    ".mise.toml",
    "mise/config.toml",
    ".config/mise.toml",
    ".tool-versions",
    "mise.lock",
    "rust-toolchain",
    "rust-toolchain.toml",
    ".nvmrc",
    ".node-version",
    ".python-version",
    ".ruby-version",
    ".go-version",
];

/// Where a built image keeps the recipe files it was built from. The worker
/// compares a checkout's recipe with this copy to know the image ran the
/// recipe's postinstall hook.
pub const IMAGE_RECIPE_DIR: &str = "/opt/mj-image/recipe";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MiseArtifact {
    pub version: String,
    pub url: String,
    pub sha256: String,
}

/// The pinned static mise download for a Linux CPU architecture (Rust's
/// `std::env::consts::ARCH` spelling: `x86_64`, `aarch64`), read from
/// `assets/mise/runtime.json`. The worker installs it into containers and the
/// image generator adds it to built images, so both use this one reader.
pub fn pinned_mise(arch: &str) -> Result<MiseArtifact> {
    let manifest: serde_json::Value =
        serde_json::from_str(include_str!("../assets/mise/runtime.json"))?;
    let version = manifest["version"].as_str().unwrap_or_default();
    ensure!(
        version == mj_core::harness_runtime::MISE_VERSION,
        "mise download metadata does not match the managed runtime pin"
    );
    let platform = &manifest["platforms"][format!("linux-{arch}")];
    let field = |name: &str| {
        platform[name]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| anyhow::anyhow!("mise has no pinned {name} for linux-{arch}"))
    };
    Ok(MiseArtifact {
        version: version.to_owned(),
        url: field("url")?,
        sha256: field("sha256")?,
    })
}

#[derive(Debug, Clone)]
pub struct ImageBase {
    /// Platform-specific, immutable reference (repository@sha256:digest).
    pub reference: String,
    pub user: String,
    pub workdir: String,
}

pub fn recipe_hash(files: &BTreeMap<String, String>) -> Result<String> {
    Ok(mj_core::hex::lower_hex(Sha256::digest(serde_json::to_vec(
        files,
    )?)))
}

/// Directory of `exec` wrappers for every baked tool, after the mise shims on
/// `PATH`. A shim resolves versions from the working directory's mise
/// configuration and fails ("No version is set for shim") outside a checkout:
/// before the clone, in `/`, `/tmp`, or `/workspace/<id>`. Inside a checkout the
/// shims come first, so the checkout's own versions win; elsewhere the lookup
/// falls through to these wrappers, which run the version baked into the image.
/// The fall-through is mise's own: with `not_found_system_fallback = true` (the
/// default in the pinned mise 2026.11.9), a shim with no active project
/// `mise.toml` searches the remaining `PATH` entries after `/opt/mise/shims`. It
/// therefore reaches `/opt/mj-image/bin` before `/usr/bin`, and exits "No version
/// is set for shim" only when no later `PATH` entry provides the binary.
const IMAGE_FALLBACK_BIN_DIR: &str = "/opt/mj-image/bin";

/// Shell for the image build's install layer, run in the recipe directory right
/// after `mise install`. It writes one `exec` wrapper per executable in
/// `mise bin-paths`, earlier directories winning. Wrappers rather than symlinks
/// keep `argv[0]` and `$0`-relative lookups pointing at the real install
/// directory. It fails when `mise bin-paths` or a wrapper write fails.
fn fallback_wrappers_command(bin_dir: &str) -> String {
    format!(
        r##"rm -rf '{bin_dir}' && mkdir -p '{bin_dir}' && dirs=$(mise bin-paths) && printf '%s\n' "$dirs" | while IFS= read -r dir; do [ -d "$dir" ] || continue; for bin in "$dir"/*; do [ -f "$bin" ] && [ -x "$bin" ] || continue; name=${{bin##*/}}; [ -e '{bin_dir}'"/$name" ] && continue; escaped=$(printf '%s' "$bin" | sed "s/'/'\\\\''/g"); printf "#!/bin/sh\nexec '%s' \"\$@\"\n" "$escaped" > '{bin_dir}'"/$name" && chmod 0755 '{bin_dir}'"/$name" || exit 1; done; done"##
    )
}

/// The complete build input: no source checkout or build context is required.
pub fn containerfile(
    base: &ImageBase,
    platform: &str,
    mise: &MiseArtifact,
    files: &BTreeMap<String, String>,
) -> Result<String> {
    for value in [
        &base.reference,
        &base.user,
        &base.workdir,
        platform,
        &mise.url,
        &mise.version,
    ] {
        ensure!(
            !value.contains(['\r', '\n']),
            "image metadata contains a newline"
        );
    }
    ensure!(
        mise.sha256.len() == 64 && mise.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid mise SHA256"
    );
    ensure!(
        base.reference.contains("@sha256:"),
        "base image must be pinned by digest"
    );
    let recipe = recipe_hash(files)?;
    let mut text = format!(
        "FROM {}\nUSER root\nADD --checksum=sha256:{} {} /usr/local/bin/mise\nRUN chmod 0755 /usr/local/bin/mise\nENV MISE_DATA_DIR=/opt/mise \\\n    RUSTUP_HOME=/opt/rustup \\\n    CARGO_HOME=/opt/cargo \\\n    MISE_TRUSTED_CONFIG_PATHS={IMAGE_RECIPE_DIR} \\\n    MISE_IDIOMATIC_VERSION_FILE_ENABLE_TOOLS=rust,node,python,ruby,go \\\n    PATH=/opt/mise/shims:{IMAGE_FALLBACK_BIN_DIR}:$PATH\nWORKDIR {IMAGE_RECIPE_DIR}\n",
        base.reference, mise.sha256, mise.url
    );
    for (path, contents) in files {
        ensure!(
            RECIPE_FILES.contains(&path.as_str()),
            "unsupported recipe file {path:?}"
        );
        let parent = std::path::Path::new(path)
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(std::path::Path::new("."));
        // Linux limits each exec argument to 32 pages (MAX_ARG_STRLEN in
        // include/uapi/linux/binfmts.h). 48 KiB encodes to 64 KiB, leaving
        // room for the shell program even with 4 KiB pages. This chunks the
        // transfer rather than limiting the recipe's total size.
        let chunks: Vec<_> = contents.as_bytes().chunks(48 * 1024).collect();
        for (index, bytes) in chunks
            .iter()
            .copied()
            .chain(chunks.is_empty().then_some(&[][..]))
            .enumerate()
        {
            let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
            let redirect = if index == 0 { ">" } else { ">>" };
            text.push_str(&format!(
                "RUN mkdir -p '{}' && printf '%s' '{}' | base64 -d {redirect} '{}' && chmod 0644 '{}'\n",
                parent.display(), encoded, path, path
            ));
        }
    }
    // Sessions on a non-root base run as that user and install recipe changes
    // into these directories. Chown in the install layer: a later layer would
    // copy every installed file up again.
    let user = base.user.split(':').next().unwrap_or_default();
    let chown = if user.is_empty() || user == "root" || user == "0" {
        String::new()
    } else {
        ensure!(
            base.user
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._:-".contains(&byte)),
            "unsupported base image user {:?}",
            base.user
        );
        format!(
            " && mkdir -p /opt/mise /opt/rustup /opt/cargo && chown -R {} /opt/mise /opt/rustup /opt/cargo",
            base.user
        )
    };
    text.push_str(&format!(
        "RUN mise bootstrap packages apply --yes && mise install && {}{chown}\n",
        fallback_wrappers_command(IMAGE_FALLBACK_BIN_DIR)
    ));
    text.push_str(&format!("LABEL dev.mj.image.recipe=\"{recipe}\" \\\n    dev.mj.image.mise={} \\\n    dev.mj.image.platform={}\n", serde_json::to_string(&mise.version)?, serde_json::to_string(platform)?));
    text.push_str(&format!(
        "USER {}\nWORKDIR {}\n",
        if base.user.is_empty() {
            "root"
        } else {
            &base.user
        },
        serde_json::to_string(if base.workdir.is_empty() {
            "/"
        } else {
            &base.workdir
        })?
    ));
    Ok(text)
}

pub fn image_tag(containerfile: &str) -> String {
    mj_core::hex::lower_hex(Sha256::digest(containerfile.as_bytes()))[..16].to_owned()
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::process::{Command, Stdio};

    use super::*;

    fn executable(path: &Path, text: &str) {
        std::fs::write(path, text).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Runs the exact install-layer snippet against a fake `mise` whose
    /// `bin-paths` prints `mise_output` and exits with `status`.
    fn run_snippet(root: &Path, mise_output: &str, status: i32) -> std::process::Output {
        let fake = root.join("fake");
        std::fs::create_dir_all(&fake).unwrap();
        let script = format!(
            "#!/bin/sh\n[ \"$1\" = bin-paths ] || exit 64\ncat <<'EOF_PATHS'\n{mise_output}\nEOF_PATHS\nexit {status}\n"
        );
        executable(&fake.join("mise"), &script);
        let bin_dir = root.join("out bin");
        Command::new("/bin/sh")
            .arg("-c")
            .arg(fallback_wrappers_command(bin_dir.to_str().unwrap()))
            .env("PATH", format!("{}:/usr/bin:/bin", fake.display()))
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    #[test]
    fn fallback_wrappers_run_baked_tools_with_first_directory_winning() {
        let root = tempfile::tempdir().unwrap();
        // A space and a single quote exercise the quoting at both levels.
        let first = root.path().join("first dir");
        let second = root.path().join("it's second");
        let missing = root.path().join("missing");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        executable(
            &first.join("gh"),
            "#!/bin/sh\necho first-gh \"$0\" \"$@\"\n",
        );
        executable(&second.join("gh"), "#!/bin/sh\necho second-gh\n");
        executable(&second.join("node"), "#!/bin/sh\necho node \"$1\"\n");
        std::fs::write(second.join("data"), "not executable").unwrap();
        let listing = format!(
            "{}\n{}\n\n{}",
            first.display(),
            missing.display(),
            second.display()
        );

        // Building from an earlier session image leaves its wrappers behind;
        // they must neither win over the new recipe's tools nor survive it.
        let bin = root.path().join("out bin");
        std::fs::create_dir_all(&bin).unwrap();
        executable(&bin.join("gh"), "#!/bin/sh\necho stale-gh\n");
        executable(&bin.join("removed"), "#!/bin/sh\necho stale-removed\n");

        let output = run_snippet(root.path(), &listing, 0);
        assert!(output.status.success(), "{output:?}");
        assert!(!bin.join("data").exists());
        assert!(!bin.join("removed").exists());
        for name in ["gh", "node"] {
            let mode = std::fs::metadata(bin.join(name))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o755);
        }
        let run = |name: &str, args: &[&str]| {
            let out = Command::new(bin.join(name))
                .args(args)
                .stdin(Stdio::null())
                .output()
                .unwrap();
            assert!(out.status.success(), "{out:?}");
            String::from_utf8(out.stdout).unwrap()
        };
        assert_eq!(
            run("gh", &["a b", "c"]),
            format!("first-gh {} a b c\n", first.join("gh").display())
        );
        assert_eq!(run("node", &["x"]), "node x\n");
    }

    #[test]
    fn fallback_wrappers_accept_an_empty_tool_list() {
        let root = tempfile::tempdir().unwrap();
        let output = run_snippet(root.path(), "", 0);
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            std::fs::read_dir(root.path().join("out bin"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn fallback_wrappers_fail_when_mise_bin_paths_fails() {
        let root = tempfile::tempdir().unwrap();
        let output = run_snippet(root.path(), "", 3);
        assert!(!output.status.success(), "{output:?}");
    }

    #[test]
    fn containerfile_puts_fallback_wrappers_after_shims_and_before_chown() {
        let base = ImageBase {
            reference: format!("docker.io/library/ubuntu:noble@sha256:{}", "a".repeat(64)),
            user: "hel".into(),
            workdir: "/workspace".into(),
        };
        let mise = MiseArtifact {
            url: "https://example.invalid/mise".into(),
            version: "1".into(),
            sha256: "b".repeat(64),
        };
        let text = containerfile(&base, "linux/amd64", &mise, &BTreeMap::new()).unwrap();
        assert!(text.contains("PATH=/opt/mise/shims:/opt/mj-image/bin:$PATH\n"));
        let install = text
            .find("&& mise install && rm -rf '/opt/mj-image/bin' && mkdir -p '/opt/mj-image/bin'")
            .unwrap();
        assert!(install < text.find("&& chown -R hel").unwrap());
    }
}
