//! Install-aware startup update checks for `mj`.
//!
//! Mjolnir reaches users through several channels — the curl release
//! installer, the npm package, the Homebrew tap, and crates.io — and each
//! channel publishes its "latest" somewhere else. This module decides how
//! the running binary was installed, asks that channel whether a newer
//! release exists, and (in [`check_prompt_and_apply`]) asks the
//! user before changing anything. npm and Homebrew upgrades delegate to the
//! package manager; curl installs self-replace; cargo and npx installs only
//! print the command, because a live `cargo install` rebuild or a nested
//! `npx` run is not something to start from inside mj.

use std::ffi::OsString;
use std::io::{self, Cursor, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use flate2::read::GzDecoder;
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/BrokkAi/mjolnir/releases/latest";
const NPM_LATEST_URL: &str = "https://registry.npmjs.org/@brokkai%2Fmjolnir/latest";
const HOMEBREW_FORMULA_URL: &str =
    "https://raw.githubusercontent.com/BrokkAi/homebrew-tap/main/Formula/mjolnir.rb";
const BIN_NAME: &str = "mj";
const WINDOWS_BIN_NAME: &str = "mj.exe";
const VOICE_WORKER_NAME: &str = "mj-voice-worker";
const WINDOWS_VOICE_WORKER_NAME: &str = "mj-voice-worker.exe";
const NPM_MANAGED_ENV: &str = "MJOLNIR_MANAGED_BY_NPM";
const NPX_MANAGED_ENV: &str = "MJOLNIR_MANAGED_BY_NPX";
const HOMEBREW_MANAGED_ENV: &str = "MJOLNIR_MANAGED_BY_HOMEBREW";
const NO_UPDATE_CHECK_ENV: &str = "MJOLNIR_NO_UPDATE_CHECK";

/// A check at most once a day keeps interactive startups fast while still
/// surfacing a release the same day for daily users.
const STAMP_MAX_AGE_MS: u64 = 24 * 60 * 60 * 1000;

/// The endpoints a channel consults, grouped so loopback tests can point
/// every fetch at a local server instead of the real registries.
#[derive(Debug, Clone)]
struct UpdateSources {
    latest_release: String,
    npm_latest: String,
    homebrew_formula: String,
    cargo_index: String,
}

impl Default for UpdateSources {
    fn default() -> Self {
        Self {
            latest_release: LATEST_RELEASE_URL.to_string(),
            npm_latest: NPM_LATEST_URL.to_string(),
            homebrew_formula: HOMEBREW_FORMULA_URL.to_string(),
            cargo_index: CARGO_INDEX_URL.to_string(),
        }
    }
}

/// How the running `mj` binary was installed. Decides both where the latest
/// version is published and who is allowed to replace the binary: package
/// managers own their trees, so upgrades there run the manager's own
/// command, while a curl install may only be replaced in place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallMethod {
    Npm,
    Npx,
    Homebrew,
    Cargo { voice_worker: bool },
    Direct,
}

impl InstallMethod {
    /// Detects the install method from the launcher-declared environment
    /// markers, falling back to executable-path forensics. The markers are
    /// set by `npm/launcher/mj.js` and the Homebrew formula's wrapper script.
    fn current() -> Self {
        Self::detect(
            |name| std::env::var_os(name),
            std::env::current_exe().ok().as_deref(),
        )
    }

    fn detect<F>(env: F, exe: Option<&Path>) -> Self
    where
        F: Fn(&str) -> Option<OsString>,
    {
        if env(NPX_MANAGED_ENV).is_some() {
            return Self::Npx;
        }
        if env(NPM_MANAGED_ENV).is_some() {
            return Self::Npm;
        }
        if env(HOMEBREW_MANAGED_ENV).is_some() {
            return Self::Homebrew;
        }
        exe.map_or(Self::Direct, |exe| {
            install_method_from_exe(exe, env!("CARGO_PKG_VERSION"))
        })
    }

    /// The command a person would run to upgrade through this channel. Used
    /// verbatim for the notice-only channels and as the basis for the
    /// delegated upgrade of npm and Homebrew.
    fn update_command(&self) -> Option<String> {
        match self {
            Self::Npm => Some("npm install -g @brokkai/mjolnir@latest".to_string()),
            Self::Npx => Some("npx -y @brokkai/mjolnir@latest".to_string()),
            Self::Homebrew => Some("brew upgrade mjolnir".to_string()),
            Self::Cargo { voice_worker: true } => {
                Some("cargo install --locked brokk-mjolnir brokk-mj-voice-worker".to_string())
            }
            Self::Cargo {
                voice_worker: false,
            } => Some("cargo install --locked brokk-mjolnir".to_string()),
            Self::Direct => None,
        }
    }

    fn channel_name(&self) -> &'static str {
        match self {
            Self::Npm | Self::Npx => "npm",
            Self::Homebrew => "Homebrew",
            Self::Cargo { .. } => "crates.io",
            Self::Direct => "GitHub Releases",
        }
    }
}

fn install_method_from_exe(exe_path: &Path, current_version: &str) -> InstallMethod {
    if is_homebrew_executable(exe_path) {
        return InstallMethod::Homebrew;
    }
    if is_npm_bundle_executable(exe_path) {
        return InstallMethod::Npm;
    }

    let Some(install_root) = cargo_install_root(exe_path, current_version) else {
        return InstallMethod::Direct;
    };
    InstallMethod::Cargo {
        voice_worker: cargo_install_recorded(
            &install_root,
            "brokk-mj-voice-worker",
            None,
            VOICE_WORKER_NAME,
        ),
    }
}

fn is_homebrew_executable(exe_path: &Path) -> bool {
    let components = path_text_components(exe_path);
    components
        .windows(2)
        .any(|pair| pair == ["Cellar", "mjolnir"])
}

/// Recognizes an npm bundle binary even when the launcher's marker is
/// missing: without this, an env-less npm install would be misread as a
/// direct install and the self-replace path would write inside
/// `node_modules`, corrupting npm's package database.
fn is_npm_bundle_executable(exe_path: &Path) -> bool {
    let components = path_text_components(exe_path);
    components
        .windows(2)
        .any(|pair| pair == ["node_modules", "@brokkai"])
}

fn path_text_components(exe_path: &Path) -> Vec<&str> {
    exe_path
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .collect()
}

