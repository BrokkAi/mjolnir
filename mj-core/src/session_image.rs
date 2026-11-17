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
        version == crate::harness_runtime::MISE_VERSION,
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
    Ok(crate::hex::lower_hex(Sha256::digest(serde_json::to_vec(
        files,
    )?)))
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
        "FROM {}\nUSER root\nADD --checksum=sha256:{} {} /usr/local/bin/mise\nRUN chmod 0755 /usr/local/bin/mise\nENV MISE_DATA_DIR=/opt/mise \\\n    RUSTUP_HOME=/opt/rustup \\\n    CARGO_HOME=/opt/cargo \\\n    MISE_TRUSTED_CONFIG_PATHS=/opt/mj-image/recipe \\\n    MISE_IDIOMATIC_VERSION_FILE_ENABLE_TOOLS=rust,node,python,ruby,go \\\n    PATH=/opt/mise/shims:$PATH\nWORKDIR /opt/mj-image/recipe\n",
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
        "RUN mise bootstrap packages apply --yes && mise install{chown}\n"
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
    crate::hex::lower_hex(Sha256::digest(containerfile.as_bytes()))[..16].to_owned()
}
