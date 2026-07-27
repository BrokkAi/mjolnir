//! Pinned Anvil pseudo-builtin discovery and background installation.

use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, OnceLock};

use anyhow::{Context, Result};

use crate::install::Progress;
use crate::registry::BinaryTarget;

pub const VERSION: &str = "0.24.0";

static CLI_OVERRIDE: OnceLock<PathBuf> = OnceLock::new();
static INSTALL_STATE: LazyLock<Mutex<InstallState>> =
    LazyLock::new(|| Mutex::new(InstallState::Idle));

#[derive(Debug, Clone)]
enum InstallState {
    Idle,
    Installing {
        total_bytes: Option<u64>,
        downloaded_bytes: u64,
        extracting: bool,
    },
    Ready(PathBuf),
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct Detection {
    pub path: Option<PathBuf>,
    pub evidence: String,
    pub installing: bool,
    pub error: Option<String>,
}

pub fn configure_cli_override(path: Option<PathBuf>) {
    if let Some(path) = path {
        let _ = CLI_OVERRIDE.set(path);
    }
}

pub fn detect() -> Detection {
    if let Some(path) = CLI_OVERRIDE.get() {
        return override_detection(path, "--anvil-path");
    }
    if let Some(path) = std::env::var_os("MJ_ANVIL_PATH").map(PathBuf::from) {
        return override_detection(&path, "MJ_ANVIL_PATH");
    }
    if let Some(path) = sibling_path().filter(|path| path.is_file()) {
        return Detection {
            evidence: format!("bundled sibling {}", path.display()),
            path: Some(path),
            installing: false,
            error: None,
        };
    }
    if let Some(path) = managed_path().filter(|path| path.is_file()) {
        return Detection {
            evidence: format!("managed Anvil {VERSION}"),
            path: Some(path),
            installing: false,
            error: None,
        };
    }
    let state = INSTALL_STATE
        .lock()
        .map(|state| state.clone())
        .unwrap_or_else(|_| {
            InstallState::Failed("Anvil installer state is unavailable".to_string())
        });
    match state {
        InstallState::Idle => Detection {
            path: None,
            evidence: format!("managed Anvil {VERSION} is not installed"),
            installing: false,
            error: None,
        },
        InstallState::Installing {
            total_bytes,
            downloaded_bytes,
            extracting,
        } => {
            let progress = if extracting {
                "extracting".to_string()
            } else if let Some(total) = total_bytes {
                format!(
                    "downloading {}%",
                    downloaded_bytes.saturating_mul(100) / total.max(1)
                )
            } else if downloaded_bytes > 0 {
                format!("downloading {downloaded_bytes} bytes")
            } else {
                "downloading".to_string()
            };
            Detection {
                path: None,
                evidence: format!("managed Anvil {VERSION}: {progress}"),
                installing: true,
                error: None,
            }
        }
        InstallState::Ready(path) => Detection {
            evidence: format!("managed Anvil {VERSION}"),
            path: Some(path),
            installing: false,
            error: None,
        },
        InstallState::Failed(error) => Detection {
            path: None,
            evidence: format!("managed Anvil {VERSION} install failed"),
            installing: false,
            error: Some(error),
        },
    }
}

fn override_detection(path: &Path, source: &str) -> Detection {
    if path.is_file() {
        Detection {
            evidence: format!("{source}: {}", path.display()),
            path: Some(path.to_path_buf()),
            installing: false,
            error: None,
        }
    } else {
        Detection {
            path: None,
            evidence: format!("{source}: {}", path.display()),
            installing: false,
            error: Some("configured Anvil override does not exist".to_string()),
        }
    }
}

pub fn start_background_install() {
    let detection = detect();
    if detection.path.is_some() || detection.error.is_some() || detection.installing {
        return;
    }
    let Some(target) = release_target() else {
        if let Ok(mut state) = INSTALL_STATE.lock() {
            *state = InstallState::Failed("no pinned Anvil asset for this platform".to_string());
        }
        return;
    };
    if let Ok(mut state) = INSTALL_STATE.lock() {
        if !matches!(*state, InstallState::Idle | InstallState::Failed(_)) {
            return;
        }
        *state = InstallState::Installing {
            total_bytes: None,
            downloaded_bytes: 0,
            extracting: false,
        };
    }
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        if let Ok(mut state) = INSTALL_STATE.lock() {
            *state = InstallState::Failed(
                "Anvil background installation requires an async runtime".to_string(),
            );
        }
        return;
    };
    runtime.spawn(async move {
        let result = install_target(target).await;
        if let Ok(mut state) = INSTALL_STATE.lock() {
            *state = match result {
                Ok(path) => InstallState::Ready(path),
                Err(error) => InstallState::Failed(format!("{error:#}")),
            };
        }
    });
}