fn cargo_install_root(exe_path: &Path, current_version: &str) -> Option<PathBuf> {
    let canonical_exe = exe_path.canonicalize().ok()?;
    let bin_dir = canonical_exe.parent()?;
    if bin_dir.file_name()? != "bin" {
        return None;
    }
    let install_root = bin_dir.parent()?;
    cargo_install_recorded(
        install_root,
        "brokk-mjolnir",
        Some(current_version),
        BIN_NAME,
    )
    .then(|| install_root.to_path_buf())
}

fn cargo_install_recorded(
    install_root: &Path,
    package: &str,
    version: Option<&str>,
    binary: &str,
) -> bool {
    cargo_json_install_recorded(install_root, package, version, binary)
        || cargo_toml_install_recorded(install_root, package, version, binary)
}

fn cargo_json_install_recorded(
    install_root: &Path,
    package: &str,
    version: Option<&str>,
    binary: &str,
) -> bool {
    let Ok(raw) = std::fs::read_to_string(install_root.join(".crates2.json")) else {
        return false;
    };
    let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return false;
    };
    manifest
        .get("installs")
        .and_then(serde_json::Value::as_object)
        .is_some_and(|installs| {
            installs.iter().any(|(source, record)| {
                cargo_source_matches(source, package, version)
                    && record
                        .get("bins")
                        .and_then(serde_json::Value::as_array)
                        .is_some_and(|bins| bins.iter().any(|name| name.as_str() == Some(binary)))
            })
        })
}

fn cargo_toml_install_recorded(
    install_root: &Path,
    package: &str,
    version: Option<&str>,
    binary: &str,
) -> bool {
    let Ok(raw) = std::fs::read_to_string(install_root.join(".crates.toml")) else {
        return false;
    };
    let Ok(manifest) = raw.parse::<toml::Value>() else {
        return false;
    };
    manifest
        .get("v1")
        .and_then(toml::Value::as_table)
        .is_some_and(|installs| {
            installs.iter().any(|(source, bins)| {
                cargo_source_matches(source, package, version)
                    && bins
                        .as_array()
                        .is_some_and(|bins| bins.iter().any(|name| name.as_str() == Some(binary)))
            })
        })
}

fn cargo_source_matches(source: &str, package: &str, version: Option<&str>) -> bool {
    let Some(rest) = source
        .strip_prefix(package)
        .and_then(|rest| rest.strip_prefix(' '))
    else {
        return false;
    };
    let Some(recorded_version) = rest.split_whitespace().next() else {
        return false;
    };
    version.is_none_or(|expected| recorded_version == expected)
}

/// An upgrade that the running process can perform itself: the release
/// archive and its checksum sidecar, ready to download.
#[derive(Debug, Clone, PartialEq, Eq)]
struct UpdateInfo {
    version: Version,
    tag: String,
    asset: ReleaseAsset,
    checksum_asset: ReleaseAsset,
}

/// What a channel reports. `Managed` upgrades delegate to the package
/// manager; `Direct` upgrades replace the running binary from the release
/// archive.
#[derive(Debug, Clone, PartialEq, Eq)]
enum AvailableUpdate {
    Managed {
        version: Version,
        method: InstallMethod,
    },
    Direct(UpdateInfo),
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
struct GitHubRelease {
    tag_name: String,
    #[serde(default)]
    assets: Vec<ReleaseAsset>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
struct ReleaseAsset {
    name: String,
    browser_download_url: String,
}

#[derive(Debug, Deserialize)]
struct NpmLatest {
    version: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Platform {
    os_family: &'static str,
    arch: &'static str,
    rust_target: String,
}

/// Fetches the newest release the running install's channel publishes, or
/// `None` when the channel is not ahead of the running version.
async fn latest_update(
    sources: &UpdateSources,
    method: &InstallMethod,
) -> Result<Option<AvailableUpdate>> {
    let current = parse_version(env!("CARGO_PKG_VERSION"))
        .with_context(|| format!("parse current version {}", env!("CARGO_PKG_VERSION")))?;
    if *method == InstallMethod::Direct {
        let release = fetch_latest_release(sources)
            .await
            .context("fetch latest mj release")?;
        return update_info_from_release(&release, &current, &current_platform()?)
            .map(|update| update.map(AvailableUpdate::Direct));
    }

    let latest = fetch_latest_managed_version(sources, method).await?;
    Ok((latest > current).then(|| AvailableUpdate::Managed {
        version: latest,
        method: method.clone(),
    }))
}

async fn fetch_latest_release(sources: &UpdateSources) -> Result<GitHubRelease> {
    let body = fetch_text(&sources.latest_release).await?;
    serde_json::from_str(&body).context("parse release body")
}

async fn fetch_latest_managed_version(
    sources: &UpdateSources,
    method: &InstallMethod,
) -> Result<Version> {
    match method {
        InstallMethod::Npm | InstallMethod::Npx => {
            let body = fetch_text(&sources.npm_latest)
                .await
                .context("fetch latest npm package")?;
            let latest: NpmLatest = serde_json::from_str(&body).context("parse npm metadata")?;
            parse_version(&latest.version).context("parse latest npm version")
        }
        InstallMethod::Homebrew => {
            let body = fetch_text(&sources.homebrew_formula)
                .await
                .context("fetch Homebrew formula")?;
            parse_homebrew_formula_version(&body)
        }
        InstallMethod::Cargo { .. } => {
            // crates.io installs are notice-only, but the notice still needs
            // to know whether anything newer exists. The sparse index lists
            // every published version, yanked ones included-and-skipped.
            let body = fetch_text(&sources.cargo_index)
                .await
                .context("fetch crates.io index entry")?;
            parse_cargo_index_version(&body)
        }
        InstallMethod::Direct => anyhow::bail!("direct installs use GitHub release metadata"),
    }
}

const CARGO_INDEX_URL: &str = "https://index.crates.io/br/ok/brokk-mjolnir";

async fn fetch_text(url: &str) -> Result<String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
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
    resp.text().await.with_context(|| format!("read {url}"))
}

fn parse_homebrew_formula_version(formula: &str) -> Result<Version> {
    let raw = formula
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("version \"")?.strip_suffix('"'))
        .ok_or_else(|| anyhow::anyhow!("Homebrew formula has no version"))?;
    parse_version(raw).context("parse Homebrew formula version")
}

fn parse_cargo_index_version(index: &str) -> Result<Version> {
    let mut latest: Option<Version> = None;
    for line in index.lines().filter(|line| !line.trim().is_empty()) {
        let entry: CargoIndexEntry =
            serde_json::from_str(line).context("parse crates.io index entry")?;
        if entry.yanked {
            continue;
        }
        let version = parse_version(&entry.vers).context("parse crates.io package version")?;
        if latest.as_ref().is_none_or(|current| version > *current) {
            latest = Some(version);
        }
    }
    latest.ok_or_else(|| anyhow::anyhow!("crates.io index has no published versions"))
}

#[derive(Debug, Deserialize)]
struct CargoIndexEntry {
    vers: String,
    #[serde(default)]
    yanked: bool,
}

fn update_info_from_release(
    release: &GitHubRelease,
    current: &Version,
    platform: &Platform,
) -> Result<Option<UpdateInfo>> {
    let latest = parse_version(&release.tag_name)
        .with_context(|| format!("parse release tag {}", release.tag_name))?;
    if latest <= *current {
        return Ok(None);
    }

    let asset = select_mj_asset(&release.assets, platform)
        .with_context(|| format!("find mj asset for {}/{}", platform.os_family, platform.arch))?;
    let checksum_name = format!("{}.sha256", asset.name);
    let checksum_asset = release
        .assets
        .iter()
        .find(|candidate| candidate.name == checksum_name)
        .cloned()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "release {} is missing required checksum asset {}",
                release.tag_name,
                checksum_name
            )
        })?;

    Ok(Some(UpdateInfo {
        version: latest,
        tag: release.tag_name.clone(),
        asset,
        checksum_asset,
    }))
}

