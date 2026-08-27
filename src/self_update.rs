//! Install-aware startup update checks for `mj` and its bundled sidecars.

use std::ffi::OsString;
use std::io::{self, Cursor, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result};
use flate2::read::GzDecoder;
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};

const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/BrokkAi/mjolnir/releases/latest";
const NPM_LATEST_URL: &str = "https://registry.npmjs.org/@brokkai%2Fmjolnir/latest";
const HOMEBREW_FORMULA_URL: &str =
    "https://raw.githubusercontent.com/BrokkAi/homebrew-tap/main/Formula/mjolnir.rb";
const CARGO_INDEX_URL: &str = "https://index.crates.io/br/ok/brokk-mjolnir";
const BIN_NAME: &str = "mj";
const WINDOWS_BIN_NAME: &str = "mj.exe";
const VOICE_WORKER_NAME: &str = "mj-voice-worker";
const WINDOWS_VOICE_WORKER_NAME: &str = "mj-voice-worker.exe";
const NPM_MANAGED_ENV: &str = "MJOLNIR_MANAGED_BY_NPM";
const NPX_MANAGED_ENV: &str = "MJOLNIR_MANAGED_BY_NPX";
const HOMEBREW_MANAGED_ENV: &str = "MJOLNIR_MANAGED_BY_HOMEBREW";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupUpdateResult {
    Skipped,
    UpToDate,
    Notified,
    Declined,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum InstallMethod {
    Npm,
    Npx,
    Homebrew,
    Cargo { voice_worker: bool },
    Direct,
}

impl InstallMethod {
    fn current() -> Self {
        if std::env::var_os(NPX_MANAGED_ENV).is_some() {
            return Self::Npx;
        }
        if std::env::var_os(NPM_MANAGED_ENV).is_some() {
            return Self::Npm;
        }
        if std::env::var_os(HOMEBREW_MANAGED_ENV).is_some() {
            return Self::Homebrew;
        }

        std::env::current_exe().ok().map_or(Self::Direct, |exe| {
            install_method_from_exe(&exe, env!("CARGO_PKG_VERSION"))
        })
    }

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

#[derive(Debug, Clone, PartialEq, Eq)]
enum AvailableUpdate {
    Managed {
        version: Version,
        method: InstallMethod,
    },
    Direct(UpdateInfo),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UpdateInfo {
    version: Version,
    tag: String,
    asset: ReleaseAsset,
    checksum_asset: ReleaseAsset,
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

#[derive(Debug, Deserialize)]
struct CargoIndexEntry {
    vers: String,
    #[serde(default)]
    yanked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Platform {
    os_family: &'static str,
    arch: &'static str,
    rust_target: String,
}

pub async fn check_prompt_and_restart_if_accepted() -> Result<StartupUpdateResult> {
    if cfg!(debug_assertions) || !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Ok(StartupUpdateResult::Skipped);
    }

    let method = InstallMethod::current();
    let Some(update) = latest_update(&method).await? else {
        return Ok(StartupUpdateResult::UpToDate);
    };

    let AvailableUpdate::Direct(update) = update else {
        notify_managed_update(&update);
        return Ok(StartupUpdateResult::Notified);
    };

    if !prompt_for_update(&update)? {
        return Ok(StartupUpdateResult::Declined);
    }

    match download_apply_and_restart(&update).await {
        Ok(()) => unreachable!("restart replaces the current process on success"),
        Err(e) => {
            eprintln!("mj: upgrade failed: {e:#}");
            eprintln!("mj: continuing with {}", env!("CARGO_PKG_VERSION"));
            Ok(StartupUpdateResult::Skipped)
        }
    }
}

async fn latest_update(method: &InstallMethod) -> Result<Option<AvailableUpdate>> {
    let current = parse_version(env!("CARGO_PKG_VERSION"))
        .with_context(|| format!("parse current version {}", env!("CARGO_PKG_VERSION")))?;
    if *method == InstallMethod::Direct {
        let release = fetch_latest_release()
            .await
            .context("fetch latest mj release")?;
        return update_info_from_release(&release, &current, &current_platform()?)
            .map(|update| update.map(AvailableUpdate::Direct));
    }

    let latest = fetch_latest_managed_version(method).await?;
    Ok((latest > current).then(|| AvailableUpdate::Managed {
        version: latest,
        method: method.clone(),
    }))
}

async fn fetch_latest_release() -> Result<GitHubRelease> {
    let body = fetch_text(LATEST_RELEASE_URL).await?;
    serde_json::from_str(&body).context("parse release body")
}

async fn fetch_latest_managed_version(method: &InstallMethod) -> Result<Version> {
    match method {
        InstallMethod::Npm | InstallMethod::Npx => {
            let body = fetch_text(NPM_LATEST_URL)
                .await
                .context("fetch latest npm package")?;
            let latest: NpmLatest = serde_json::from_str(&body).context("parse npm metadata")?;
            parse_version(&latest.version).context("parse latest npm version")
        }
        InstallMethod::Homebrew => {
            let body = fetch_text(HOMEBREW_FORMULA_URL)
                .await
                .context("fetch Homebrew formula")?;
            parse_homebrew_formula_version(&body)
        }
        InstallMethod::Cargo { .. } => {
            let body = fetch_text(CARGO_INDEX_URL)
                .await
                .context("fetch crates.io index entry")?;
            parse_cargo_index_version(&body)
        }
        InstallMethod::Direct => anyhow::bail!("direct installs use GitHub release metadata"),
    }
}

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

fn notify_managed_update(update: &AvailableUpdate) {
    let AvailableUpdate::Managed { version, method } = update else {
        return;
    };
    println!(
        "{}",
        managed_update_notice(version, method, env!("CARGO_PKG_VERSION"))
            .expect("managed installs have an update command")
    );
}

fn managed_update_notice(
    version: &Version,
    method: &InstallMethod,
    current_version: &str,
) -> Option<String> {
    Some(format!(
        "mj {} is available through {}; current version is {}. Run: {}",
        version,
        method.channel_name(),
        current_version,
        method.update_command()?
    ))
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

fn prompt_for_update(update: &UpdateInfo) -> Result<bool> {
    print!(
        "mj {} is available; current version is {}. Upgrade now? [Y/n] ",
        update.tag,
        env!("CARGO_PKG_VERSION")
    );
    io::stdout().flush().context("flush update prompt")?;

    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .context("read update prompt answer")?;
    let answer = answer.trim();
    Ok(answer.is_empty() || matches!(answer, "y" | "Y" | "yes" | "YES"))
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
    match replacement {
        Replacement::RestartNow(restart_exe) => restart_current_process(&restart_exe),
        Replacement::DeferredRestart => std::process::exit(0),
    }
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
        anyhow::bail!(
            "checksum mismatch for {}: expected {}, got {}",
            update.asset.name,
            expected,
            actual
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
/// retires the voice worker must not strand older updaters the way removing
/// the bundled Anvil stranded mj <= 1.5.x. Only `mj` itself is mandatory.
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
            .context("read mj binary from archive")?;
        if bytes.is_empty() {
            anyhow::bail!("archive contained an empty {expected_name} binary");
        }
        return Ok(bytes);
    }
    anyhow::bail!("archive did not contain expected binary: {expected_name}");
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
            anyhow::bail!("archive contained an empty {expected_name} binary");
        }
        return Ok(bytes);
    }
    anyhow::bail!("archive did not contain expected binary: {expected_name}");
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
    if cfg!(windows) && target.exists() {
        std::fs::remove_file(&target)
            .with_context(|| format!("remove old {}", target.display()))?;
    }
    std::fs::rename(&tmp, &target)
        .with_context(|| format!("rename {} -> {}", tmp.display(), target.display()))?;
    #[cfg(unix)]
    strip_quarantine(&target);
    Ok(())
}

enum Replacement {
    #[cfg_attr(windows, allow(dead_code))]
    RestartNow(PathBuf),
    #[cfg_attr(not(windows), allow(dead_code))]
    DeferredRestart,
}

#[cfg(unix)]
fn replace_current_exe(current_exe: &Path, new_binary: &[u8]) -> Result<Replacement> {
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
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("chmod {}", tmp_path.display()))?;

    strip_quarantine(&tmp_path);
    std::fs::rename(&tmp_path, &target_exe)
        .with_context(|| format!("rename {} -> {}", tmp_path.display(), target_exe.display()))?;
    strip_quarantine(&target_exe);
    Ok(Replacement::RestartNow(target_exe))
}

#[cfg(windows)]
fn replace_current_exe(current_exe: &Path, new_binary: &[u8]) -> Result<Replacement> {
    let target_exe = current_exe
        .canonicalize()
        .with_context(|| format!("resolve executable target {}", current_exe.display()))?;
    let parent = target_exe
        .parent()
        .ok_or_else(|| anyhow::anyhow!("executable has no parent: {}", target_exe.display()))?;
    let tmp_path = parent.join(format!(
        ".{}.self-update.{}.tmp.exe",
        BIN_NAME,
        std::process::id()
    ));

    std::fs::write(&tmp_path, new_binary)
        .with_context(|| format!("write {}", tmp_path.display()))?;

    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let script = windows_replacement_script(std::process::id(), &tmp_path, &target_exe, &args);
    spawn_powershell_replacement(&script).context("spawn Windows self-update helper")?;
    Ok(Replacement::DeferredRestart)
}

#[cfg(not(any(unix, windows)))]
fn replace_current_exe(_current_exe: &Path, _new_binary: &[u8]) -> Result<Replacement> {
    anyhow::bail!("self-update replacement is only supported on Unix and Windows platforms")
}

#[cfg(any(windows, test))]
fn windows_replacement_script(
    parent_id: u32,
    source: &Path,
    target: &Path,
    restart_args: &[OsString],
) -> String {
    let restart_args = restart_args
        .iter()
        .map(|arg| powershell_single_quoted(&arg.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        r#"$ErrorActionPreference = 'Stop'
$parentId = {parent_id}
$source = {source}
$target = {target}
$restartArgs = @({restart_args})
for ($i = 0; $i -lt 600; $i++) {{
    try {{
        Wait-Process -Id $parentId -Timeout 1 -ErrorAction SilentlyContinue
    }} catch {{}}
    try {{
        Move-Item -LiteralPath $source -Destination $target -Force
        Start-Process -FilePath $target -ArgumentList $restartArgs
        exit 0
    }} catch {{
        Start-Sleep -Milliseconds 250
    }}
}}
exit 1
"#,
        source = powershell_single_quoted(&source.display().to_string()),
        target = powershell_single_quoted(&target.display().to_string()),
    )
}

#[cfg(any(windows, test))]
fn powershell_single_quoted(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(windows)]
fn spawn_powershell_replacement(script: &str) -> Result<()> {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x08000000;
    Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            script,
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .context("start powershell.exe")?;
    Ok(())
}

#[cfg(unix)]
fn strip_quarantine(path: &Path) {
    #[cfg(target_os = "macos")]
    {
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

fn restart_current_process(current_exe: &Path) -> Result<()> {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = Command::new(current_exe).args(args).exec();
        Err(err).with_context(|| format!("exec {}", current_exe.display()))
    }
    #[cfg(not(unix))]
    {
        Command::new(current_exe)
            .args(args)
            .spawn()
            .with_context(|| format!("restart {}", current_exe.display()))?;
        std::process::exit(0);
    }
}

fn install_method_from_exe(exe_path: &Path, current_version: &str) -> InstallMethod {
    if is_homebrew_executable(exe_path) {
        return InstallMethod::Homebrew;
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
    let components = exe_path
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .collect::<Vec<_>>();
    components
        .windows(2)
        .any(|pair| pair == ["Cellar", "mjolnir"])
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

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

    fn windows_x64() -> Platform {
        Platform {
            os_family: "windows",
            arch: "x86_64",
            rust_target: "x86_64-pc-windows-msvc".to_string(),
        }
    }

    fn android_arm() -> Platform {
        Platform {
            os_family: "android",
            arch: "aarch64",
            rust_target: "aarch64-linux-android".to_string(),
        }
    }

    fn make_tar_gz(file_name: &str, content: &[u8]) -> Vec<u8> {
        use flate2::Compression;
        use flate2::write::GzEncoder;

        let mut header = tar::Header::new_gnu();
        header.set_path(file_name).expect("path");
        header.set_size(content.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();

        let mut tar_bytes: Vec<u8> = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_bytes);
            builder.append(&header, content).expect("append");
            builder.finish().expect("finish");
        }

        let mut gz = GzEncoder::new(Vec::new(), Compression::default());
        gz.write_all(&tar_bytes).expect("gz write");
        gz.finish().expect("gz finish")
    }

    fn make_zip(file_name: &str, content: &[u8]) -> Vec<u8> {
        let cursor = Cursor::new(Vec::new());
        let mut writer = zip::ZipWriter::new(cursor);
        writer
            .start_file(file_name, zip::write::SimpleFileOptions::default())
            .expect("start file");
        writer.write_all(content).expect("zip write");
        writer.finish().expect("finish").into_inner()
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
    fn managed_update_notice_names_channel_version_and_command() {
        assert_eq!(
            managed_update_notice(
                &Version::parse("1.15.0").expect("version"),
                &InstallMethod::Homebrew,
                "1.14.1"
            )
            .as_deref(),
            Some(
                "mj 1.15.0 is available through Homebrew; current version is 1.14.1. Run: brew upgrade mjolnir"
            )
        );
    }

    #[test]
    fn parses_homebrew_formula_version() {
        let formula = r#"
class Mjolnir < Formula
  version "1.15.2"
end
"#;
        assert_eq!(
            parse_homebrew_formula_version(formula).expect("version"),
            Version::parse("1.15.2").expect("semver")
        );
    }

    #[test]
    fn cargo_index_uses_latest_non_yanked_version() {
        let index = concat!(
            r#"{"vers":"1.14.0","yanked":false}"#,
            "\n",
            r#"{"vers":"1.15.0","yanked":true}"#,
            "\n",
            r#"{"vers":"1.14.2","yanked":false}"#,
            "\n",
        );
        assert_eq!(
            parse_cargo_index_version(index).expect("version"),
            Version::parse("1.14.2").expect("semver")
        );
    }

    #[test]
    fn detects_homebrew_cellar_on_macos_and_linux() {
        assert_eq!(
            install_method_from_exe(
                Path::new("/opt/homebrew/Cellar/mjolnir/1.14.1/libexec/mj"),
                "1.14.1"
            ),
            InstallMethod::Homebrew
        );
        assert_eq!(
            install_method_from_exe(
                Path::new("/home/linuxbrew/.linuxbrew/Cellar/mjolnir/1.14.1/libexec/mj"),
                "1.14.1"
            ),
            InstallMethod::Homebrew
        );
    }

    #[test]
    fn detects_cargo_install_and_installed_voice_worker() {
        let root = tempfile::tempdir().expect("tempdir");
        let bin_dir = root.path().join("bin");
        std::fs::create_dir(&bin_dir).expect("bin dir");
        let executable = bin_dir.join(if cfg!(windows) {
            WINDOWS_BIN_NAME
        } else {
            BIN_NAME
        });
        std::fs::write(&executable, b"mj").expect("executable");
        std::fs::write(
            root.path().join(".crates.toml"),
            r#"[v1]
"brokk-mjolnir 1.14.1 (registry+https://github.com/rust-lang/crates.io-index)" = ["mj"]
"brokk-mj-voice-worker 1.14.1 (registry+https://github.com/rust-lang/crates.io-index)" = ["mj-voice-worker"]
"#,
        )
        .expect("manifest");

        assert_eq!(
            install_method_from_exe(&executable, "1.14.1"),
            InstallMethod::Cargo { voice_worker: true }
        );
    }

    #[test]
    fn cargo_metadata_must_match_the_running_version() {
        let root = tempfile::tempdir().expect("tempdir");
        let bin_dir = root.path().join("bin");
        std::fs::create_dir(&bin_dir).expect("bin dir");
        let executable = bin_dir.join(if cfg!(windows) {
            WINDOWS_BIN_NAME
        } else {
            BIN_NAME
        });
        std::fs::write(&executable, b"mj").expect("executable");
        std::fs::write(
            root.path().join(".crates2.json"),
            r#"{"installs":{"brokk-mjolnir 1.14.0 (registry+https://github.com/rust-lang/crates.io-index)":{"bins":["mj"]}}}"#,
        )
        .expect("manifest");

        assert_eq!(
            install_method_from_exe(&executable, "1.14.1"),
            InstallMethod::Direct
        );
    }

    #[test]
    fn release_newer_than_current_returns_update_info() {
        let release = GitHubRelease {
            tag_name: "v0.5.0".to_string(),
            assets: vec![
                asset("brokk-mjolnir-v0.5.0-x86_64-unknown-linux-gnu.tar.gz"),
                asset("brokk-mjolnir-v0.5.0-x86_64-unknown-linux-gnu.tar.gz.sha256"),
            ],
        };

        let update = update_info_from_release(
            &release,
            &Version::parse("0.4.2").expect("version"),
            &linux_x64(),
        )
        .expect("update info")
        .expect("update");

        assert_eq!(update.version, Version::parse("0.5.0").expect("version"));
        assert_eq!(
            update.asset.name,
            "brokk-mjolnir-v0.5.0-x86_64-unknown-linux-gnu.tar.gz"
        );
        assert_eq!(
            update.checksum_asset.name,
            "brokk-mjolnir-v0.5.0-x86_64-unknown-linux-gnu.tar.gz.sha256"
        );
    }

    #[test]
    fn release_newer_than_current_requires_checksum_asset() {
        let release = GitHubRelease {
            tag_name: "v0.5.0".to_string(),
            assets: vec![asset(
                "brokk-mjolnir-v0.5.0-x86_64-unknown-linux-gnu.tar.gz",
            )],
        };

        let err = update_info_from_release(
            &release,
            &Version::parse("0.4.2").expect("version"),
            &linux_x64(),
        )
        .expect_err("missing checksum should fail");

        assert!(
            err.to_string()
                .contains("missing required checksum asset brokk-mjolnir-v0.5.0-x86_64-unknown-linux-gnu.tar.gz.sha256")
        );
    }

    #[test]
    fn release_not_newer_returns_none() {
        let release = GitHubRelease {
            tag_name: "v0.4.2".to_string(),
            assets: vec![asset(
                "brokk-mjolnir-v0.4.2-x86_64-unknown-linux-gnu.tar.gz",
            )],
        };

        let update = update_info_from_release(
            &release,
            &Version::parse("0.4.2").expect("version"),
            &linux_x64(),
        )
        .expect("update info");

        assert!(update.is_none());
    }

    #[test]
    fn macos_prefers_universal_asset() {
        let assets = vec![
            asset("brokk-mjolnir-v0.5.0-aarch64-apple-darwin.tar.gz"),
            asset("brokk-mjolnir-v0.5.0-universal-apple-darwin.tar.gz"),
        ];

        let selected = select_mj_asset(&assets, &mac_arm()).expect("select");

        assert_eq!(
            selected.name,
            "brokk-mjolnir-v0.5.0-universal-apple-darwin.tar.gz"
        );
    }

    #[test]
    fn linux_selects_target_asset() {
        let assets = vec![
            asset("brokk-mjolnir-v0.5.0-aarch64-unknown-linux-gnu.tar.gz"),
            asset("brokk-mjolnir-v0.5.0-x86_64-unknown-linux-gnu.tar.gz"),
        ];

        let selected = select_mj_asset(&assets, &linux_x64()).expect("select");

        assert_eq!(
            selected.name,
            "brokk-mjolnir-v0.5.0-x86_64-unknown-linux-gnu.tar.gz"
        );
    }

    #[test]
    fn windows_selects_zip_asset() {
        let assets = vec![
            asset("brokk-mjolnir-v0.5.0-x86_64-unknown-linux-gnu.tar.gz"),
            asset("brokk-mjolnir-v0.5.0-x86_64-pc-windows-msvc.zip"),
        ];

        let selected = select_mj_asset(&assets, &windows_x64()).expect("select");

        assert_eq!(
            selected.name,
            "brokk-mjolnir-v0.5.0-x86_64-pc-windows-msvc.zip"
        );
    }

    #[test]
    fn android_selects_android_asset() {
        let assets = vec![
            asset("brokk-mjolnir-v0.5.0-aarch64-unknown-linux-gnu.tar.gz"),
            asset("brokk-mjolnir-v0.5.0-aarch64-linux-android.tar.gz"),
        ];

        let selected = select_mj_asset(&assets, &android_arm()).expect("select");

        assert_eq!(
            selected.name,
            "brokk-mjolnir-v0.5.0-aarch64-linux-android.tar.gz"
        );
    }

    #[test]
    fn extract_mj_binary_finds_nested_binary() {
        let archive = make_tar_gz("brokk-mjolnir/bin/mj", b"binary bytes");

        let binary = extract_mj_binary(
            "brokk-mjolnir-v0.5.0-x86_64-unknown-linux-gnu.tar.gz",
            &archive,
        )
        .expect("extract");

        assert_eq!(binary, b"binary bytes");
    }

    #[test]
    fn extract_mj_binary_finds_windows_zip_binary() {
        let archive = make_zip("brokk-mjolnir/mj.exe", b"windows binary bytes");

        let binary = extract_mj_binary("brokk-mjolnir-v0.5.0-x86_64-pc-windows-msvc.zip", &archive)
            .expect("extract");

        assert_eq!(binary, b"windows binary bytes");
    }

    #[test]
    fn extract_voice_worker_finds_unix_sidecar() {
        let archive = make_tar_gz("brokk-mjolnir/bin/mj-voice-worker", b"voice worker bytes");

        let binary = extract_voice_worker_binary(
            "brokk-mjolnir-v0.5.0-x86_64-unknown-linux-gnu.tar.gz",
            &archive,
        )
        .expect("extract voice worker");

        assert_eq!(binary, b"voice worker bytes");
    }

    #[test]
    fn extract_voice_worker_finds_windows_sidecar() {
        let archive = make_zip(
            "brokk-mjolnir/mj-voice-worker.exe",
            b"windows voice worker bytes",
        );

        let binary = extract_voice_worker_binary(
            "brokk-mjolnir-v0.5.0-x86_64-pc-windows-msvc.zip",
            &archive,
        )
        .expect("extract voice worker");

        assert_eq!(binary, b"windows voice worker bytes");
    }

    #[test]
    fn optional_voice_worker_extracts_when_bundled() {
        let archive = make_tar_gz("brokk-mjolnir/mj-voice-worker", b"voice worker bytes");

        let binary = extract_optional_voice_worker(
            "brokk-mjolnir-v0.5.0-x86_64-unknown-linux-gnu.tar.gz",
            &archive,
        )
        .expect("bundled voice worker");

        assert_eq!(binary, b"voice worker bytes");
    }

    #[test]
    fn optional_voice_worker_tolerates_archives_without_one() {
        let archive = make_tar_gz("brokk-mjolnir/mj", b"binary bytes");

        let binary = extract_optional_voice_worker(
            "brokk-mjolnir-v0.5.0-x86_64-unknown-linux-gnu.tar.gz",
            &archive,
        );

        assert!(binary.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn install_voice_worker_writes_executable_beside_mj() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let mj = dir.path().join("mj");
        std::fs::write(&mj, b"main").expect("write mj");
        install_voice_worker(&mj, b"worker").expect("install worker");

        let worker = dir.path().join(VOICE_WORKER_NAME);
        assert_eq!(std::fs::read(&worker).expect("read worker"), b"worker");
        assert_ne!(
            std::fs::metadata(worker)
                .expect("worker metadata")
                .permissions()
                .mode()
                & 0o111,
            0
        );
    }

    #[test]
    fn checksum_hex_matches_known_value() {
        assert_eq!(
            sha256_hex(b"mj"),
            "a3f9e2bcd804ec65d1ea4fc63a74e7f02a08e63ffd0b803a8f250236f5602405"
        );
    }

    #[cfg(unix)]
    #[test]
    fn replace_current_exe_writes_next_to_resolved_target() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("mj");
        std::fs::write(&target, b"old").expect("write target");
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))
            .expect("chmod target");

        let Replacement::RestartNow(replaced) =
            replace_current_exe(&target, b"new").expect("replace")
        else {
            panic!("expected immediate restart");
        };

        assert_eq!(replaced, target.canonicalize().expect("canonical target"));
        assert_eq!(std::fs::read(&target).expect("read target"), b"new");
        assert_eq!(
            std::fs::metadata(&target)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
    }

    #[cfg(unix)]
    #[test]
    fn replace_current_exe_resolves_symlink_before_replacing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("mj-real");
        let link = dir.path().join("mj");
        std::fs::write(&target, b"old").expect("write target");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");

        let Replacement::RestartNow(replaced) =
            replace_current_exe(&link, b"new").expect("replace")
        else {
            panic!("expected immediate restart");
        };

        assert_eq!(replaced, target.canonicalize().expect("canonical target"));
        assert_eq!(std::fs::read(&target).expect("read target"), b"new");
        assert_eq!(std::fs::read_link(&link).expect("read link"), target);
    }

    #[test]
    fn windows_replacement_script_waits_moves_and_restarts() {
        let script = windows_replacement_script(
            42,
            Path::new(r"C:\Program Files\mj\.mj.self-update.tmp.exe"),
            Path::new(r"C:\Program Files\mj\mj.exe"),
            &[OsString::from("say hi"), OsString::from("it'll work")],
        );

        assert!(script.contains("Wait-Process -Id $parentId -Timeout 1"));
        assert!(script.contains("Move-Item -LiteralPath $source -Destination $target -Force"));
        assert!(script.contains("Start-Process -FilePath $target -ArgumentList $restartArgs"));
        assert!(script.contains("'say hi'"));
        assert!(script.contains("'it''ll work'"));
    }
}
