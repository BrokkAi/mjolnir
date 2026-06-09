//! ACP registry: discover available agents from the canonical
//! `agentclientprotocol/registry` index.
//!
//! Fetches `https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json`,
//! caches the result at `~/.cache/mj/registry.json`, and refreshes when the
//! cached copy is older than `Self::CACHE_TTL` (24h). Offline-friendly: a
//! cached copy is used even when fetch fails, so the picker still works.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use serde::Deserialize;

pub const REGISTRY_URL: &str =
    "https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json";

/// How long a cached registry copy is considered fresh.
pub const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// One entry in the registry. Mirrors the JSON shape but flattens away
/// fields we don't consume so we stay forward-compatible.
#[derive(Debug, Clone, Deserialize)]
pub struct Agent {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub distribution: Distribution,
}

/// Distribution channels for one agent. Multiple may be present (e.g.
/// codex-acp ships both a binary and an npx package). The picker chooses
/// the best one for the host at launch time.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Distribution {
    #[serde(default)]
    pub npx: Option<NpxPackage>,
    #[serde(default)]
    pub uvx: Option<UvxPackage>,
    #[serde(default)]
    pub binary: Option<HashMap<String, BinaryTarget>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NpxPackage {
    pub package: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UvxPackage {
    pub package: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BinaryTarget {
    /// URL to a tar.gz (Unix-like) or zip (Windows) archive.
    pub archive: String,
    /// Path to the executable inside the extracted archive, e.g. `./codex-acp`.
    pub cmd: String,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum DistributionKind {
    Binary,
    Npx,
    Uvx,
}

impl DistributionKind {
    /// Short label for picker hints (`"binary"`, `"npx"`, `"uvx"`).
    pub fn label(self) -> &'static str {
        match self {
            DistributionKind::Binary => "binary",
            DistributionKind::Npx => "npx",
            DistributionKind::Uvx => "uvx",
        }
    }
}

impl Agent {
    /// Return the distribution kinds available for the current platform,
    /// ordered by preference: package-manager launchers first, then native
    /// binaries. Empty when none of them works here.
    pub fn supported_kinds(&self, platform: &str) -> Vec<DistributionKind> {
        let mut out = Vec::new();
        if self.distribution.npx.is_some() {
            out.push(DistributionKind::Npx);
        }
        if self.distribution.uvx.is_some() {
            out.push(DistributionKind::Uvx);
        }
        if let Some(map) = &self.distribution.binary
            && map.contains_key(platform)
        {
            out.push(DistributionKind::Binary);
        }
        out
    }

    pub fn preferred_kind(&self, platform: &str) -> Option<DistributionKind> {
        self.supported_kinds(platform).into_iter().next()
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Registry {
    #[serde(default)]
    pub agents: Vec<Agent>,
}

impl Registry {
    /// Parse a registry JSON document.
    pub fn from_json(s: &str) -> Result<Self> {
        serde_json::from_str(s).context("parse registry json")
    }

    /// Find an agent by `id`. Used in tests; the bin walks `agents`
    /// directly via indexed access from the picker.
    #[cfg(test)]
    pub fn get(&self, id: &str) -> Option<&Agent> {
        self.agents.iter().find(|a| a.id == id)
    }
}

/// Detect the current platform string in the registry's convention,
/// e.g. `darwin-aarch64`, `linux-x86_64`, `windows-x86_64`.
pub fn current_platform() -> String {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        "arm64" => "aarch64",
        other => other,
    };
    format!("{os}-{arch}")
}

/// Default on-disk cache location: `$XDG_CACHE_HOME/mj/registry.json`
/// (or `~/.cache/mj/registry.json` when `XDG_CACHE_HOME` is unset).
pub fn default_cache_path() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from(".cache"))
        .join("mj")
        .join("registry.json")
}

/// Load the registry, refreshing from `url` when the cache is older than
/// `ttl` or missing. On network failure with a usable cache present,
/// returns the cached copy and emits a `tracing::warn!`. Returns an error
/// only when both fetch and cache are unavailable.
pub async fn load_with_cache(cache_path: &Path, ttl: Duration, url: &str) -> Result<Registry> {
    let cache_is_fresh = match cache_path.metadata() {
        Ok(meta) => match meta.modified() {
            Ok(mtime) => SystemTime::now()
                .duration_since(mtime)
                .map(|age| age < ttl)
                .unwrap_or(false),
            Err(_) => false,
        },
        Err(_) => false,
    };

    if cache_is_fresh {
        match std::fs::read_to_string(cache_path) {
            Ok(s) => match Registry::from_json(&s) {
                Ok(r) => return Ok(r),
                Err(e) => tracing::warn!("cached registry unparseable, refetching: {e:#}"),
            },
            Err(e) => tracing::warn!("read cached registry: {e:#}"),
        }
    }

    match fetch_fresh(url).await {
        Ok(json) => {
            if let Some(parent) = cache_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Err(e) = std::fs::write(cache_path, &json) {
                tracing::warn!("write registry cache {cache_path:?}: {e:#}");
            }
            Registry::from_json(&json)
        }
        Err(fetch_err) => match std::fs::read_to_string(cache_path) {
            Ok(s) => {
                tracing::warn!(
                    "registry fetch failed ({fetch_err:#}); using cached copy at {cache_path:?}"
                );
                Registry::from_json(&s)
            }
            Err(_) => Err(fetch_err.context("no cached registry available")),
        },
    }
}

/// Fetch the registry JSON over HTTPS. Separated so tests can stub it.
pub(crate) async fn fetch_fresh(url: &str) -> Result<String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent(concat!("mj/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("build http client")?;
    let resp = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("GET {url}: HTTP {status}");
    }
    resp.text().await.context("read registry body")
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{
        "version": "1.0.0",
        "agents": [
            {
                "id": "claude-acp",
                "name": "Claude",
                "version": "0.36.1",
                "description": "Claude ACP agent",
                "distribution": {
                    "npx": { "package": "@agentclientprotocol/claude-agent-acp@0.36.1" }
                }
            },
            {
                "id": "codex-acp",
                "name": "Codex",
                "version": "0.14.0",
                "description": "Zed Codex ACP",
                "distribution": {
                    "binary": {
                        "darwin-aarch64": {
                            "archive": "https://example.com/codex-arm64.tar.gz",
                            "cmd": "./codex-acp"
                        },
                        "linux-x86_64": {
                            "archive": "https://example.com/codex-x64.tar.gz",
                            "cmd": "./codex-acp"
                        }
                    },
                    "npx": { "package": "@zed-industries/codex-acp@0.14.0" }
                }
            },
            {
                "id": "binary-only",
                "name": "Binary Only",
                "version": "1.0.0",
                "description": "no npx fallback",
                "distribution": {
                    "binary": {
                        "darwin-aarch64": {
                            "archive": "https://example.com/bin.tar.gz",
                            "cmd": "./bin",
                            "args": ["--flag"]
                        }
                    }
                }
            }
        ]
    }"#;

    #[test]
    fn parse_fixture() {
        let reg = Registry::from_json(FIXTURE).expect("parse");
        assert_eq!(reg.agents.len(), 3);
        let claude = reg.get("claude-acp").expect("claude");
        assert!(claude.distribution.npx.is_some());
        assert!(claude.distribution.binary.is_none());
    }

    #[test]
    fn parse_real_registry_snapshot() {
        // Sanity-check that real fields (mostly unknown to us) don't trip
        // up the parser. We just need the known fields to be extracted.
        let real = r#"{
            "version": "1.0.0",
            "agents": [{
                "id": "noisy",
                "name": "Noisy",
                "version": "1.2.3",
                "description": "has unknown fields",
                "repository": "https://example.com",
                "authors": ["someone"],
                "license": "MIT",
                "icon": "https://example.com/icon.svg",
                "distribution": {
                    "uvx": {
                        "package": "noisy@1.2.3",
                        "args": ["--acp"],
                        "env": {"NOISY_QUIET": "1"}
                    }
                },
                "unknown_field": "ignored"
            }]
        }"#;
        let reg = Registry::from_json(real).expect("parse real");
        let a = reg.get("noisy").expect("noisy");
        let uvx = a.distribution.uvx.as_ref().expect("uvx");
        assert_eq!(uvx.package, "noisy@1.2.3");
        assert_eq!(uvx.args, vec!["--acp"]);
        assert_eq!(uvx.env.get("NOISY_QUIET"), Some(&"1".to_string()));
    }

    #[test]
    fn supported_kinds_prefers_npx_when_platform_binary_also_matches() {
        let reg = Registry::from_json(FIXTURE).expect("parse");
        let codex = reg.get("codex-acp").expect("codex");
        let kinds = codex.supported_kinds("darwin-aarch64");
        assert_eq!(kinds, vec![DistributionKind::Npx, DistributionKind::Binary]);
        assert_eq!(
            codex.preferred_kind("darwin-aarch64"),
            Some(DistributionKind::Npx)
        );
    }

    #[test]
    fn supported_kinds_prefers_uvx_when_platform_binary_also_matches() {
        let json = r#"{
            "agents": [{
                "id": "uvx-binary",
                "name": "uvx binary",
                "version": "1.0.0",
                "distribution": {
                    "binary": {
                        "darwin-aarch64": {
                            "archive": "https://example.com/bin.tar.gz",
                            "cmd": "./bin"
                        }
                    },
                    "uvx": { "package": "uvx-binary==1.0.0" }
                }
            }]
        }"#;
        let reg = Registry::from_json(json).expect("parse");
        let agent = reg.get("uvx-binary").expect("uvx-binary");
        let kinds = agent.supported_kinds("darwin-aarch64");
        assert_eq!(kinds, vec![DistributionKind::Uvx, DistributionKind::Binary]);
        assert_eq!(
            agent.preferred_kind("darwin-aarch64"),
            Some(DistributionKind::Uvx)
        );
    }

    #[test]
    fn supported_kinds_skips_binary_for_unsupported_platform() {
        let reg = Registry::from_json(FIXTURE).expect("parse");
        let codex = reg.get("codex-acp").expect("codex");
        let kinds = codex.supported_kinds("windows-aarch64");
        assert_eq!(kinds, vec![DistributionKind::Npx]);
        assert_eq!(
            codex.preferred_kind("windows-aarch64"),
            Some(DistributionKind::Npx)
        );
    }

    #[test]
    fn supported_kinds_empty_when_binary_only_misses_platform() {
        let reg = Registry::from_json(FIXTURE).expect("parse");
        let bin_only = reg.get("binary-only").expect("binary-only");
        let kinds = bin_only.supported_kinds("linux-x86_64");
        assert!(kinds.is_empty());
        assert_eq!(bin_only.preferred_kind("linux-x86_64"), None);
    }

    #[test]
    fn current_platform_returns_expected_shape() {
        let p = current_platform();
        assert!(
            p.contains('-'),
            "platform should look like 'os-arch', got {p}"
        );
        assert!(
            p.starts_with("darwin-")
                || p.starts_with("linux-")
                || p.starts_with("windows-")
                || p.starts_with("android-"),
            "unexpected os prefix in {p}"
        );
    }

    #[tokio::test]
    async fn load_with_cache_uses_fresh_cache_without_network() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("registry.json");
        std::fs::write(&path, FIXTURE).expect("write cache");

        // Bogus URL — cache is fresh so we should never hit the network.
        let reg = load_with_cache(&path, Duration::from_secs(3600), "http://127.0.0.1:1/")
            .await
            .expect("load");
        assert_eq!(reg.agents.len(), 3);
    }

    #[tokio::test]
    async fn load_with_cache_falls_back_to_stale_cache_when_offline() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("registry.json");
        std::fs::write(&path, FIXTURE).expect("write cache");

        // Backdate the cache so it counts as stale, then force a fetch
        // against a port that nothing is listening on. The fallback
        // should still produce a parsed registry from the stale file.
        let old = SystemTime::now() - Duration::from_secs(48 * 3600);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("open")
            .set_modified(old)
            .expect("set_modified");

        let reg = load_with_cache(&path, Duration::from_secs(3600), "http://127.0.0.1:1/")
            .await
            .expect("falls back to stale cache");
        assert_eq!(reg.agents.len(), 3);
    }

    #[tokio::test]
    async fn load_with_cache_errors_when_no_cache_and_fetch_fails() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("registry.json");
        // No file written.
        let result = load_with_cache(&path, Duration::from_secs(3600), "http://127.0.0.1:1/").await;
        assert!(result.is_err(), "expected error, got {result:?}");
    }
}