fn select_mj_asset(assets: &[ReleaseAsset], platform: &Platform) -> Result<ReleaseAsset> {
    let target_suffix = format!(
        "-{}{}",
        platform.rust_target,
        platform_archive_ext(platform)
    );
    if platform.os_family == "macos"
        && let Some(asset) = assets.iter().find(|asset| {
            is_mj_archive(&asset.name) && asset.name.ends_with("-universal-apple-darwin.tar.gz")
        })
    {
        return Ok(asset.clone());
    }
    assets
        .iter()
        .find(|asset| is_mj_archive(&asset.name) && asset.name.ends_with(&target_suffix))
        .cloned()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no mj archive found for target {}; available assets: {}",
                platform.rust_target,
                assets
                    .iter()
                    .filter(|asset| !asset.name.ends_with(".sha256"))
                    .map(|asset| asset.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
}

fn is_mj_archive(name: &str) -> bool {
    name.starts_with("brokk-mjolnir-") && (name.ends_with(".tar.gz") || name.ends_with(".zip"))
}

fn platform_archive_ext(platform: &Platform) -> &'static str {
    if platform.os_family == "windows" {
        ".zip"
    } else {
        ".tar.gz"
    }
}

fn current_platform() -> Result<Platform> {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" | "arm64" => "aarch64",
        other => anyhow::bail!("unsupported CPU architecture: {other}"),
    };
    let (os_family, rust_os) = match std::env::consts::OS {
        "android" => ("android", "linux-android"),
        "macos" => ("macos", "apple-darwin"),
        "linux" => ("linux", "unknown-linux-gnu"),
        "windows" => ("windows", "pc-windows-msvc"),
        other => anyhow::bail!("unsupported OS: {other}"),
    };

    Ok(Platform {
        os_family,
        arch,
        rust_target: format!("{arch}-{rust_os}"),
    })
}

fn parse_version(raw: &str) -> Result<Version> {
    Version::parse(raw.trim_start_matches('v')).with_context(|| format!("parse version {raw}"))
}

/// Persists when the updater last reached the network, so `mj` checks at
/// most once a day instead of on every interactive start. Written before the
/// fetch so a hung request cannot turn into a retry on every start.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct UpdateCheckStamp {
    last_check_ms: u64,
}

fn stamp_path() -> PathBuf {
    hel::hel_config::data_dir().join("update-check.json")
}

fn read_last_check_ms() -> Option<u64> {
    let raw = std::fs::read_to_string(stamp_path()).ok()?;
    let stamp: UpdateCheckStamp = serde_json::from_str(&raw).ok()?;
    Some(stamp.last_check_ms)
}

/// Best-effort: a stamp that cannot be written only costs an extra check on
/// the next start, never an upgrade failure.
fn write_last_check_ms(now_ms: u64) {
    let stamp = UpdateCheckStamp {
        last_check_ms: now_ms,
    };
    let Ok(body) = serde_json::to_string(&stamp) else {
        return;
    };
    if let Err(error) = hel::hel_config::atomic_write(&stamp_path(), body.as_bytes()) {
        tracing::debug!(%error, path = %stamp_path().display(), "could not persist update-check stamp");
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis() as u64)
        .unwrap_or_default()
}

fn check_is_due(last_check_ms: Option<u64>, now_ms: u64) -> bool {
    last_check_ms.is_none_or(|last| now_ms.saturating_sub(last) >= STAMP_MAX_AGE_MS)
}

/// What the startup check decided. A successful upgrade never produces a
/// value: the process re-execs into the new binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupUpdateOutcome {
    /// The check did not run (debug build, non-interactive, disabled, or it
    /// failed without affecting the session).
    Skipped,
    UpToDate,
    /// The channel only allows announcing the upgrade command; the user must
    /// run it themselves.
    Notified,
    Declined,
}

