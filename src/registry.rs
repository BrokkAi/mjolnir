//! Cached client for the canonical Agent Client Protocol registry.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use serde::Deserialize;

pub const REGISTRY_URL: &str =
    "https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json";
pub const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Registry {
    #[serde(default)]
    pub agents: Vec<Agent>,
}

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

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Distribution {
    #[serde(default)]
    pub npx: Option<Package>,
    #[serde(default)]
    pub uvx: Option<Package>,
    #[serde(default)]
    pub binary: Option<HashMap<String, BinaryTarget>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Package {
    pub package: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BinaryTarget {
    pub archive: String,
    #[serde(default)]
    pub sha256: String,
    pub cmd: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DistributionKind {
    Binary,
    Npx,
    Uvx,
}

impl DistributionKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Binary => "binary",
            Self::Npx => "npx",
            Self::Uvx => "uvx",
        }
    }
}

impl Agent {
    pub fn preferred_kind(&self, platform: &str) -> Option<DistributionKind> {
        if self
            .distribution
            .binary
            .as_ref()
            .is_some_and(|targets| targets.contains_key(platform))
        {
            Some(DistributionKind::Binary)
        } else if self.distribution.npx.is_some() {
            Some(DistributionKind::Npx)
        } else if self.distribution.uvx.is_some() {
            Some(DistributionKind::Uvx)
        } else {
            None
        }
    }
}

impl Registry {
    pub fn from_json(json: &str) -> Result<Self> {
        serde_json::from_str(json).context("parse ACP registry")
    }
}

pub fn current_platform() -> String {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "arm64" => "aarch64",
        other => other,
    };
    format!("{os}-{arch}")
}

pub fn default_cache_path() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from(".cache"))
        .join("mj")
        .join("registry.json")
}

pub async fn load() -> Result<Registry> {
    load_with_cache(&default_cache_path(), CACHE_TTL, REGISTRY_URL).await
}

async fn load_with_cache(cache_path: &Path, ttl: Duration, url: &str) -> Result<Registry> {
    load_with_cache_using(cache_path, ttl, || fetch(url)).await
}

async fn load_with_cache_using<F, Fut>(
    cache_path: &Path,
    ttl: Duration,
    fetch: F,
) -> Result<Registry>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<String>>,
{
    let fresh = cache_path
        .metadata()
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age < ttl);
    if fresh
        && let Ok(contents) = std::fs::read_to_string(cache_path)
        && let Ok(registry) = Registry::from_json(&contents)
    {
        return Ok(registry);
    }

    match fetch().await {
        Ok(contents) => {
            if let Some(parent) = cache_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Err(error) = std::fs::write(cache_path, &contents) {
                tracing::warn!(path = %cache_path.display(), "write ACP registry cache: {error}");
            }
            Registry::from_json(&contents)
        }
        Err(fetch_error) => {
            let contents = std::fs::read_to_string(cache_path).with_context(|| {
                format!("fetch ACP registry: {fetch_error:#}; no cached registry available")
            })?;
            tracing::warn!("ACP registry refresh failed; using cached copy: {fetch_error:#}");
            Registry::from_json(&contents)
        }
    }
}