pub fn retry_background_install() {
    if let Ok(mut state) = INSTALL_STATE.lock()
        && matches!(*state, InstallState::Failed(_))
    {
        *state = InstallState::Idle;
    }
    start_background_install();
}

pub async fn wait_until_ready() -> Result<PathBuf> {
    start_background_install();
    loop {
        let detection = detect();
        if let Some(path) = detection.path {
            return Ok(path);
        }
        if let Some(error) = detection.error {
            anyhow::bail!(error);
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

async fn install_target(mut target: BinaryTarget) -> Result<PathBuf> {
    target.sha256 = fetch_checksum(&format!("{}.sha256", target.archive)).await?;
    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
    let install = crate::install::install_or_resolve("anvil", VERSION, &target, progress_tx);
    tokio::pin!(install);
    loop {
        tokio::select! {
            result = &mut install => return result.map(|(path, _)| path),
            progress = progress_rx.recv() => {
                let Some(progress) = progress else { continue };
                if let Ok(mut state) = INSTALL_STATE.lock()
                    && let InstallState::Installing {
                        total_bytes,
                        downloaded_bytes,
                        extracting,
                    } = &mut *state
                {
                    match progress {
                        Progress::Started { total_bytes: total } => *total_bytes = total,
                        Progress::Downloaded { downloaded_bytes: downloaded } => {
                            *downloaded_bytes = downloaded;
                        }
                        Progress::Extracting => *extracting = true,
                        Progress::Done => {}
                    }
                }
            }
        }
    }
}

async fn fetch_checksum(url: &str) -> Result<String> {
    let response = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .user_agent(concat!("mj/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("build Anvil checksum client")?
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("GET {url}"))?;
    let body = response.text().await.context("read Anvil checksum")?;
    let checksum = body
        .split_whitespace()
        .next()
        .context("empty Anvil checksum")?;
    anyhow::ensure!(
        checksum.len() == 64 && checksum.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid Anvil checksum"
    );
    Ok(checksum.to_ascii_lowercase())
}

fn release_target() -> Option<BinaryTarget> {
    let target = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        ("android", "aarch64") => "aarch64-linux-android",
        ("macos", "x86_64" | "aarch64" | "arm64") => "universal-apple-darwin",
        ("windows", "x86_64") => "x86_64-pc-windows-msvc",
        _ => return None,
    };
    let archive_name = format!("brokk-anvil-v{VERSION}-{target}.zip");
    let executable = if cfg!(windows) { "anvil.exe" } else { "anvil" };
    Some(BinaryTarget {
        archive: format!(
            "https://github.com/BrokkAi/anvil/releases/download/v{VERSION}/{archive_name}"
        ),
        sha256: String::new(),
        cmd: format!("./brokk-anvil-v{VERSION}-{target}/{executable}"),
        args: Vec::new(),
        env: Default::default(),
    })
}

fn sibling_path() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()?
        .parent()
        .map(|parent| parent.join(if cfg!(windows) { "anvil.exe" } else { "anvil" }))
}

pub fn managed_path() -> Option<PathBuf> {
    let target = release_target()?;
    Some(
        crate::install::default_install_root()
            .join("anvil")
            .join(VERSION)
            .join(target.cmd.strip_prefix("./").unwrap_or(&target.cmd)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_reports_missing_path_without_falling_back() {
        let detection = override_detection(Path::new("/definitely/missing/anvil"), "test");
        assert!(detection.path.is_none());
        assert!(detection.error.is_some());
        assert!(detection.evidence.contains("test"));
    }

    #[test]
    fn pinned_target_uses_the_anvil_release() {
        if let Some(target) = release_target() {
            assert!(target.archive.contains("/anvil/releases/download/v0.24.0/"));
            assert!(target.cmd.contains("brokk-anvil-v0.24.0"));
        }
    }
}