/// Checks the running install's channel for a newer release, asks before
/// changing anything, and on consent performs the upgrade:
///
/// - npm installs run `npm install -g @brokkai/mjolnir@latest` and Homebrew
///   installs run `brew update` followed by `brew upgrade mjolnir`; mj never
///   writes into `node_modules` or the Cellar itself, because those trees
///   belong to the package managers.
/// - curl installs download the release archive, verify its SHA-256 sidecar,
///   replace the running executable, and re-exec (the 1.x mechanism,
///   unchanged).
/// - npx and cargo installs are notice-only.
///
/// Runs before the dashboard's event loop exists: the version fetch is
/// throttled and time-boxed, and the interactive package-manager runs share
/// the terminal like any foreground command, so this must never be called
/// from a UI render path.
pub async fn check_prompt_and_apply() -> StartupUpdateOutcome {
    // The controller ships for Linux and macOS (Windows users run WSL2), so
    // there is no Windows replacer to maintain; debug builds are developers,
    // who upgrade themselves.
    if cfg!(windows)
        || cfg!(debug_assertions)
        || !io::stdin().is_terminal()
        || !io::stdout().is_terminal()
        || std::env::var_os(NO_UPDATE_CHECK_ENV).is_some()
    {
        return StartupUpdateOutcome::Skipped;
    }

    let method = InstallMethod::current();
    if !check_is_due(read_last_check_ms(), now_ms()) {
        return StartupUpdateOutcome::Skipped;
    }
    write_last_check_ms(now_ms());

    let update = match latest_update(&UpdateSources::default(), &method).await {
        Ok(Some(update)) => update,
        Ok(None) => return StartupUpdateOutcome::UpToDate,
        Err(error) => {
            // A broken check must never keep someone out of mj.
            eprintln!("mj: update check failed: {error:#}");
            return StartupUpdateOutcome::Skipped;
        }
    };

    match update {
        AvailableUpdate::Direct(update) => {
            if !prompt_for_update(&update.version, &InstallMethod::Direct).unwrap_or(false) {
                return StartupUpdateOutcome::Declined;
            }
            if let Err(error) = download_apply_and_restart(&update).await {
                eprintln!("mj: upgrade failed: {error:#}");
                eprintln!("mj: continuing with {}", env!("CARGO_PKG_VERSION"));
            }
            // The success path re-execs and never returns; reaching this
            // line means the upgrade failed and the current process lives on.
            StartupUpdateOutcome::Skipped
        }
        AvailableUpdate::Managed { version, method } => match method {
            InstallMethod::Npm | InstallMethod::Homebrew => {
                if !prompt_for_update(&version, &method).unwrap_or(false) {
                    return StartupUpdateOutcome::Declined;
                }
                let upgraded = run_managed_upgrade(&version, &method)
                    .and_then(|()| restart_after_managed_upgrade(&method));
                if let Err(error) = upgraded {
                    eprintln!("mj: upgrade failed: {error:#}");
                    eprintln!("mj: continuing with {}", env!("CARGO_PKG_VERSION"));
                }
                StartupUpdateOutcome::Skipped
            }
            InstallMethod::Npx | InstallMethod::Cargo { .. } => {
                let notice = managed_update_notice(&version, &method, env!("CARGO_PKG_VERSION"))
                    .expect("notice-only channels have an update command");
                println!("{notice}");
                StartupUpdateOutcome::Notified
            }
            InstallMethod::Direct => {
                unreachable!("direct installs never report managed updates")
            }
        },
    }
}

fn prompt_for_update(version: &Version, method: &InstallMethod) -> Result<bool> {
    print!(
        "mj {version} is available through {}; current version is {}. Upgrade now? [Y/n] ",
        method.channel_name(),
        env!("CARGO_PKG_VERSION")
    );
    io::stdout().flush().context("flush update prompt")?;

    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .context("read update prompt answer")?;
    Ok(prompt_answer_is_yes(&answer))
}

/// An empty answer accepts, matching the 1.x prompt's default.
fn prompt_answer_is_yes(answer: &str) -> bool {
    matches!(answer.trim(), "" | "y" | "Y" | "yes" | "YES")
}

fn managed_update_notice(
    version: &Version,
    method: &InstallMethod,
    current_version: &str,
) -> Option<String> {
    Some(format!(
        "mj {version} is available through {}; current version is {current_version}. Run: {}",
        method.channel_name(),
        method.update_command()?
    ))
}

fn npm_upgrade_command() -> Command {
    let mut command = Command::new("npm");
    command.args(["install", "-g", "@brokkai/mjolnir@latest"]);
    command
}

fn brew_update_command() -> Command {
    let mut command = Command::new("brew");
    command.arg("update");
    command
}

fn brew_upgrade_command() -> Command {
    let mut command = Command::new("brew");
    command.args(["upgrade", "mjolnir"]);
    command
}

/// Runs the channel's own upgrade command in the foreground with its live
/// output on the terminal. `run_inherited` keeps stdin closed so neither
/// package manager can stop to ask a question nobody is there to answer.
fn run_managed_upgrade(version: &Version, method: &InstallMethod) -> Result<()> {
    match method {
        InstallMethod::Npm => {
            println!("mj: running npm install -g @brokkai/mjolnir@latest");
            let status = hel::hel_subprocess::run_inherited(&mut npm_upgrade_command())
                .context("run npm install -g @brokkai/mjolnir@latest")?;
            ensure!(
                status.success(),
                "npm install exited with {status}; npm usually explains why above"
            );
        }
        InstallMethod::Homebrew => {
            // The version check read the tap formula on GitHub, but the
            // local brew only knows about it after its index refreshes;
            // without `brew update` the upgrade would report "already
            // up-to-date" on a fresh release.
            println!("mj: running brew update");
            let status = hel::hel_subprocess::run_inherited(&mut brew_update_command())
                .context("run brew update")?;
            ensure!(status.success(), "brew update exited with {status}");
            println!("mj: running brew upgrade mjolnir");
            let status = hel::hel_subprocess::run_inherited(&mut brew_upgrade_command())
                .context("run brew upgrade mjolnir")?;
            ensure!(status.success(), "brew upgrade exited with {status}");
        }
        other => bail!("{other:?} installs do not support delegated upgrades"),
    }
    println!("mj: upgraded to {version}; restarting");
    Ok(())
}

/// How the process re-execs after a successful managed upgrade.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RestartTarget {
    /// The upgrade replaced the binary file at this exact path, so re-execing
    /// it loads the new version (direct replacements and npm bundles, whose
    /// paths survive `npm install -g`).
    SameExe(PathBuf),
    /// Homebrew moves the new release into a fresh Cellar directory and
    /// repoints its wrappers, so re-execing this process's own Cellar path
    /// would relaunch the old version. Resolving `mj` on `PATH` runs the
    /// formula's wrapper, which execs the new libexec binary.
    Wrapper,
}

fn managed_restart_target(method: &InstallMethod, current_exe: &Path) -> Result<RestartTarget> {
    match method {
        InstallMethod::Npm => Ok(RestartTarget::SameExe(current_exe.to_path_buf())),
        InstallMethod::Homebrew => Ok(RestartTarget::Wrapper),
        other => bail!("{other:?} installs do not support delegated upgrades"),
    }
}

fn restart_after_managed_upgrade(method: &InstallMethod) -> Result<()> {
    let current_exe = std::env::current_exe().context("resolve current executable")?;
    restart_current_process(managed_restart_target(method, &current_exe)?)
}

