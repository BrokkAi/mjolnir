//! Single owner for embedded runtime download manifests.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
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
    validate_sha256(&field("sha256")?)?;
    Ok(MiseArtifact {
        version: version.to_owned(),
        url: field("url")?,
        sha256: field("sha256")?,
    })
}

pub fn manifest(kind: &str) -> Result<serde_json::Value> {
    use mj_core::harness_runtime::*;
    let (text, version, field) = match kind {
        "node" => (
            include_str!("../assets/node/runtime.json"),
            NODE_VERSION,
            "version",
        ),
        "kimi" => (
            include_str!("../assets/kimi/runtime.json"),
            KIMI_VERSION,
            "version",
        ),
        "grok" => (
            include_str!("../assets/grok/runtime.json"),
            GROK_VERSION,
            "version",
        ),
        "muse" => (
            include_str!("../assets/muse/runtime.json"),
            MUSE_VERSION,
            "muse_version",
        ),
        "opencode" => (
            include_str!("../assets/opencode/runtime.json"),
            OPENCODE_VERSION,
            "version",
        ),
        "antigravity" => (
            include_str!("../assets/antigravity/runtime.json"),
            ANTIGRAVITY_VERSION,
            "version",
        ),
        _ => anyhow::bail!("unknown runtime manifest {kind}"),
    };
    let value: serde_json::Value = serde_json::from_str(text)?;
    ensure!(
        value[field] == version,
        "{kind} manifest version does not match pin"
    );
    for platform in value["platforms"]
        .as_object()
        .into_iter()
        .flat_map(|p| p.values())
    {
        for (key, digest) in platform.as_object().into_iter().flatten() {
            if key.ends_with("sha256") {
                validate_sha256(digest.as_str().unwrap_or_default())?;
            }
        }
    }
    Ok(value)
}

pub fn validate_sha256(value: &str) -> Result<()> {
    ensure!(
        value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid runtime SHA256"
    );
    Ok(())
}

pub fn platform_arch(platform: &str) -> Result<&'static str> {
    match platform {
        "linux/amd64" => Ok("x86_64"),
        "linux/arm64" => Ok("aarch64"),
        _ => anyhow::bail!("unsupported image platform {platform}"),
    }
}

pub struct RuntimeArtifact {
    pub url: String,
    pub sha256: String,
    pub target: String,
    pub filename: String,
    pub interpreter: Option<String>,
}
pub fn artifact(kind: &str, key: &str) -> Result<RuntimeArtifact> {
    artifact_from_metadata(kind, &manifest(kind)?, key)
}

pub fn artifact_from_metadata(
    kind: &str,
    metadata: &serde_json::Value,
    key: &str,
) -> Result<RuntimeArtifact> {
    use mj_core::harness_runtime::*;
    let p = &metadata["platforms"][key];
    let field = |name: &str| -> Result<String> {
        Ok(p[name]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing {kind} {key} {name}"))?
            .to_owned())
    };
    let target = match kind {
        "kimi" => field("filename")?,
        "muse" => field("muse_target")?,
        "antigravity" => String::new(),
        _ => field("target")?,
    };
    let filename = match kind {
        "node" => format!("node-v{NODE_VERSION}-{target}.tar.gz"),
        "kimi" => target.clone(),
        "grok" => format!("grok-{GROK_VERSION}-{target}.zst"),
        "muse" => format!("muse-{target}"),
        "opencode" => format!("cli-{target}-{OPENCODE_VERSION}.tgz"),
        "antigravity" => field("url")?
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .to_owned(),
        _ => anyhow::bail!("unknown runtime {kind}"),
    };
    let url = match kind {
        "node" => {
            format!("{NODE_DIST_URL}/v{NODE_VERSION}/{filename}")
        }
        "kimi" => format!("{KIMI_PRIMARY_BASE}/{KIMI_VERSION}/{filename}"),
        "grok" => format!(
            "https://storage.googleapis.com/grok-build-public-artifacts/cli/grok-{GROK_VERSION}-{target}.zst"
        ),
        "muse" => format!(
            "https://lookaside.facebook.com/lookaside/muse/download/?channel=muse&version={MUSE_VERSION}&file=muse-{target}"
        ),
        "opencode" => format!(
            "https://registry.npmjs.org/@opencode/cli-{target}/-/cli-{target}-{OPENCODE_VERSION}.tgz"
        ),
        "antigravity" => field("url")?,
        _ => anyhow::bail!("unknown runtime {kind}"),
    };
    let sha256 = field(if kind == "muse" {
        "muse_sha256"
    } else {
        "sha256"
    })?;
    validate_sha256(&sha256)?;
    Ok(RuntimeArtifact {
        url,
        target,
        filename,
        sha256,
        interpreter: p["interpreter"].as_str().map(str::to_owned),
    })
}

pub const NODE_DIST_URL: &str = "https://nodejs.org/dist";
pub const KIMI_PRIMARY_BASE: &str = "https://code.kimi.com/kimi-code/binaries";
pub const GROK_PRIMARY_BASE: &str = "https://x.ai/cli";
pub fn kimi_fallback_url(filename: &str) -> String {
    format!(
        "https://code.kimi.ai/kimi-code/binaries/{}/{filename}",
        mj_core::harness_runtime::KIMI_VERSION
    )
}
