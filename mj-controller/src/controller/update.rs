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
use std::io::{self, BufRead, Cursor, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use flate2::read::GzDecoder;
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};

const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/BrokkAi/mjolnir/releases/latest";
const NPM_LATEST_URL: &str = "https://registry.npmjs.org/@brokkai%2Fmjolnir/latest";
const HOMEBREW_FORMULA_URL: &str =
    "https://raw.githubusercontent.com/BrokkAi/homebrew-tap/main/Formula/mjolnir.rb";
const BIN_NAME: &str = "mj";
const WINDOWS_BIN_NAME: &str = "mj.exe";
const VOICE_WORKER_NAME: &str = "mj-voice-worker";
const NPM_MANAGED_ENV: &str = "MJOLNIR_MANAGED_BY_NPM";
const NPX_MANAGED_ENV: &str = "MJOLNIR_MANAGED_BY_NPX";
const HOMEBREW_MANAGED_ENV: &str = "MJOLNIR_MANAGED_BY_HOMEBREW";
const NO_UPDATE_CHECK_ENV: &str = "MJOLNIR_NO_UPDATE_CHECK";

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
///   update the controller and every bundled application helper, and re-exec.
/// - npx and cargo installs are notice-only.
///
/// Runs on every interactive startup before the daemon or dashboard starts.
/// The version fetch is time-boxed, and interactive package-manager runs share
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
                let upgraded =
                    run_managed_upgrade(&version, &method).and_then(restart_current_process);
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

    read_update_answer(&mut io::stdin().lock())
}

fn read_update_answer(input: &mut impl BufRead) -> Result<bool> {
    let mut answer = String::new();
    let bytes_read = input
        .read_line(&mut answer)
        .context("read update prompt answer")?;
    Ok(bytes_read != 0 && prompt_answer_is_yes(&answer))
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
fn run_managed_upgrade(version: &Version, method: &InstallMethod) -> Result<RestartTarget> {
    // npm moves and unlinks the old package. Resolve this before the upgrade,
    // while current_exe still identifies the installation's stable path.
    let current_exe = std::env::current_exe().context("resolve current executable")?;
    let restart = managed_restart_target(method, &current_exe)?;
    match method {
        InstallMethod::Npm => {
            println!("mj: running npm install -g @brokkai/mjolnir@latest");
            let status = mj_core::subprocess::run_inherited(&mut npm_upgrade_command())
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
            let status = mj_core::subprocess::run_inherited(&mut brew_update_command())
                .context("run brew update")?;
            ensure!(status.success(), "brew update exited with {status}");
            println!("mj: running brew upgrade mjolnir");
            let status = mj_core::subprocess::run_inherited(&mut brew_upgrade_command())
                .context("run brew upgrade mjolnir")?;
            ensure!(status.success(), "brew upgrade exited with {status}");
        }
        other => bail!("{other:?} installs do not support delegated upgrades"),
    }
    println!("mj: upgraded to {version}; restarting");
    Ok(restart)
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

    let current_exe = std::env::current_exe().context("resolve current executable")?;
    let replacement = install_release_archive(&current_exe, &update.asset.name, &archive)
        .context("install release bundle")?;

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

/// Extract the complete application bundle before changing any installed file.
/// Companions can be retired in future releases, but mj itself is mandatory.
fn install_release_archive(
    current_exe: &Path,
    archive_name: &str,
    archive_bytes: &[u8],
) -> Result<PathBuf> {
    ensure!(
        cfg!(unix),
        "self-update replacement is only supported on Unix platforms"
    );
    let target_exe = current_exe
        .canonicalize()
        .with_context(|| format!("resolve executable target {}", current_exe.display()))?;
    let parent = target_exe
        .parent()
        .ok_or_else(|| anyhow::anyhow!("executable has no parent: {}", target_exe.display()))?;
    // Keep staging on the destination filesystem so each replacement is an
    // atomic rename, including binaries that are currently running.
    let staging = tempfile::Builder::new()
        .prefix(".mj-self-update-")
        .tempdir_in(parent)
        .context("create update staging directory")?;
    let mut binaries = stage_release_archive(archive_name, archive_bytes, staging.path())?;
    let executable_name = if archive_name.ends_with(".zip") {
        WINDOWS_BIN_NAME
    } else {
        BIN_NAME
    };
    ensure!(
        staging.path().join(executable_name).is_file(),
        "archive did not contain expected binary: {executable_name}"
    );
    // Replace the controller last, once every packaged companion is installed.
    binaries.sort_by_key(|path| path.file_name() == Some(executable_name.as_ref()));
    strip_quarantine(staging.path());
    for binary in binaries {
        let name = binary
            .file_name()
            .context("staged binary has no file name")?;
        let target = if name == executable_name {
            target_exe.clone()
        } else {
            parent.join(name)
        };
        std::fs::rename(&binary, &target)
            .with_context(|| format!("install {}", target.display()))?;
    }
    Ok(target_exe)
}

fn stage_release_archive(
    archive_name: &str,
    archive_bytes: &[u8],
    directory: &Path,
) -> Result<Vec<PathBuf>> {
    let mut binaries = Vec::new();
    if archive_name.ends_with(".zip") {
        let mut archive =
            zip::ZipArchive::new(Cursor::new(archive_bytes)).context("open zip archive")?;
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index).context("read zip entry")?;
            let path = entry.enclosed_name().ok_or_else(|| {
                anyhow::anyhow!("zip entry escapes destination: {}", entry.name())
            })?;
            let is_file = entry.is_file() && !entry.is_symlink();
            if let Some(binary) = stage_archive_binary(directory, &path, is_file, &mut entry)? {
                binaries.push(binary);
            }
        }
    } else {
        let mut archive = tar::Archive::new(GzDecoder::new(archive_bytes));
        for entry in archive.entries().context("read tar entries")? {
            let mut entry = entry.context("read tar entry")?;
            let path = entry.path().context("read tar entry path")?.into_owned();
            let is_file = entry.header().entry_type().is_file();
            if let Some(binary) = stage_archive_binary(directory, &path, is_file, &mut entry)? {
                binaries.push(binary);
            }
        }
    }
    Ok(binaries)
}

fn stage_archive_binary(
    directory: &Path,
    path: &Path,
    is_file: bool,
    mut contents: impl Read,
) -> Result<Option<PathBuf>> {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return Ok(None);
    };
    let stem = name.strip_suffix(".exe").unwrap_or(name);
    if !matches!(
        stem,
        BIN_NAME | "mj-desktop" | VOICE_WORKER_NAME | "mj-worker"
    ) && !stem.starts_with("mj-worker-")
    {
        return Ok(None);
    }
    ensure!(
        is_file,
        "archive binary is not a regular file: {}",
        path.display()
    );
    let target = directory.join(name);
    let mut output = std::fs::File::create_new(&target)
        .with_context(|| format!("stage {name}; each binary must appear only once"))?;
    let size = io::copy(&mut contents, &mut output).with_context(|| format!("extract {name}"))?;
    ensure!(size != 0, "archive contained an empty {name} binary");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        output
            .set_permissions(std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("chmod {name}"))?;
    }
    Ok(Some(target))
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
mod tests;