#[cfg(unix)]
fn restart_current_process(target: RestartTarget) -> Result<()> {
    use std::os::unix::process::CommandExt;

    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let mut command = match target {
        RestartTarget::SameExe(exe) => Command::new(exe),
        RestartTarget::Wrapper => Command::new("mj"),
    };
    let error = command.args(args).exec();
    Err(error).context("exec replacement mj")
}

#[cfg(not(unix))]
fn restart_current_process(_target: RestartTarget) -> Result<()> {
    bail!("automatic restart is only supported on Unix platforms")
}

async fn download_apply_and_restart(update: &UpdateInfo) -> Result<()> {
    println!("mj: downloading {} ({})", update.tag, update.asset.name);
    let archive = download_bytes(&update.asset.browser_download_url)
        .await
        .with_context(|| format!("download {}", update.asset.name))?;
    verify_checksum(update, &archive).await?;

    let new_binary =
        extract_mj_binary(&update.asset.name, &archive).context("extract mj binary")?;
    let current_exe = std::env::current_exe().context("resolve current executable")?;
    if !cfg!(target_os = "android")
        && let Some(worker) = extract_optional_voice_worker(&update.asset.name, &archive)
    {
        install_voice_worker(&current_exe, &worker).context("install voice worker")?;
    }
    let replacement =
        replace_current_exe(&current_exe, &new_binary).context("replace current executable")?;

    println!("mj: upgraded to {}; restarting", update.tag);
    restart_current_process(RestartTarget::SameExe(replacement))
}

async fn download_bytes(url: &str) -> Result<Vec<u8>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
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
    resp.bytes()
        .await
        .map(|bytes| bytes.to_vec())
        .context("read response body")
}

async fn verify_checksum(update: &UpdateInfo, archive: &[u8]) -> Result<()> {
    let body = download_bytes(&update.checksum_asset.browser_download_url)
        .await
        .with_context(|| format!("download {}", update.checksum_asset.name))?;
    let body = String::from_utf8(body).context("checksum file is not utf-8")?;
    let expected = body
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow::anyhow!("empty checksum file {}", update.checksum_asset.name))?;
    let actual = sha256_hex(archive);
    if expected != actual {
        bail!(
            "checksum mismatch for {}: expected {expected}, got {actual}",
            update.asset.name
        );
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn extract_mj_binary(archive_name: &str, archive_bytes: &[u8]) -> Result<Vec<u8>> {
    extract_named_binary(archive_name, archive_bytes, BIN_NAME, WINDOWS_BIN_NAME)
}

fn extract_voice_worker_binary(archive_name: &str, archive_bytes: &[u8]) -> Result<Vec<u8>> {
    extract_named_binary(
        archive_name,
        archive_bytes,
        VOICE_WORKER_NAME,
        WINDOWS_VOICE_WORKER_NAME,
    )
}

/// Sidecar binaries are optional in release archives: a future release that
/// retires the voice worker must not strand older updaters. Only `mj` itself
/// is mandatory.
fn extract_optional_voice_worker(archive_name: &str, archive_bytes: &[u8]) -> Option<Vec<u8>> {
    match extract_voice_worker_binary(archive_name, archive_bytes) {
        Ok(worker) => Some(worker),
        Err(e) => {
            eprintln!("mj: skipping voice worker update: {e:#}");
            None
        }
    }
}

fn extract_named_binary(
    archive_name: &str,
    archive_bytes: &[u8],
    unix_name: &str,
    windows_name: &str,
) -> Result<Vec<u8>> {
    if archive_name.ends_with(".zip") {
        return extract_named_binary_from_zip(archive_bytes, windows_name);
    }
    extract_named_binary_from_tar_gz(archive_bytes, unix_name)
}

fn extract_named_binary_from_tar_gz(archive_bytes: &[u8], expected_name: &str) -> Result<Vec<u8>> {
    let gz = GzDecoder::new(archive_bytes);
    let mut archive = tar::Archive::new(gz);
    for entry in archive.entries().context("read tar entries")? {
        let mut entry = entry.context("read tar entry")?;
        let path = entry.path().context("read tar entry path")?;
        if path.file_name().and_then(|name| name.to_str()) != Some(expected_name) {
            continue;
        }
        let mut bytes = Vec::new();
        entry
            .read_to_end(&mut bytes)
            .context("read binary from archive")?;
        if bytes.is_empty() {
            bail!("archive contained an empty {expected_name} binary");
        }
        return Ok(bytes);
    }
    bail!("archive did not contain expected binary: {expected_name}")
}

fn extract_named_binary_from_zip(archive_bytes: &[u8], expected_name: &str) -> Result<Vec<u8>> {
    let cursor = Cursor::new(archive_bytes);
    let mut archive = zip::ZipArchive::new(cursor).context("open zip archive")?;
    for index in 0..archive.len() {
        let mut file = archive
            .by_index(index)
            .with_context(|| format!("read zip entry {index}"))?;
        let path = file
            .enclosed_name()
            .ok_or_else(|| anyhow::anyhow!("zip entry escapes destination: {}", file.name()))?;
        if path.file_name().and_then(|name| name.to_str()) != Some(expected_name) {
            continue;
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .with_context(|| format!("read {expected_name} binary from archive"))?;
        if bytes.is_empty() {
            bail!("archive contained an empty {expected_name} binary");
        }
        return Ok(bytes);
    }
    bail!("archive did not contain expected binary: {expected_name}")
}

fn install_voice_worker(current_exe: &Path, bytes: &[u8]) -> Result<()> {
    install_sibling_binary(
        current_exe,
        VOICE_WORKER_NAME,
        WINDOWS_VOICE_WORKER_NAME,
        bytes,
    )
}

fn install_sibling_binary(
    current_exe: &Path,
    unix_name: &str,
    windows_name: &str,
    bytes: &[u8],
) -> Result<()> {
    let current_exe = current_exe
        .canonicalize()
        .with_context(|| format!("resolve executable target {}", current_exe.display()))?;
    let parent = current_exe
        .parent()
        .ok_or_else(|| anyhow::anyhow!("executable has no parent: {}", current_exe.display()))?;
    let name = if cfg!(windows) {
        windows_name
    } else {
        unix_name
    };
    let target = parent.join(name);
    let tmp = parent.join(format!(".{name}.self-update.{}.tmp", std::process::id()));
    std::fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("chmod {}", tmp.display()))?;
    }
    std::fs::rename(&tmp, &target)
        .with_context(|| format!("rename {} -> {}", tmp.display(), target.display()))?;
    strip_quarantine(&target);
    Ok(())
}