async fn fetch(url: &str) -> Result<String> {
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent(concat!("mj/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("build ACP registry client")?
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("GET {url}"))?;
    response.text().await.context("read ACP registry")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    const FIXTURE: &str = r#"{
        "agents": [{
            "id": "codex-acp", "name": "Codex", "version": "1.0.0",
            "distribution": {
                "binary": {"linux-x86_64": {"archive": "https://example/a.tgz", "sha256": "abcd", "cmd": "./codex-acp"}},
                "npx": {"package": "@agentclientprotocol/codex-acp"}
            }
        }]
    }"#;

    #[test]
    fn parses_registry_and_prefers_platform_binary() {
        let registry = Registry::from_json(FIXTURE).expect("registry");
        assert_eq!(registry.agents[0].name, "Codex");
        assert_eq!(
            registry.agents[0]
                .distribution
                .binary
                .as_ref()
                .and_then(|targets| targets.get("linux-x86_64"))
                .map(|target| target.sha256.as_str()),
            Some("abcd")
        );
        assert_eq!(
            registry.agents[0].preferred_kind("linux-x86_64"),
            Some(DistributionKind::Binary)
        );
        assert_eq!(
            registry.agents[0].preferred_kind("darwin-aarch64"),
            Some(DistributionKind::Npx)
        );
    }

    #[test]
    fn distribution_falls_back_to_uvx_or_none_and_labels_are_stable() {
        let mut agent = Registry::from_json(FIXTURE).unwrap().agents.remove(0);
        agent.distribution.binary = None;
        agent.distribution.npx = None;
        agent.distribution.uvx = Some(Package {
            package: "codex-acp".to_string(),
            args: Vec::new(),
            env: HashMap::new(),
        });
        assert_eq!(
            agent.preferred_kind("linux-x86_64"),
            Some(DistributionKind::Uvx)
        );
        agent.distribution.uvx = None;
        assert_eq!(agent.preferred_kind("linux-x86_64"), None);
        assert_eq!(DistributionKind::Binary.label(), "binary");
        assert_eq!(DistributionKind::Npx.label(), "npx");
        assert_eq!(DistributionKind::Uvx.label(), "uvx");
    }

    #[test]
    fn invalid_registry_json_has_parse_context() {
        let error = Registry::from_json("not json").expect_err("invalid registry");
        assert!(error.to_string().contains("parse ACP registry"), "{error}");
    }

    #[test]
    fn platform_name_uses_registry_conventions() {
        let expected_os = if std::env::consts::OS == "macos" {
            "darwin"
        } else {
            std::env::consts::OS
        };
        let expected_arch = if std::env::consts::ARCH == "arm64" {
            "aarch64"
        } else {
            std::env::consts::ARCH
        };
        assert_eq!(current_platform(), format!("{expected_os}-{expected_arch}"));
    }

    #[tokio::test]
    async fn fresh_valid_cache_skips_fetch() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("registry.json");
        std::fs::write(&cache, FIXTURE).unwrap();
        let called = AtomicBool::new(false);

        let registry = load_with_cache_using(&cache, Duration::from_secs(60), || async {
            called.store(true, Ordering::Relaxed);
            anyhow::bail!("fresh cache unexpectedly fetched")
        })
        .await
        .unwrap();

        assert_eq!(registry.agents[0].id, "codex-acp");
        assert!(!called.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn invalid_fresh_cache_is_replaced_by_fetch() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("registry.json");
        std::fs::write(&cache, "not json").unwrap();

        let registry = load_with_cache_using(&cache, Duration::from_secs(60), || async {
            Ok(FIXTURE.to_string())
        })
        .await
        .unwrap();

        assert_eq!(registry.agents[0].id, "codex-acp");
        assert_eq!(std::fs::read_to_string(cache).unwrap(), FIXTURE);
    }

    #[tokio::test]
    async fn successful_fetch_creates_cache_parent() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("nested/cache/registry.json");

        let registry =
            load_with_cache_using(&cache, Duration::ZERO, || async { Ok(FIXTURE.to_string()) })
                .await
                .unwrap();

        assert_eq!(registry.agents[0].name, "Codex");
        assert_eq!(std::fs::read_to_string(cache).unwrap(), FIXTURE);
    }

    #[tokio::test]
    async fn failed_refresh_falls_back_to_stale_cache() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("registry.json");
        std::fs::write(&cache, FIXTURE).unwrap();

        let registry = load_with_cache_using(&cache, Duration::ZERO, || async {
            anyhow::bail!("network unavailable")
        })
        .await
        .unwrap();

        assert_eq!(registry.agents[0].id, "codex-acp");
    }

    #[tokio::test]
    async fn failed_refresh_without_cache_reports_both_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("missing.json");

        let error = load_with_cache_using(&cache, Duration::ZERO, || async {
            anyhow::bail!("network unavailable")
        })
        .await
        .expect_err("missing cache");
        let message = format!("{error:#}");
        assert!(message.contains("network unavailable"), "{message}");
        assert!(
            message.contains("no cached registry available"),
            "{message}"
        );
    }
}