/// Writes the new binary next to the running one and renames it into place,
/// which on Unix atomically replaces the file even while this process is
/// still executing its old contents. Returns the resolved replacement path.
#[cfg(unix)]
fn replace_current_exe(current_exe: &Path, new_binary: &[u8]) -> Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    let target_exe = current_exe
        .canonicalize()
        .with_context(|| format!("resolve executable target {}", current_exe.display()))?;
    let parent = target_exe
        .parent()
        .ok_or_else(|| anyhow::anyhow!("executable has no parent: {}", target_exe.display()))?;
    let tmp_path = parent.join(format!(
        ".{}.self-update.{}.tmp",
        BIN_NAME,
        std::process::id()
    ));

    std::fs::write(&tmp_path, new_binary)
        .with_context(|| format!("write {}", tmp_path.display()))?;
    std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("chmod {}", tmp_path.display()))?;

    strip_quarantine(&tmp_path);
    std::fs::rename(&tmp_path, &target_exe)
        .with_context(|| format!("rename {} -> {}", tmp_path.display(), target_exe.display()))?;
    strip_quarantine(&target_exe);
    Ok(target_exe)
}

#[cfg(not(unix))]
fn replace_current_exe(_current_exe: &Path, _new_binary: &[u8]) -> Result<PathBuf> {
    bail!("self-update replacement is only supported on Unix platforms")
}

#[cfg(unix)]
fn strip_quarantine(path: &Path) {
    #[cfg(target_os = "macos")]
    {
        // Downloads inherit a quarantine attribute on macOS; Gatekeeper
        // would refuse to exec the replacement without this.
        let _ = Command::new("xattr")
            .arg("-dr")
            .arg("com.apple.quarantine")
            .arg(path)
            .status();
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = path;
    }
}

#[cfg(not(unix))]
fn strip_quarantine(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn exe(path: &str) -> Option<&Path> {
        Some(Path::new(path))
    }

    fn detect_with_env(markers: &[&str], exe_path: Option<&Path>) -> InstallMethod {
        InstallMethod::detect(
            |name| markers.contains(&name).then(|| OsString::from("true")),
            exe_path,
        )
    }

    #[test]
    fn launcher_markers_override_path_forensics() {
        let npm_bundle = "/usr/lib/node_modules/@brokkai/mjolnir-linux-x64-gnu/bin/mj";
        assert_eq!(
            detect_with_env(&[NPM_MANAGED_ENV], exe("/home/user/.local/bin/mj")),
            InstallMethod::Npm
        );
        assert_eq!(
            detect_with_env(&[NPX_MANAGED_ENV, NPM_MANAGED_ENV], exe(npm_bundle)),
            InstallMethod::Npx
        );
        assert_eq!(
            detect_with_env(&[HOMEBREW_MANAGED_ENV], exe(npm_bundle)),
            InstallMethod::Homebrew
        );
    }

    #[test]
    fn marker_free_detection_reads_the_executable_path() {
        assert_eq!(
            InstallMethod::detect(
                |_| None,
                exe("/opt/homebrew/Cellar/mjolnir/2.4.0/libexec/mj")
            ),
            InstallMethod::Homebrew
        );
        assert_eq!(
            InstallMethod::detect(
                |_| None,
                exe("/home/linuxbrew/.linuxbrew/Cellar/mjolnir/2.4.0/libexec/mj")
            ),
            InstallMethod::Homebrew
        );
        assert_eq!(
            InstallMethod::detect(
                |_| None,
                exe(
                    "/usr/lib/node_modules/@brokkai/mjolnir/node_modules/@brokkai/mjolnir-linux-x64-gnu/bin/mj"
                )
            ),
            InstallMethod::Npm
        );
        assert_eq!(
            InstallMethod::detect(|_| None, exe("/home/user/.local/bin/mj")),
            InstallMethod::Direct
        );
        assert_eq!(InstallMethod::detect(|_| None, None), InstallMethod::Direct);
    }

    #[test]
    fn cargo_detection_requires_a_matching_install_record() {
        let root = tempfile::tempdir().expect("tempdir");
        let bin_dir = root.path().join("bin");
        std::fs::create_dir(&bin_dir).expect("bin dir");
        let executable = bin_dir.join(BIN_NAME);
        std::fs::write(&executable, b"mj").expect("executable");
        let recorded = |body: &str| {
            std::fs::write(root.path().join(".crates.toml"), body).expect("manifest");
            InstallMethod::detect(|_| None, Some(executable.as_path()))
        };

        assert_eq!(
            recorded(concat!(
                "[v1]\n",
                "\"brokk-mjolnir 2.4.0 (registry+https://github.com/rust-lang/crates.io-index)\" = [\"mj\"]\n",
                "\"brokk-mj-voice-worker 2.4.0 (registry+https://github.com/rust-lang/crates.io-index)\" = [\"mj-voice-worker\"]\n",
            )),
            InstallMethod::Cargo { voice_worker: true }
        );
        assert_eq!(
            recorded(concat!(
                "[v1]\n",
                "\"brokk-mjolnir 2.4.0 (registry+https://github.com/rust-lang/crates.io-index)\" = [\"mj\"]\n",
            )),
            InstallMethod::Cargo {
                voice_worker: false
            }
        );
        // A record for a different installed version is not this install.
        assert_eq!(
            recorded(concat!(
                "[v1]\n",
                "\"brokk-mjolnir 2.3.0 (registry+https://github.com/rust-lang/crates.io-index)\" = [\"mj\"]\n",
            )),
            InstallMethod::Direct
        );
    }

    #[test]
    fn managed_install_methods_provide_their_own_update_commands() {
        assert_eq!(
            InstallMethod::Npm.update_command().as_deref(),
            Some("npm install -g @brokkai/mjolnir@latest")
        );
        assert_eq!(
            InstallMethod::Npx.update_command().as_deref(),
            Some("npx -y @brokkai/mjolnir@latest")
        );
        assert_eq!(
            InstallMethod::Homebrew.update_command().as_deref(),
            Some("brew upgrade mjolnir")
        );
        assert_eq!(
            InstallMethod::Cargo { voice_worker: true }
                .update_command()
                .as_deref(),
            Some("cargo install --locked brokk-mjolnir brokk-mj-voice-worker")
        );
        assert_eq!(InstallMethod::Direct.update_command(), None);
    }

    #[test]
    fn channels_name_their_distribution_source() {
        assert_eq!(InstallMethod::Npm.channel_name(), "npm");
        assert_eq!(InstallMethod::Homebrew.channel_name(), "Homebrew");
        assert_eq!(
            InstallMethod::Cargo {
                voice_worker: false
            }
            .channel_name(),
            "crates.io"
        );
        assert_eq!(InstallMethod::Direct.channel_name(), "GitHub Releases");
    }

    #[test]
    fn parses_homebrew_formula_version() {
        let formula = r#"
class Mjolnir < Formula
  desc "Session control plane for ACP coding agents"
  version "2.5.0"
end
"#;
        assert_eq!(
            parse_homebrew_formula_version(formula).expect("version"),
            Version::parse("2.5.0").expect("semver")
        );
        assert!(parse_homebrew_formula_version("class Mjolnir < Formula\nend").is_err());
    }

    #[test]
    fn cargo_index_uses_latest_non_yanked_version() {
        let index = concat!(
            r#"{"vers":"2.4.0","yanked":false}"#,
            "\n",
            r#"{"vers":"2.5.0","yanked":true}"#,
            "\n",
            r#"{"vers":"2.4.2","yanked":false}"#,
            "\n",
        );
        assert_eq!(
            parse_cargo_index_version(index).expect("version"),
            Version::parse("2.4.2").expect("semver")
        );
    }

    #[test]
    fn parse_version_tolerates_release_tags() {
        assert_eq!(
            parse_version("v2.5.0").expect("version"),
            Version::parse("2.5.0").expect("semver")
        );
    }

    fn asset(name: &str) -> ReleaseAsset {
        ReleaseAsset {
            name: name.to_string(),
            browser_download_url: format!("https://example.com/{name}"),
        }
    }

    fn linux_x64() -> Platform {
        Platform {
            os_family: "linux",
            arch: "x86_64",
            rust_target: "x86_64-unknown-linux-gnu".to_string(),
        }
    }

    fn mac_arm() -> Platform {
        Platform {
            os_family: "macos",
            arch: "aarch64",
            rust_target: "aarch64-apple-darwin".to_string(),
        }
    }

    #[test]
    fn release_newer_than_current_returns_update_info() {
        let release = GitHubRelease {
            tag_name: "v2.5.0".to_string(),
            assets: vec![
                asset("brokk-mjolnir-v2.5.0-x86_64-unknown-linux-gnu.tar.gz"),
                asset("brokk-mjolnir-v2.5.0-x86_64-unknown-linux-gnu.tar.gz.sha256"),
            ],
        };

        let update = update_info_from_release(
            &release,
            &Version::parse("2.4.0").expect("version"),
            &linux_x64(),
        )
        .expect("update info")
        .expect("update");

        assert_eq!(update.version, Version::parse("2.5.0").expect("version"));
        assert_eq!(
            update.asset.name,
            "brokk-mjolnir-v2.5.0-x86_64-unknown-linux-gnu.tar.gz"
        );
        assert_eq!(
            update.checksum_asset.name,
            "brokk-mjolnir-v2.5.0-x86_64-unknown-linux-gnu.tar.gz.sha256"
        );
    }

    #[test]
    fn release_not_newer_returns_none() {
        let release = GitHubRelease {
            tag_name: "v2.4.0".to_string(),
            assets: vec![asset(
                "brokk-mjolnir-v2.4.0-x86_64-unknown-linux-gnu.tar.gz",
            )],
        };

        let update = update_info_from_release(
            &release,
            &Version::parse("2.4.0").expect("version"),
            &linux_x64(),
        )
        .expect("update info");

        assert!(update.is_none());
    }

    #[test]
    fn release_newer_than_current_requires_checksum_asset() {
        let release = GitHubRelease {
            tag_name: "v2.5.0".to_string(),
            assets: vec![asset(
                "brokk-mjolnir-v2.5.0-x86_64-unknown-linux-gnu.tar.gz",
            )],
        };

        let error = update_info_from_release(
            &release,
            &Version::parse("2.4.0").expect("version"),
            &linux_x64(),
        )
        .expect_err("missing checksum should fail");

        assert!(error
            .to_string()
            .contains("missing required checksum asset brokk-mjolnir-v2.5.0-x86_64-unknown-linux-gnu.tar.gz.sha256"));
    }

    #[test]
    fn macos_prefers_universal_asset() {
        let assets = vec![
            asset("brokk-mjolnir-v2.5.0-aarch64-apple-darwin.tar.gz"),
            asset("brokk-mjolnir-v2.5.0-universal-apple-darwin.tar.gz"),
        ];

        let selected = select_mj_asset(&assets, &mac_arm()).expect("select");

        assert_eq!(
            selected.name,
            "brokk-mjolnir-v2.5.0-universal-apple-darwin.tar.gz"
        );
    }

    #[test]
    fn linux_selects_target_asset() {
        let assets = vec![
            asset("brokk-mjolnir-v2.5.0-aarch64-unknown-linux-gnu.tar.gz"),
            asset("brokk-mjolnir-v2.5.0-x86_64-unknown-linux-gnu.tar.gz"),
        ];

        let selected = select_mj_asset(&assets, &linux_x64()).expect("select");

        assert_eq!(
            selected.name,
            "brokk-mjolnir-v2.5.0-x86_64-unknown-linux-gnu.tar.gz"
        );
    }

    #[test]
    fn stale_check_stamps_are_due() {
        let day_ms = STAMP_MAX_AGE_MS;
        assert!(check_is_due(None, 1_000));
        assert!(check_is_due(Some(0), day_ms));
        assert!(!check_is_due(Some(0), day_ms - 1));
        assert!(check_is_due(Some(0), u64::MAX)); // never panics on overflow
    }

    #[test]
    fn empty_prompt_answer_accepts_the_upgrade() {
        assert!(prompt_answer_is_yes(""));
        assert!(prompt_answer_is_yes("\n"));
        assert!(prompt_answer_is_yes(" y \n"));
        assert!(prompt_answer_is_yes("Y"));
        assert!(prompt_answer_is_yes("yes"));
        assert!(prompt_answer_is_yes("YES"));
        assert!(!prompt_answer_is_yes("n"));
        assert!(!prompt_answer_is_yes("N"));
        assert!(!prompt_answer_is_yes("no"));
        assert!(!prompt_answer_is_yes("later"));
    }

    #[test]
    fn managed_update_notice_names_channel_version_and_command() {
        assert_eq!(
            managed_update_notice(
                &Version::parse("2.5.0").expect("version"),
                &InstallMethod::Homebrew,
                "2.4.0",
            )
            .as_deref(),
            Some(
                "mj 2.5.0 is available through Homebrew; current version is 2.4.0. Run: brew upgrade mjolnir"
            )
        );
        assert_eq!(
            managed_update_notice(
                &Version::parse("2.5.0").expect("version"),
                &InstallMethod::Cargo {
                    voice_worker: false
                },
                "2.4.0",
            )
            .as_deref(),
            Some(
                "mj 2.5.0 is available through crates.io; current version is 2.4.0. Run: cargo install --locked brokk-mjolnir"
            )
        );
    }

    #[test]
    fn delegated_upgrades_run_the_package_managers_own_commands() {
        let npm = npm_upgrade_command();
        assert_eq!(npm.get_program(), "npm");
        let npm_args: Vec<String> = npm
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();
        assert_eq!(npm_args, ["install", "-g", "@brokkai/mjolnir@latest"]);

        let brew_update = brew_update_command();
        assert_eq!(brew_update.get_program(), "brew");
        assert_eq!(brew_update.get_args().count(), 1);
        assert_eq!(brew_update.get_args().next().unwrap(), "update");

        let brew_upgrade = brew_upgrade_command();
        assert_eq!(brew_upgrade.get_program(), "brew");
        let brew_args: Vec<String> = brew_upgrade
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();
        assert_eq!(brew_args, ["upgrade", "mjolnir"]);
    }

    #[test]
    fn managed_restart_retargets_homebrew_to_its_wrapper() {
        // SameExe is correct for npm because `npm install -g` replaces the
        // bundle files in place; Homebrew must re-resolve the wrapper or the
        // restart would relaunch the old Cellar version.
        let exe = Path::new("/opt/homebrew/Cellar/mjolnir/2.4.0/libexec/mj");
        assert_eq!(
            managed_restart_target(&InstallMethod::Npm, exe).expect("target"),
            RestartTarget::SameExe(exe.to_path_buf())
        );
        assert_eq!(
            managed_restart_target(&InstallMethod::Homebrew, exe).expect("target"),
            RestartTarget::Wrapper
        );
        assert!(managed_restart_target(&InstallMethod::Npx, exe).is_err());
    }

    /// Serves canned bodies per path prefix from a loopback port and returns
    /// sources pointed at it, so channel fetches never leave the machine.
    async fn serve_update_sources(
        routes: Vec<(&'static str, &'static str)>,
    ) -> (UpdateSources, std::net::SocketAddr) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback listener");
        let addr = listener.local_addr().expect("listener address");
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut buffer = vec![0u8; 4096];
                let read = socket.read(&mut buffer).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buffer[..read]).to_string();
                let path = request.split_whitespace().nth(1).unwrap_or_default();
                let matched = routes
                    .iter()
                    .find(|(route, _)| path.starts_with(route))
                    .map(|(_, body)| *body);
                let (status, body) = match matched {
                    Some(body) => ("200 OK", body),
                    None => ("404 Not Found", ""),
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });

        let base = format!("http://{addr}");
        (
            UpdateSources {
                latest_release: format!("{base}/release"),
                npm_latest: format!("{base}/npm"),
                homebrew_formula: format!("{base}/formula"),
                cargo_index: format!("{base}/index"),
            },
            addr,
        )
    }

    #[tokio::test]
    async fn npm_registry_update_is_offered_when_newer() {
        let (sources, _) = serve_update_sources(vec![("/npm", r#"{"version":"9.9.9"}"#)]).await;

        let update = latest_update(&sources, &InstallMethod::Npm)
            .await
            .expect("update check");

        assert_eq!(
            update,
            Some(AvailableUpdate::Managed {
                version: Version::parse("9.9.9").expect("version"),
                method: InstallMethod::Npm,
            })
        );
    }

    #[tokio::test]
    async fn up_to_date_channel_offers_nothing() {
        let (sources, _) = serve_update_sources(vec![("/npm", r#"{"version":"0.0.1"}"#)]).await;

        let update = latest_update(&sources, &InstallMethod::Npm)
            .await
            .expect("update check");

        assert_eq!(update, None);
    }

    #[tokio::test]
    async fn homebrew_formula_update_is_offered_when_newer() {
        let formula = "class Mjolnir < Formula\n  version \"9.9.9\"\nend\n";
        let (sources, _) = serve_update_sources(vec![("/formula", formula)]).await;

        let update = latest_update(&sources, &InstallMethod::Homebrew)
            .await
            .expect("update check");

        assert!(matches!(update, Some(AvailableUpdate::Managed { .. })));
    }

    #[tokio::test]
    async fn failed_channel_fetch_is_reported_as_an_error() {
        // No route matches /npm, so the stub answers 404.
        let (sources, _) = serve_update_sources(vec![("/formula", "unused")]).await;

        let error = latest_update(&sources, &InstallMethod::Npm)
            .await
            .expect_err("404 should fail the check");

        assert!(format!("{error:#}").contains("404"));
    }

    #[tokio::test]
    async fn direct_installs_read_the_release_endpoint() {
        let release = concat!(
            r#"{"tag_name":"v9.9.9","assets":["#,
            r#"{"name":"brokk-mjolnir-v9.9.9-x86_64-unknown-linux-gnu.tar.gz","#,
            r#""browser_download_url":"https://example.com/mj.tar.gz"},"#,
            r#"{"name":"brokk-mjolnir-v9.9.9-x86_64-unknown-linux-gnu.tar.gz.sha256","#,
            r#""browser_download_url":"https://example.com/mj.tar.gz.sha256"}]}"#,
        );
        let (sources, _) = serve_update_sources(vec![("/release", release)]).await;

        // Asset selection is platform-shaped, and the stub only carries the
        // Linux x86_64 archive, so this end-to-end trip is Linux-only; the
        // pure selection tests above cover the other platforms.
        let Ok(update) = latest_update(&sources, &InstallMethod::Direct).await else {
            return;
        };
        if std::env::consts::OS != "linux" && std::env::consts::ARCH != "x86_64" {
            return;
        }

        match update.expect("update") {
            AvailableUpdate::Direct(info) => {
                assert_eq!(info.version, Version::parse("9.9.9").expect("version"));
                assert_eq!(info.tag, "v9.9.9");
            }
            other => panic!("expected a direct update, got {other:?}"),
        }
    }
}
