//! Exact, target-local harness installations, including container fallbacks.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::process::Command;
use std::time::{Duration, Instant};

use crate::config::{ExecutionPolicy, HarnessKind};
use crate::harness_runtime::{
    GROK_VERSION, HarnessPin, KIMI_VERSION, LEASE_FILE, MANIFEST_FILE, ManagedHarnessManifest,
    NODE_VERSION, harness_uses_node_runtime, managed_harness_cache_root,
    managed_harness_install_dir, managed_harness_manifest_matches, managed_node_cache_root,
    npm_bridge, pin,
};
use crate::worker_launch::HarnessRuntimePolicy;
use anyhow::{Context, Result, anyhow, bail};

const INSTALL_LOCK_FILE: &str = ".install.lock";
const INSTALL_LOCK_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const HARNESS_COMMAND_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const MANAGED_INSTALL_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const NODE_PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_NODE_DIST_URL: &str = "https://nodejs.org/dist";
const ARTIFACT_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const MAX_ARTIFACT_BYTES: u64 = 512 * 1024 * 1024;
const MAX_ARTIFACT_REDIRECTS: usize = 10;

const CODEX_PACKAGE_JSON: &[u8] = include_bytes!("../assets/harnesses/codex/package.json");
const CODEX_PACKAGE_LOCK: &[u8] = include_bytes!("../assets/harnesses/codex/package-lock.json");
const CLAUDE_PACKAGE_JSON: &[u8] = include_bytes!("../assets/harnesses/claude/package.json");
const CLAUDE_PACKAGE_LOCK: &[u8] = include_bytes!("../assets/harnesses/claude/package-lock.json");
const MIMO_PACKAGE_JSON: &[u8] = include_bytes!("../assets/harnesses/mimo/package.json");
const MIMO_PACKAGE_LOCK: &[u8] = include_bytes!("../assets/harnesses/mimo/package-lock.json");

#[derive(Debug)]
pub struct ManagedHarness {
    pub command: PathBuf,
    pub args: Vec<String>,
    pub environment: BTreeMap<String, String>,
    /// The installed harness executable itself. For an adapter that is launched
    /// through a shell bootstrap, this is the native harness or ACP binary the
    /// bootstrap would exec; login fallbacks use it directly.
    pub entrypoint: PathBuf,
    /// The harness command-line program installed beside an ACP adapter.
    pub cli_entrypoint: PathBuf,
    pub lease_path: PathBuf,
    pub cache_root: PathBuf,
    _lease: File,
    _managed_node: Option<ManagedNodeRuntime>,
}

/// A shared managed Node.js installation leased for the lifetime of an ACP
/// process. The lease prevents a corrupt-cache repair from replacing files
/// beneath a live session.
#[derive(Debug)]
pub struct ManagedNodeRuntime {
    path: PathBuf,
    lease_path: PathBuf,
    _lease: File,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ManagedNodeHealth {
    Missing,
    Usable,
    Unusable(String),
}

impl ManagedNodeHealth {
    fn unusable_reason(&self) -> Option<&str> {
        match self {
            Self::Unusable(reason) => Some(reason),
            Self::Missing | Self::Usable => None,
        }
    }
}

impl ManagedNodeRuntime {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn lease_path(&self) -> &Path {
        &self.lease_path
    }
}

pub fn spawn_gc_on(
    runtime: &tokio::runtime::Handle,
    root: PathBuf,
    harness: HarnessKind,
) -> tokio::task::JoinHandle<()> {
    runtime.spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
        interval.tick().await;
        loop {
            interval.tick().await;
            let root = root.clone();
            let result = tokio::task::spawn_blocking(move || {
                gc_harness_root(&root.join(harness.id()), pin(harness).install_id)
            })
            .await;
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    tracing::warn!(%error, harness = harness.id(), "managed harness garbage collection failed");
                }
                Err(error) => {
                    tracing::error!(%error, harness = harness.id(), "managed harness garbage collection task failed");
                    return;
                }
            }
        }
    })
}

pub async fn resolve(
    runtime: HarnessRuntimePolicy,
    harness: HarnessKind,
    execution_policy: ExecutionPolicy,
    environment: &BTreeMap<String, String>,
) -> Result<Option<ManagedHarness>> {
    if runtime == HarnessRuntimePolicy::Ambient {
        return Ok(None);
    }
    // DSH has no managed installation: the npm package bundles the same
    // release binaries, so the launch description the controller already
    // provides resolves a compatible target `dsh-acp` or fetches the pinned
    // package through npx.
    if harness == HarnessKind::Dsh {
        return Ok(None);
    }
    let environment = crate::login_environment::with_overrides(environment).await?;
    let root = managed_harness_cache_root(
        environment.get("XDG_CACHE_HOME").map(OsStr::new),
        environment.get("HOME").map(OsStr::new),
    )?;
    resolve_at_async(&root, harness, execution_policy, &environment)
        .await
        .map(Some)
}

/// Launch description for a harness [`resolve`] does not manage, for call
/// sites the controller's bridge launch does not reach (the model discovery
/// probe). Only DSH: [`crate::harness_runtime::dsh_acp_launch_script`]
/// resolves a compatible target `dsh-acp` or the pinned npm package, so there
/// is no install to lease and nothing to cache.
pub fn unmanaged_launch(harness: HarnessKind) -> Option<(PathBuf, Vec<String>)> {
    match harness {
        HarnessKind::Dsh => Some((
            PathBuf::from("sh"),
            vec![
                "-c".to_owned(),
                crate::harness_runtime::dsh_acp_launch_script(),
            ],
        )),
        _ => None,
    }
}

#[cfg(test)]
fn resolve_at(
    root: &Path,
    harness: HarnessKind,
    execution_policy: ExecutionPolicy,
    environment: &BTreeMap<String, String>,
) -> Result<ManagedHarness> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build managed harness test runtime")?
        .block_on(resolve_at_async(
            root,
            harness,
            execution_policy,
            environment,
        ))
}

/// Install or reuse the exact managed harness for `kind` from a fully
/// resolved environment.
///
/// Unlike [`resolve`], this does not start the account's login shell. Callers
/// that already hold a resolved environment (notably `mj login`, which may run
/// before any daemon and under a shell that cannot be started non-interactively)
/// use this directly.
pub async fn resolve_with_environment(
    harness: HarnessKind,
    execution_policy: ExecutionPolicy,
    environment: &BTreeMap<String, String>,
) -> Result<ManagedHarness> {
    if harness == HarnessKind::Dsh {
        bail!(
            "{} has no managed installation; launch it through its npx script",
            harness.display_name()
        );
    }
    let root = managed_harness_cache_root(
        environment.get("XDG_CACHE_HOME").map(OsStr::new),
        environment.get("HOME").map(OsStr::new),
    )?;
    resolve_at_async(&root, harness, execution_policy, environment).await
}

async fn resolve_at_async(
    root: &Path,
    harness: HarnessKind,
    execution_policy: ExecutionPolicy,
    environment: &BTreeMap<String, String>,
) -> Result<ManagedHarness> {
    let mut environment = environment.clone();
    let managed_node = if harness_uses_node_runtime(harness) {
        ensure_node_runtime(&mut environment).await?
    } else {
        None
    };
    let selected = pin(harness);
    let harness_root = root.join(harness.id());
    std::fs::create_dir_all(&harness_root)
        .with_context(|| format!("create managed {} cache", harness.display_name()))?;
    let install_lock = open_lock(&harness_root.join(INSTALL_LOCK_FILE))?;
    lock_file(&install_lock, false, "managed harness installer").await?;
    remove_abandoned_staging(&harness_root)?;

    let install = managed_harness_install_dir(root, harness);
    if !complete_install(&install, harness, selected)? {
        if install.exists() {
            let lease = open_lock(&install.join(LEASE_FILE))?;
            match lease.try_lock() {
                Ok(()) => {}
                Err(std::fs::TryLockError::WouldBlock) => bail!(
                    "managed harness repair deferred: {} is still in use",
                    install.display()
                ),
                Err(std::fs::TryLockError::Error(error)) => {
                    return Err(error).context("lock invalid managed harness for repair");
                }
            }
            std::fs::remove_dir_all(&install).with_context(|| {
                format!("remove incomplete managed harness {}", install.display())
            })?;
        }
        install_into(&harness_root, &install, harness, selected, &environment).await?;
    }
    gc_harness_root(&harness_root, selected.install_id)?;

    let lease_path = install.join(LEASE_FILE);
    let lease = open_lock(&lease_path)?;
    lock_file(&lease, true, "managed harness lease").await?;
    drop(install_lock);

    let mut launch_environment = BTreeMap::new();
    if managed_node.is_some() {
        launch_environment.insert(
            "PATH".into(),
            environment
                .get("PATH")
                .cloned()
                .context("managed Node runtime did not produce PATH")?,
        );
    }
    // The managed Muse install provides only the native server; the adapter
    // itself comes from its pinned npm package.
    if harness == HarnessKind::Muse {
        launch_environment.insert(
            "MUSE_CLI".into(),
            install
                .join(selected.entrypoint)
                .to_string_lossy()
                .into_owned(),
        );
    }
    if harness == HarnessKind::Codex {
        launch_environment.insert(
            "CODEX_PATH".to_owned(),
            install
                .join("node_modules/@openai/codex/bin/codex.js")
                .to_string_lossy()
                .into_owned(),
        );
    }
    // The bridge's SDK bundles an older CLI; select our separately pinned Code.
    if let (HarnessKind::Claude, Some(entry)) = (harness, harness.extra_managed_entrypoint()) {
        launch_environment.insert(
            "CLAUDE_CODE_EXECUTABLE".into(),
            install.join(entry).to_string_lossy().into_owned(),
        );
    }
    // The pinned installation is not the release channel, so its downloader
    // must never fetch a replacement under a running session.
    if harness == HarnessKind::OpenCode {
        launch_environment.insert("OPENCODE_DISABLE_AUTOUPDATE".into(), "1".into());
    }
    if harness == HarnessKind::Mimo {
        launch_environment.insert("MIMOCODE_DISABLE_AUTOUPDATE".into(), "1".into());
    }
    // Muse launches its adapter the way the other npm bridges do: the target's
    // own `muse-acp` when present, otherwise npx fetches the pinned package.
    let (command, args) = if harness == HarnessKind::Muse {
        let bridge = npm_bridge(harness).expect("Muse installs its adapter from npm");
        (
            PathBuf::from("sh"),
            vec!["-c".to_owned(), bridge.bootstrap_script()],
        )
    } else if harness == HarnessKind::OpenCode {
        // OpenCode imports the session's staged logins before its ACP server
        // starts; the script replaces itself with the pinned executable.
        (
            PathBuf::from("sh"),
            vec![
                "-c".to_owned(),
                format!(
                    "opencode=\"$1\"; {}",
                    crate::harness_runtime::opencode_acp_script()
                ),
                "mj-opencode".to_owned(),
                install
                    .join(selected.entrypoint)
                    .to_string_lossy()
                    .into_owned(),
            ],
        )
    } else {
        (
            install.join(selected.entrypoint),
            harness
                .bridge_args(execution_policy)
                .into_iter()
                .map(str::to_owned)
                .collect(),
        )
    };
    Ok(ManagedHarness {
        command,
        args,
        environment: launch_environment,
        entrypoint: install.join(selected.entrypoint),
        cli_entrypoint: install.join(selected.cli_entrypoint),
        lease_path,
        cache_root: root.to_path_buf(),
        _lease: lease,
        _managed_node: managed_node,
    })
}

async fn lock_file(file: &File, shared: bool, purpose: &str) -> Result<()> {
    let started = Instant::now();
    loop {
        let result = if shared {
            file.try_lock_shared()
        } else {
            file.try_lock()
        };
        match result {
            Ok(()) => return Ok(()),
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(error).with_context(|| format!("lock {purpose}"));
            }
            Err(std::fs::TryLockError::WouldBlock) => {}
        }
        if started.elapsed() >= INSTALL_LOCK_TIMEOUT {
            bail!(
                "timed out after {} seconds waiting to lock {purpose}",
                INSTALL_LOCK_TIMEOUT.as_secs()
            );
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn remove_abandoned_staging(harness_root: &Path) -> Result<()> {
    for entry in std::fs::read_dir(harness_root)
        .with_context(|| format!("scan managed harness staging in {}", harness_root.display()))?
    {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !name.starts_with(".install-") || !entry.file_type()?.is_dir() {
            continue;
        }
        std::fs::remove_dir_all(entry.path()).with_context(|| {
            format!(
                "remove abandoned harness staging {}",
                entry.path().display()
            )
        })?;
    }
    Ok(())
}

async fn install_into(
    harness_root: &Path,
    final_path: &Path,
    harness: HarnessKind,
    selected: HarnessPin,
    environment: &BTreeMap<String, String>,
) -> Result<()> {
    let staging = tempfile::Builder::new()
        .prefix(".install-")
        .tempdir_in(harness_root)
        .with_context(|| format!("create staging directory in {}", harness_root.display()))?;
    match harness {
        HarnessKind::Codex => {
            install_npm(
                staging.path(),
                CODEX_PACKAGE_JSON,
                CODEX_PACKAGE_LOCK,
                harness,
                environment,
            )
            .await?
        }
        HarnessKind::Claude => {
            install_npm(
                staging.path(),
                CLAUDE_PACKAGE_JSON,
                CLAUDE_PACKAGE_LOCK,
                harness,
                environment,
            )
            .await?
        }
        HarnessKind::Kimi => install_kimi(staging.path(), environment).await?,
        HarnessKind::Grok => install_grok(staging.path(), environment).await?,
        HarnessKind::Muse => install_muse_native(staging.path(), environment).await?,
        HarnessKind::OpenCode => install_opencode(staging.path(), environment).await?,
        HarnessKind::Mimo => {
            install_npm(
                staging.path(),
                MIMO_PACKAGE_JSON,
                MIMO_PACKAGE_LOCK,
                harness,
                environment,
            )
            .await?
        }
        HarnessKind::Dsh => {
            unreachable!("DSH has no managed installation; resolve returns before installing")
        }
    }
    relativize_internal_links(staging.path(), staging.path())?;
    validate_entrypoint(staging.path(), selected, harness)?;
    open_lock(&staging.path().join(LEASE_FILE))?;
    let manifest = ManagedHarnessManifest::for_install(harness, selected.install_id);
    let body = serde_json::to_vec_pretty(&manifest)?;
    let staging_path = staging.keep();
    match std::fs::rename(&staging_path, final_path) {
        Ok(()) => {
            validate_entrypoint(final_path, selected, harness)
                .context("validate relocated managed harness")?;
            crate::config::atomic_write(&final_path.join(MANIFEST_FILE), &body)
        }
        Err(error) if final_path.exists() => {
            std::fs::remove_dir_all(&staging_path).with_context(|| {
                format!("remove losing harness staging {}", staging_path.display())
            })?;
            if complete_install(final_path, harness, selected)? {
                Ok(())
            } else {
                Err(error)
                    .with_context(|| format!("publish managed harness {}", final_path.display()))
            }
        }
        Err(error) => {
            Err(error).with_context(|| format!("publish managed harness {}", final_path.display()))
        }
    }
}

/// Installer-generated absolute links inside staging must survive its rename.
/// Do not follow directory links or rewrite links to external tools.
fn relativize_internal_links(root: &Path, directory: &Path) -> Result<()> {
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let kind = entry.file_type()?;
        if kind.is_dir() {
            relativize_internal_links(root, &path)?;
        } else if kind.is_symlink() {
            let target = std::fs::read_link(&path)?;
            if target.is_absolute()
                && let Ok(target) = target.strip_prefix(root)
            {
                let parent = directory.strip_prefix(root)?;
                let mut relative = PathBuf::new();
                for _ in parent.components() {
                    relative.push("..");
                }
                relative.push(target);
                std::fs::remove_file(&path)?;
                std::os::unix::fs::symlink(relative, &path)
                    .with_context(|| format!("relocate managed harness link {}", path.display()))?;
            }
        }
    }
    Ok(())
}

async fn install_npm(
    staging: &Path,
    package_json: &[u8],
    package_lock: &[u8],
    harness: HarnessKind,
    environment: &BTreeMap<String, String>,
) -> Result<()> {
    std::fs::write(staging.join("package.json"), package_json)
        .context("write managed harness package.json")?;
    std::fs::write(staging.join("package-lock.json"), package_lock)
        .context("write managed harness package-lock.json")?;
    let mut command = tokio::process::Command::new("npm");
    command
        .args(["ci", "--omit=dev", "--no-audit", "--no-fund"])
        .current_dir(staging)
        .env_clear()
        .envs(environment);
    run_bounded_checked(
        &mut command,
        "install exact managed npm harness",
        MANAGED_INSTALL_TIMEOUT,
    )
    .await?;
    verify_install_scripts_ran(staging, harness, environment).await
}

/// npm 12 and later refuse dependency install scripts unless the package is
/// listed in the root `allowScripts` field. A refused script is only a warning,
/// so the install would otherwise publish a harness whose entrypoint is the
/// package's "not installed" stub. Report that here, where the message can say
/// which asset to edit, instead of letting the bridge fail to start later.
async fn verify_install_scripts_ran(
    staging: &Path,
    harness: HarnessKind,
    environment: &BTreeMap<String, String>,
) -> Result<()> {
    let mut command = tokio::process::Command::new("npm");
    command
        .args(["install-scripts", "ls", "--json"])
        .current_dir(staging)
        .env_clear()
        .envs(environment);
    let output = crate::subprocess::run_bounded(&mut command, 1024 * 1024, HARNESS_COMMAND_TIMEOUT)
        .await
        .context("read managed harness install-script status")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        // npm versions that do not gate install scripts have no such command.
        if stderr.contains("Unknown command") || stdout.contains("Unknown command") {
            return Ok(());
        }
        return Err(anyhow!(
            "read managed harness install-script status failed: {}",
            output_summary(&output)
        ));
    }
    let pending =
        pending_install_scripts(&stdout).context("read managed harness install-script status")?;
    if pending.is_empty() {
        return Ok(());
    }
    bail!(
        "managed {} harness install skipped install scripts for {}. npm gates dependency install \
         scripts, so add each package to allowScripts in \
         mj-core/assets/harnesses/{}/package.json and reinstall.",
        harness.display_name(),
        pending.join(", "),
        harness.id()
    )
}

/// Collect the `pkg@version` keys npm reports as waiting for approval.
fn pending_install_scripts(stdout: &str) -> Result<Vec<String>> {
    let status: serde_json::Value = serde_json::from_str(stdout)?;
    let groups = match &status {
        serde_json::Value::Object(object) => object
            .get("allowScripts")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default(),
        serde_json::Value::Array(groups) => groups.clone(),
        _ => bail!("unexpected install-script status"),
    };
    let mut pending = Vec::new();
    for changes in groups
        .iter()
        .filter_map(|group| group.get("changes").and_then(serde_json::Value::as_array))
    {
        for change in changes {
            if change.get("change").and_then(serde_json::Value::as_str) == Some("pending")
                && let Some(key) = change.get("key").and_then(serde_json::Value::as_str)
            {
                pending.push(key.to_owned());
            }
        }
    }
    Ok(pending)
}

/// Install the native Muse server a managed Muse session drives.
///
/// The `@brokkai/muse-acp` adapter comes from its pinned npm package, which
/// carries the adapter only; the `muse` executable it drives keeps its own
/// verified download.
async fn install_muse_native(staging: &Path, environment: &BTreeMap<String, String>) -> Result<()> {
    use crate::harness_runtime::MUSE_VERSION;
    let metadata: serde_json::Value =
        serde_json::from_str(include_str!("../assets/muse/runtime.json"))?;
    anyhow::ensure!(
        metadata["muse_version"] == MUSE_VERSION,
        "Muse download metadata does not match the managed runtime pin"
    );
    let key = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
    let platform = metadata["platforms"]
        .get(&key)
        .with_context(|| format!("Muse Code does not have a managed runtime for {key}"))?;
    let field = |name: &str| {
        platform[name]
            .as_str()
            .with_context(|| format!("missing Muse artifact field {name}"))
    };
    let bin = staging.join("bin");
    std::fs::create_dir_all(&bin)?;
    let muse_target = field("muse_target")?;
    let url = format!(
        "https://lookaside.facebook.com/lookaside/muse/download/?channel=muse&version={MUSE_VERSION}&file=muse-{muse_target}"
    );
    let muse = bin.join("muse");
    download_verified_async(&url, &muse, field("muse_sha256")?, environment).await?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&muse, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

/// Install the pinned OpenCode release from the per-platform npm package its
/// official installer downloads. OpenCode 2 publishes no other archive: each
/// `@opencode/cli-<target>` tarball holds the executable at
/// `package/bin/opencode`.
async fn install_opencode(staging: &Path, environment: &BTreeMap<String, String>) -> Result<()> {
    use crate::harness_runtime::OPENCODE_VERSION;
    let metadata: serde_json::Value =
        serde_json::from_str(include_str!("../assets/opencode/runtime.json"))?;
    anyhow::ensure!(
        metadata["version"] == OPENCODE_VERSION,
        "OpenCode download metadata does not match the managed runtime pin"
    );
    let key = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
    let platform = metadata["platforms"]
        .get(&key)
        .with_context(|| format!("OpenCode does not have a managed runtime for {key}"))?;
    let field = |name: &str| {
        platform[name]
            .as_str()
            .with_context(|| format!("missing OpenCode artifact field {name}"))
    };
    let target = field("target")?;
    let file = format!("cli-{target}-{OPENCODE_VERSION}.tgz");
    let url = format!("https://registry.npmjs.org/@opencode/cli-{target}/-/{file}");
    let archive = staging.join(&file);
    download_verified_async(&url, &archive, field("sha256")?, environment).await?;
    let entrypoint = staging.join("opencode");
    let archive_for_extract = archive.clone();
    let entrypoint_for_extract = entrypoint.clone();
    tokio::task::spawn_blocking(move || {
        extract_archive_file(
            &archive_for_extract,
            Path::new("package/bin/opencode"),
            &entrypoint_for_extract,
        )
    })
    .await
    .context("OpenCode extraction task failed")??;
    std::fs::remove_file(&archive)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&entrypoint, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

fn extract_archive_file(archive: &Path, wanted: &Path, destination: &Path) -> Result<()> {
    let archive_file =
        File::open(archive).with_context(|| format!("open archive {}", archive.display()))?;
    let decoder = flate2::read::GzDecoder::new(archive_file);
    let mut tar_archive = tar::Archive::new(decoder);
    let archive_name = archive.to_path_buf();
    for entry in tar_archive
        .entries()
        .with_context(|| format!("read archive {}", archive_name.display()))?
    {
        let mut entry =
            entry.with_context(|| format!("read archive {}", archive_name.display()))?;
        let path = entry
            .path()
            .with_context(|| format!("read entry path in {}", archive_name.display()))?
            .into_owned();
        if path == wanted {
            anyhow::ensure!(
                entry.header().entry_type().is_file(),
                "archive entry {} is not a regular file",
                wanted.display()
            );
            entry.unpack(destination).with_context(|| {
                format!("extract {} to {}", wanted.display(), destination.display())
            })?;
            return Ok(());
        }
    }
    bail!(
        "archive {} did not contain {}",
        archive_name.display(),
        wanted.display()
    )
}

async fn download_verified_async(
    url: &str,
    destination: &Path,
    expected: &str,
    environment: &BTreeMap<String, String>,
) -> Result<()> {
    let result = async {
        download_verified_inner(url, destination, environment).await?;
        let actual = crate::worker_launch::worker_executable_digest(destination)?;
        anyhow::ensure!(
            actual == expected,
            "harness runtime checksum mismatch for {}: expected {expected}, got {actual}",
            destination.display()
        );
        Ok(())
    }
    .await;
    if result.is_err() {
        let _ = std::fs::remove_file(destination);
    }
    result
}

async fn download_verified_inner(
    url: &str,
    destination: &Path,
    environment: &BTreeMap<String, String>,
) -> Result<()> {
    let parsed = url::Url::parse(url).with_context(|| format!("parse artifact URL {url:?}"))?;
    anyhow::ensure!(
        artifact_url_is_allowed(&parsed),
        "artifact downloads require HTTPS: {url}"
    );
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut builder = reqwest::Client::builder()
        .user_agent(concat!("mj/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(30))
        .timeout(ARTIFACT_DOWNLOAD_TIMEOUT)
        .https_only(!cfg!(test))
        .redirect(artifact_redirect_policy());
    for name in ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"] {
        if let Some(proxy) = environment.get(name).filter(|value| !value.is_empty()) {
            let mut proxy = reqwest::Proxy::all(proxy)
                .with_context(|| format!("configure artifact download proxy from {name}"))?;
            if let Some(no_proxy) = environment
                .get("NO_PROXY")
                .or_else(|| environment.get("no_proxy"))
                .and_then(|value| reqwest::NoProxy::from_string(value))
            {
                proxy = proxy.no_proxy(Some(no_proxy));
            }
            builder = builder.proxy(proxy);
            break;
        }
    }
    let client = builder.build().context("build artifact HTTP client")?;
    let mut response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("request artifact {url}"))?
        .error_for_status()
        .with_context(|| format!("download artifact {url}"))?;
    if response
        .content_length()
        .is_some_and(|length| length > MAX_ARTIFACT_BYTES)
    {
        bail!(
            "artifact {url} is larger than the {} byte download limit",
            MAX_ARTIFACT_BYTES
        );
    }
    let mut file = tokio::fs::File::create(destination)
        .await
        .with_context(|| format!("create artifact file {}", destination.display()))?;
    let mut received = 0u64;
    while let Some(chunk) = response
        .chunk()
        .await
        .with_context(|| format!("read artifact {url}"))?
    {
        received = received.saturating_add(chunk.len() as u64);
        anyhow::ensure!(
            received <= MAX_ARTIFACT_BYTES,
            "artifact {url} exceeded the {} byte download limit",
            MAX_ARTIFACT_BYTES
        );
        tokio::io::AsyncWriteExt::write_all(&mut file, &chunk)
            .await
            .with_context(|| format!("write artifact file {}", destination.display()))?;
    }
    tokio::io::AsyncWriteExt::flush(&mut file)
        .await
        .with_context(|| format!("flush artifact file {}", destination.display()))?;
    anyhow::ensure!(received > 0, "artifact {url} was empty");
    Ok(())
}

fn artifact_url_is_allowed(url: &url::Url) -> bool {
    url.scheme() == "https"
        || (cfg!(test)
            && url.scheme() == "http"
            && url.host().is_some_and(|host| match host {
                url::Host::Ipv4(address) => address.is_loopback(),
                url::Host::Ipv6(address) => address.is_loopback(),
                url::Host::Domain(host) => host.eq_ignore_ascii_case("localhost"),
            }))
}

fn artifact_redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= MAX_ARTIFACT_REDIRECTS {
            return attempt.error("too many artifact download redirects");
        }
        if !artifact_url_is_allowed(attempt.url()) {
            return attempt.error("artifact download redirected away from HTTPS");
        }
        attempt.follow()
    })
}

/// Select a Node.js runtime for an ACP launch.
///
/// A compatible `node`, `npm`, and `npx` already on PATH are retained
/// unchanged. Otherwise the exact pinned Node.js distribution is installed in
/// Mjolnir's shared cache and prepended to PATH for this launch. `Ok(None)`
/// means the system runtime was retained; `Ok(Some(path))` names the managed
/// installation.
pub async fn ensure_node_runtime(
    environment: &mut BTreeMap<String, String>,
) -> Result<Option<ManagedNodeRuntime>> {
    if system_node_is_compatible(environment).await {
        return Ok(None);
    }
    let root = managed_node_cache_root(
        environment.get("XDG_CACHE_HOME").map(OsStr::new),
        environment.get("HOME").map(OsStr::new),
    )?;
    let metadata: serde_json::Value =
        serde_json::from_str(include_str!("../assets/node/runtime.json"))?;
    install_managed_node(&root, environment, &metadata)
        .await
        .map(Some)
}

#[cfg(test)]
async fn ensure_node_runtime_from_metadata(
    root: &Path,
    environment: &mut BTreeMap<String, String>,
    metadata: &serde_json::Value,
) -> Result<Option<ManagedNodeRuntime>> {
    if system_node_is_compatible(environment).await {
        return Ok(None);
    }
    install_managed_node(root, environment, metadata)
        .await
        .map(Some)
}

async fn install_managed_node(
    root: &Path,
    environment: &mut BTreeMap<String, String>,
    metadata: &serde_json::Value,
) -> Result<ManagedNodeRuntime> {
    anyhow::ensure!(
        metadata["version"] == NODE_VERSION,
        "Node.js download metadata does not match the managed runtime pin"
    );
    let key = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
    let platform = metadata["platforms"]
        .get(&key)
        .with_context(|| format!("Node.js does not have a managed runtime for {key}"))?;
    let field = |name: &str| {
        platform[name]
            .as_str()
            .with_context(|| format!("missing Node.js artifact field {name}"))
    };
    let target = field("target")?;
    let folder = format!("node-v{NODE_VERSION}-{target}");
    let install = root.join(&folder);
    ensure_node_artifact_platform(platform)?;

    std::fs::create_dir_all(root)
        .with_context(|| format!("create managed Node.js cache {}", root.display()))?;
    let install_lock = open_lock(&root.join(INSTALL_LOCK_FILE))?;
    lock_file(&install_lock, false, "managed Node.js installer").await?;
    remove_abandoned_staging(root)?;

    let cached = managed_node_health(&install, environment).await?;
    if cached != ManagedNodeHealth::Usable {
        if install.exists() {
            let lease = open_lock(&install.join(LEASE_FILE))?;
            match lease.try_lock() {
                Ok(()) => {}
                Err(std::fs::TryLockError::WouldBlock) => bail!(
                    "managed Node.js repair deferred: {} is still in use; the cached installation is unusable: {}",
                    install.display(),
                    cached
                        .unusable_reason()
                        .unwrap_or("the installation is missing or incomplete")
                ),
                Err(std::fs::TryLockError::Error(error)) => {
                    return Err(error).context("lock invalid managed Node.js for repair");
                }
            }
            std::fs::remove_dir_all(&install).with_context(|| {
                format!("remove incomplete managed Node.js {}", install.display())
            })?;
        }

        let staging = tempfile::Builder::new()
            .prefix(".install-")
            .tempdir_in(root)
            .with_context(|| format!("create Node.js staging directory in {}", root.display()))?;
        let file = format!("{folder}.tar.gz");
        let archive = staging.path().join(&file);
        let url = node_distribution_url(environment, &file)?;
        download_verified_async(&url, &archive, field("sha256")?, environment).await?;
        let staging_root = staging.path().to_path_buf();
        let folder_for_extract = folder.clone();
        tokio::task::spawn_blocking(move || {
            extract_node_archive(&archive, &staging_root, &folder_for_extract)
        })
        .await
        .context("Node.js extraction task failed")??;
        let extracted = staging.path().join(&folder);
        if let ManagedNodeHealth::Unusable(reason) =
            managed_node_health(&extracted, environment).await?
        {
            bail!("downloaded Node.js archive is not usable: {reason}");
        } else if !extracted.is_dir() {
            bail!(
                "downloaded Node.js archive did not contain {}",
                extracted.display()
            );
        }
        open_lock(&extracted.join(LEASE_FILE))?;
        match std::fs::rename(&extracted, &install) {
            Ok(()) => {}
            Err(error) if install.exists() => {
                if let ManagedNodeHealth::Unusable(reason) =
                    managed_node_health(&install, environment).await?
                {
                    return Err(error).context(format!(
                        "publish managed Node.js {}: {reason}",
                        install.display()
                    ));
                }
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("publish managed Node.js {}", install.display()));
            }
        }
    }
    let lease_path = install.join(LEASE_FILE);
    let lease = open_lock(&lease_path)?;
    lock_file(&lease, true, "managed Node.js lease").await?;
    gc_node_root(root, &folder)?;
    drop(install_lock);

    let bin = install.join("bin");
    prepend_path(environment, &bin)?;
    Ok(ManagedNodeRuntime {
        path: install,
        lease_path,
        _lease: lease,
    })
}

fn gc_node_root(root: &Path, keep: &str) -> Result<()> {
    for entry in std::fs::read_dir(root)
        .with_context(|| format!("scan managed Node.js cache {}", root.display()))?
    {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name == keep || name.starts_with('.') || !entry.file_type()?.is_dir() {
            continue;
        }
        let lease = match open_lock(&entry.path().join(LEASE_FILE)) {
            Ok(lease) => lease,
            Err(error) => {
                tracing::warn!(%error, path = %entry.path().display(), "could not inspect obsolete managed Node.js");
                continue;
            }
        };
        match lease.try_lock() {
            Ok(()) => {
                if let Err(error) = std::fs::remove_dir_all(entry.path()) {
                    tracing::warn!(%error, path = %entry.path().display(), "could not remove obsolete managed Node.js");
                }
            }
            Err(std::fs::TryLockError::WouldBlock) => {}
            Err(std::fs::TryLockError::Error(error)) => {
                tracing::warn!(%error, path = %entry.path().display(), "could not lock obsolete managed Node.js");
            }
        }
    }
    Ok(())
}

fn node_distribution_url(environment: &BTreeMap<String, String>, file: &str) -> Result<String> {
    let configured = environment
        .get("NODEJS_ORG_MIRROR")
        .or_else(|| environment.get("NVM_NODEJS_ORG_MIRROR"))
        .map(String::as_str)
        .filter(|value| !value.is_empty());
    let base = configured.unwrap_or(DEFAULT_NODE_DIST_URL);
    let parsed =
        url::Url::parse(base).with_context(|| format!("parse Node.js download mirror {base:?}"))?;
    anyhow::ensure!(
        artifact_url_is_allowed(&parsed),
        "Node.js download mirror must use HTTPS: {base}"
    );
    anyhow::ensure!(
        parsed.username().is_empty()
            && parsed.password().is_none()
            && parsed.query().is_none()
            && parsed.fragment().is_none(),
        "Node.js download mirror must be a plain HTTPS base URL without credentials, a query, or a fragment: {base}"
    );
    Ok(format!(
        "{}/v{NODE_VERSION}/{file}",
        base.trim_end_matches('/')
    ))
}

fn ensure_node_artifact_platform(platform: &serde_json::Value) -> Result<()> {
    let Some(interpreter) = platform
        .get("interpreter")
        .and_then(serde_json::Value::as_str)
    else {
        return Ok(());
    };
    anyhow::ensure!(
        Path::new(interpreter).is_file(),
        "Mjolnir's pinned Node.js for this target is a glibc build, but the required program interpreter {interpreter} is missing. Install Node.js 22 or newer with npm and npx on the target PATH"
    );
    Ok(())
}

async fn system_node_is_compatible(environment: &BTreeMap<String, String>) -> bool {
    if crate::program_path::find_program_on_path("node", environment.get("PATH").map(OsStr::new))
        .is_none()
    {
        return false;
    }
    let output = match bounded_env_output(
        tokio::process::Command::new("node").arg("--version"),
        environment,
    )
    .await
    {
        Ok(output) => output,
        Err(error) => {
            tracing::warn!(
                %error,
                "system Node.js exists but its version probe failed; using Mjolnir's managed runtime"
            );
            return false;
        }
    };
    if !output.status.success()
        || !node_version_is_supported(String::from_utf8_lossy(&output.stdout).trim())
    {
        if !output.status.success() {
            tracing::warn!(
                status = %output.status,
                "system Node.js exists but its version probe exited unsuccessfully; using Mjolnir's managed runtime"
            );
        }
        return false;
    }
    for command in ["npm", "npx"] {
        if crate::program_path::find_program_on_path(
            command,
            environment.get("PATH").map(OsStr::new),
        )
        .is_none()
        {
            return false;
        }
        let output = match bounded_env_output(
            tokio::process::Command::new(command).arg("--version"),
            environment,
        )
        .await
        {
            Ok(output) => output,
            Err(error) => {
                tracing::warn!(
                    command,
                    %error,
                    "system Node.js companion command exists but its probe failed; using Mjolnir's managed runtime"
                );
                return false;
            }
        };
        if !output.status.success() {
            tracing::warn!(
                command,
                status = %output.status,
                "system Node.js companion command exited unsuccessfully; using Mjolnir's managed runtime"
            );
            return false;
        }
    }
    true
}

async fn managed_node_health(
    install: &Path,
    environment: &BTreeMap<String, String>,
) -> Result<ManagedNodeHealth> {
    if !install.exists() {
        return Ok(ManagedNodeHealth::Missing);
    }
    if !install.is_dir() {
        return Ok(ManagedNodeHealth::Unusable(format!(
            "{} is not a directory",
            install.display()
        )));
    }
    let bin = install.join("bin");
    if let Some(name) = ["node", "npm", "npx"].into_iter().find(|name| {
        let path = bin.join(name);
        !entrypoint_is_executable(&path)
    }) {
        return Ok(ManagedNodeHealth::Unusable(format!(
            "{} is missing or not executable",
            bin.join(name).display()
        )));
    };
    let mut probe_environment = environment.clone();
    prepend_path(&mut probe_environment, &bin)?;
    let mut node = tokio::process::Command::new(bin.join("node"));
    node.arg("--version");
    let output = match bounded_env_output(&mut node, &probe_environment).await {
        Ok(output) => output,
        Err(error) => {
            return Ok(ManagedNodeHealth::Unusable(format!(
                "node --version could not run: {error:#}"
            )));
        }
    };
    let version = String::from_utf8_lossy(&output.stdout);
    let version = version.trim();
    if !output.status.success() {
        return Ok(ManagedNodeHealth::Unusable(format!(
            "node --version exited with {}",
            output.status
        )));
    }
    if !node_version_is_supported(version) {
        return Ok(ManagedNodeHealth::Unusable(format!(
            "node reports unsupported version {version:?}"
        )));
    }
    for name in ["npm", "npx"] {
        let mut command = tokio::process::Command::new(bin.join(name));
        command.arg("--version");
        let output = match bounded_env_output(&mut command, &probe_environment).await {
            Ok(output) => output,
            Err(error) => {
                return Ok(ManagedNodeHealth::Unusable(format!(
                    "{name} --version could not run: {error:#}"
                )));
            }
        };
        if !output.status.success() {
            return Ok(ManagedNodeHealth::Unusable(format!(
                "{name} --version exited with {}",
                output.status
            )));
        }
    }
    Ok(ManagedNodeHealth::Usable)
}

async fn bounded_env_output(
    command: &mut tokio::process::Command,
    environment: &BTreeMap<String, String>,
) -> Result<std::process::Output> {
    command.env_clear().envs(environment);
    crate::subprocess::run_bounded(command, 64 * 1024, NODE_PROBE_TIMEOUT)
        .await
        .context("probe Node.js runtime")
}

fn node_version_is_supported(version: &str) -> bool {
    version
        .strip_prefix('v')
        .and_then(|version| version.split('.').next())
        .and_then(|major| major.parse::<u64>().ok())
        .is_some_and(|major| major >= 22)
}

fn prepend_path(environment: &mut BTreeMap<String, String>, bin: &Path) -> Result<()> {
    let existing = environment
        .get("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(path))
        .collect::<Vec<_>>();
    if existing.first().is_some_and(|first| first == bin) {
        return Ok(());
    }
    let path = std::iter::once(bin.to_path_buf()).chain(existing);
    let path = std::env::join_paths(path)
        .context("construct managed Node.js PATH")?
        .into_string()
        .map_err(|_| anyhow::anyhow!("managed Node.js PATH is not UTF-8"))?;
    environment.insert("PATH".into(), path);
    Ok(())
}

fn extract_node_archive(archive: &Path, destination: &Path, folder: &str) -> Result<()> {
    let archive = File::open(archive)
        .with_context(|| format!("open Node.js archive {}", archive.display()))?;
    let decoder = flate2::read::GzDecoder::new(archive);
    let mut archive = tar::Archive::new(decoder);
    archive
        .unpack(destination)
        .with_context(|| format!("extract Node.js archive into {}", destination.display()))?;
    let extracted = destination.join(folder);
    anyhow::ensure!(
        extracted.is_dir(),
        "Node.js archive did not extract the expected directory {}",
        extracted.display()
    );
    Ok(())
}

async fn install_kimi(staging: &Path, environment: &BTreeMap<String, String>) -> Result<()> {
    let metadata: serde_json::Value =
        serde_json::from_str(include_str!("../assets/kimi/runtime.json"))?;
    install_kimi_from_metadata(staging, environment, &metadata).await
}

async fn install_kimi_from_metadata(
    staging: &Path,
    environment: &BTreeMap<String, String>,
    metadata: &serde_json::Value,
) -> Result<()> {
    anyhow::ensure!(
        metadata["version"] == KIMI_VERSION,
        "Kimi Code download metadata does not match the managed runtime pin"
    );
    let key = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
    let platform = metadata["platforms"]
        .get(&key)
        .with_context(|| format!("Kimi Code does not have a managed runtime for {key}"))?;
    let field = |name: &str| {
        platform[name]
            .as_str()
            .with_context(|| format!("missing Kimi Code artifact field {name}"))
    };
    let filename = field("filename")?;
    let destinations = [
        format!(
            "{}/{KIMI_VERSION}/{filename}",
            kimi_artifact_base(environment).trim_end_matches('/')
        ),
        format!("https://code.kimi.ai/kimi-code/binaries/{KIMI_VERSION}/{filename}"),
    ];
    let bin = staging.join("bin");
    std::fs::create_dir_all(&bin)?;
    let entrypoint = bin.join("kimi");
    download_verified_first_async(&destinations, &entrypoint, field("sha256")?, environment)
        .await?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&entrypoint, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

async fn install_grok(staging: &Path, environment: &BTreeMap<String, String>) -> Result<()> {
    let metadata: serde_json::Value =
        serde_json::from_str(include_str!("../assets/grok/runtime.json"))?;
    install_grok_from_metadata(staging, environment, &metadata).await
}

async fn install_grok_from_metadata(
    staging: &Path,
    environment: &BTreeMap<String, String>,
    metadata: &serde_json::Value,
) -> Result<()> {
    anyhow::ensure!(
        metadata["version"] == GROK_VERSION,
        "Grok download metadata does not match the managed runtime pin"
    );
    let key = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
    let platform = metadata["platforms"]
        .get(&key)
        .with_context(|| format!("Grok does not have a managed runtime for {key}"))?;
    let field = |name: &str| {
        platform[name]
            .as_str()
            .with_context(|| format!("missing Grok artifact field {name}"))
    };
    let target = field("target")?;
    let filename = format!("grok-{GROK_VERSION}-{target}.zst");
    let destinations = [
        format!(
            "{}/{filename}",
            grok_artifact_base(environment).trim_end_matches('/')
        ),
        format!("https://storage.googleapis.com/grok-build-public-artifacts/cli/{filename}"),
    ];
    let archive = staging.join(&filename);
    download_verified_first_async(&destinations, &archive, field("sha256")?, environment).await?;
    let bin = staging.join("bin");
    std::fs::create_dir_all(&bin)?;
    let entrypoint = bin.join("grok");
    let archive_for_extract = archive.clone();
    let entrypoint_for_extract = entrypoint.clone();
    tokio::task::spawn_blocking(move || {
        extract_zstd_file(&archive_for_extract, &entrypoint_for_extract)
    })
    .await
    .context("Grok extraction task failed")??;
    std::fs::remove_file(&archive)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&entrypoint, std::fs::Permissions::from_mode(0o755))?;
    std::os::unix::fs::symlink("grok", bin.join("agent"))
        .context("create managed Grok agent launcher")?;
    Ok(())
}

fn extract_zstd_file(archive: &Path, destination: &Path) -> Result<()> {
    let input =
        File::open(archive).with_context(|| format!("open Grok archive {}", archive.display()))?;
    let mut decoder = ruzstd::decoding::StreamingDecoder::new(std::io::BufReader::new(input))
        .map_err(|error| anyhow!("read Grok archive {}: {error}", archive.display()))?;
    let mut output = File::create(destination)
        .with_context(|| format!("create Grok executable {}", destination.display()))?;
    std::io::copy(&mut decoder, &mut output)
        .with_context(|| format!("extract Grok executable to {}", destination.display()))?;
    Ok(())
}

fn kimi_artifact_base(environment: &BTreeMap<String, String>) -> String {
    #[cfg(test)]
    if let Some(base) = environment.get("MJ_TEST_KIMI_ARTIFACT_BASE") {
        return base.clone();
    }
    #[cfg(not(test))]
    let _ = environment;
    "https://code.kimi.com/kimi-code/binaries".to_owned()
}

fn grok_artifact_base(environment: &BTreeMap<String, String>) -> String {
    #[cfg(test)]
    if let Some(base) = environment.get("MJ_TEST_GROK_ARTIFACT_BASE") {
        return base.clone();
    }
    #[cfg(not(test))]
    let _ = environment;
    "https://x.ai/cli".to_owned()
}

async fn download_verified_first_async(
    urls: &[String],
    destination: &Path,
    expected: &str,
    environment: &BTreeMap<String, String>,
) -> Result<()> {
    let mut failures = Vec::new();
    for url in urls {
        match download_verified_async(url, destination, expected, environment).await {
            Ok(()) => return Ok(()),
            Err(error) => failures.push(format!("{url}: {error:#}")),
        }
    }
    bail!(
        "could not download a verified managed runtime from any configured source: {}",
        failures.join("; ")
    )
}

async fn run_bounded_checked(
    command: &mut tokio::process::Command,
    operation: &str,
    timeout: Duration,
) -> Result<()> {
    let output = crate::subprocess::run_bounded(command, 4 * 1024 * 1024, timeout)
        .await
        .with_context(|| operation.to_owned())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(anyhow!("{operation} failed: {}", output_summary(&output)))
    }
}

fn output_summary(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    let tail = detail
        .chars()
        .rev()
        .take(4_000)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    format!("{} ({tail})", output.status)
}

#[cfg(test)]
fn download_verified(
    url: &str,
    destination: &Path,
    expected: &str,
    environment: &BTreeMap<String, String>,
) -> Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build managed harness test runtime")?
        .block_on(download_verified_async(
            url,
            destination,
            expected,
            environment,
        ))
}

#[cfg(test)]
fn apply_environment(command: &mut Command, environment: &BTreeMap<String, String>) {
    command.env_clear().envs(environment);
}

#[cfg(test)]
fn run_checked(command: &mut Command, operation: &str) -> Result<()> {
    let output =
        crate::subprocess::run_with_input(command, &[]).with_context(|| operation.to_owned())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(anyhow!("{operation} failed: {}", output_summary(&output)))
    }
}

fn complete_install(path: &Path, harness: HarnessKind, selected: HarnessPin) -> Result<bool> {
    if !managed_harness_manifest_matches(path, harness, selected.install_id)? {
        return Ok(false);
    }
    Ok(validate_entrypoint(path, selected, harness).is_ok())
}

fn validate_entrypoint(path: &Path, selected: HarnessPin, harness: HarnessKind) -> Result<()> {
    for entry in std::iter::once(selected.entrypoint).chain(harness.extra_managed_entrypoint()) {
        let entrypoint = path.join(entry);
        if !entrypoint_is_executable(&entrypoint) {
            bail!(
                "{} installer did not create executable {}",
                harness.display_name(),
                entrypoint.display()
            );
        }
    }
    Ok(())
}

pub fn entrypoint_is_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    use std::os::unix::fs::PermissionsExt;
    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
}

fn open_lock(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600);
    options
        .open(path)
        .with_context(|| format!("open managed harness lock {}", path.display()))
}

fn gc_harness_root(root: &Path, keep_install_id: &str) -> Result<()> {
    for entry in std::fs::read_dir(root)
        .with_context(|| format!("scan managed harness cache {}", root.display()))?
    {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name == keep_install_id || name.starts_with('.') || !entry.file_type()?.is_dir() {
            continue;
        }
        let lease_path = entry.path().join(LEASE_FILE);
        let lease = match open_lock(&lease_path) {
            Ok(lease) => lease,
            Err(error) => {
                tracing::warn!(%error, path = %entry.path().display(), "could not inspect obsolete managed harness");
                continue;
            }
        };
        match lease.try_lock() {
            Ok(()) => {
                if let Err(error) = std::fs::remove_dir_all(entry.path()) {
                    tracing::warn!(%error, path = %entry.path().display(), "could not remove obsolete managed harness");
                }
            }
            Err(std::fs::TryLockError::WouldBlock) => {}
            Err(std::fs::TryLockError::Error(error)) => {
                tracing::warn!(%error, path = %entry.path().display(), "could not lock obsolete managed harness");
            }
        }
    }
    Ok(())
}

pub fn acquire_supervisor_lease(path: &Path) -> Result<File> {
    let lease = open_lock(path)?;
    lease
        .lock_shared()
        .with_context(|| format!("lease managed harness for supervisor {}", path.display()))?;
    Ok(lease)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};

    use super::*;

    #[test]
    #[ignore = "downloads the pinned Grok artifact into a disposable cache"]
    fn grok_real_install_launches_from_final_directory() {
        let parent =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/grok-install-validation");
        std::fs::create_dir_all(&parent).unwrap();
        let temp = tempfile::tempdir_in(parent).unwrap();
        let environment = crate::login_environment::discover().unwrap();
        let managed = resolve_at(
            temp.path(),
            HarnessKind::Grok,
            ExecutionPolicy::ConfiguredApprovals,
            &environment,
        )
        .unwrap();
        for path in [
            managed.command.clone(),
            managed.command.with_file_name("agent"),
        ] {
            let mut command = Command::new(path);
            command.arg("--version");
            apply_environment(&mut command, &environment);
            run_checked(&mut command, "verify relocated Grok launcher").unwrap();
        }
    }

    #[test]
    #[ignore = "downloads the pinned Kimi artifact into a disposable cache"]
    fn kimi_real_install_launches_from_final_directory() {
        let parent =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/kimi-install-validation");
        std::fs::create_dir_all(&parent).unwrap();
        let temp = tempfile::tempdir_in(parent).unwrap();
        let environment = crate::login_environment::discover().unwrap();
        let managed = resolve_at(
            temp.path(),
            HarnessKind::Kimi,
            ExecutionPolicy::ConfiguredApprovals,
            &environment,
        )
        .unwrap();
        let mut command = Command::new(&managed.command);
        command.arg("--version");
        apply_environment(&mut command, &environment);
        run_checked(&mut command, "verify relocated Kimi executable").unwrap();
    }

    #[test]
    fn direct_kimi_artifact_installs_the_pinned_executable() {
        let temp = tempfile::tempdir().unwrap();
        let body = b"#!/bin/sh\necho kimi-fixture\n".to_vec();
        let server = ArtifactServer::serve(body.clone());
        let fixture = temp.path().join("kimi-artifact");
        std::fs::write(&fixture, &body).unwrap();
        let mut metadata: serde_json::Value =
            serde_json::from_str(include_str!("../assets/kimi/runtime.json")).unwrap();
        metadata["platforms"][current_node_platform()]["sha256"] = serde_json::Value::String(
            crate::worker_launch::worker_executable_digest(&fixture).unwrap(),
        );
        let environment =
            BTreeMap::from([("MJ_TEST_KIMI_ARTIFACT_BASE".to_owned(), server.url.clone())]);
        let staging = temp.path().join("staging");
        std::fs::create_dir_all(&staging).unwrap();

        run_async(install_kimi_from_metadata(
            &staging,
            &environment,
            &metadata,
        ))
        .unwrap();

        let output =
            crate::subprocess::run_with_input(&mut Command::new(staging.join("bin/kimi")), &[])
                .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"kimi-fixture\n");
    }

    #[test]
    fn direct_grok_artifact_installs_relocatable_launchers() {
        let temp = tempfile::tempdir().unwrap();
        let body = b"#!/bin/sh\necho grok-fixture\n";
        let compressed = ruzstd::encoding::compress_to_vec(
            &body[..],
            ruzstd::encoding::CompressionLevel::Fastest,
        );
        let server = ArtifactServer::serve(compressed.clone());
        let fixture = temp.path().join("grok-artifact.zst");
        std::fs::write(&fixture, &compressed).unwrap();
        let mut metadata: serde_json::Value =
            serde_json::from_str(include_str!("../assets/grok/runtime.json")).unwrap();
        metadata["platforms"][current_node_platform()]["sha256"] = serde_json::Value::String(
            crate::worker_launch::worker_executable_digest(&fixture).unwrap(),
        );
        let environment =
            BTreeMap::from([("MJ_TEST_GROK_ARTIFACT_BASE".to_owned(), server.url.clone())]);
        let staging = temp.path().join("staging");
        let relocated = temp.path().join("relocated");
        std::fs::create_dir_all(&staging).unwrap();

        run_async(install_grok_from_metadata(
            &staging,
            &environment,
            &metadata,
        ))
        .unwrap();
        std::fs::rename(&staging, &relocated).unwrap();

        for launcher in ["grok", "agent"] {
            let path = relocated.join("bin").join(launcher);
            if launcher == "agent" {
                assert_eq!(std::fs::read_link(&path).unwrap(), Path::new("grok"));
            } else {
                assert!(path.is_file());
            }
            let output = crate::subprocess::run_with_input(&mut Command::new(path), &[]).unwrap();
            assert!(output.status.success());
            assert_eq!(output.stdout, b"grok-fixture\n");
        }
    }

    #[test]
    fn archive_extraction_selects_only_the_expected_regular_file() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("opencode.tgz");
        let encoder = flate2::write::GzEncoder::new(
            File::create(&archive).unwrap(),
            flate2::Compression::default(),
        );
        let mut builder = tar::Builder::new(encoder);
        for (path, body) in [
            ("package/bin/opencode", "#!/bin/sh\necho ok\n"),
            ("package/unrelated", "must not be extracted\n"),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder
                .append_data(&mut header, path, body.as_bytes())
                .unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap();
        let destination = temp.path().join("opencode");

        extract_archive_file(&archive, Path::new("package/bin/opencode"), &destination).unwrap();

        assert_eq!(
            std::fs::read(&destination).unwrap(),
            b"#!/bin/sh\necho ok\n"
        );
        assert!(!temp.path().join("package/unrelated").exists());
    }

    #[test]
    fn link_relocation_preserves_relative_and_external_links_without_following_them() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("staging");
        executable(&root.join("bin/tool"), "#!/bin/sh\nexit 0\n");
        let external = temp.path().join("external");
        std::fs::create_dir(&external).unwrap();
        std::os::unix::fs::symlink("tool", root.join("bin/relative")).unwrap();
        std::os::unix::fs::symlink(&external, root.join("external")).unwrap();
        std::os::unix::fs::symlink(root.join("bin/tool"), external.join("untouched")).unwrap();
        relativize_internal_links(&root, &root).unwrap();
        assert_eq!(
            std::fs::read_link(root.join("bin/relative")).unwrap(),
            Path::new("tool")
        );
        assert_eq!(std::fs::read_link(root.join("external")).unwrap(), external);
        assert_eq!(
            std::fs::read_link(external.join("untouched")).unwrap(),
            root.join("bin/tool")
        );
    }

    #[test]
    fn invalid_install_is_not_removed_while_leased() {
        let temp = tempfile::tempdir().unwrap();
        let cache = temp.path().join("cache");
        let selected = pin(HarnessKind::Grok);
        let install = cache.join("grok").join(selected.install_id);
        complete_fake(
            &cache.join("grok"),
            HarnessKind::Grok,
            selected.install_id,
            "bin/grok",
        );
        executable(&install.join("bin/agent"), "#!/bin/sh\nexit 0\n");
        let lease = acquire_supervisor_lease(&install.join(LEASE_FILE)).unwrap();
        std::fs::remove_file(install.join("bin/grok")).unwrap();
        let error = resolve_at(
            &cache,
            HarnessKind::Grok,
            ExecutionPolicy::ConfiguredApprovals,
            &BTreeMap::new(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("repair deferred"));
        assert!(install.exists());
        drop(lease);
    }

    #[test]
    fn pending_install_scripts_names_packages_npm_blocked() {
        // Recorded from npm 12.0.2, which blocks dependency install scripts
        // unless the root package lists them in allowScripts.
        let recorded = r#"{"allowScripts":[{"name":"esbuild","changes":[{"key":"esbuild@0.25.0","change":"pending"}]}]}"#;
        assert_eq!(
            pending_install_scripts(recorded).unwrap(),
            vec!["esbuild@0.25.0"]
        );
        assert!(
            pending_install_scripts(r#"{"allowScripts":[]}"#)
                .unwrap()
                .is_empty()
        );
        assert!(pending_install_scripts("not json").is_err());
    }

    #[test]
    fn node_pin_matches_runtime_metadata() {
        let metadata: serde_json::Value =
            serde_json::from_str(include_str!("../assets/node/runtime.json"))
                .expect("parse Node.js runtime metadata");
        assert_eq!(metadata["version"], NODE_VERSION);
    }

    #[test]
    fn kimi_pin_matches_runtime_metadata() {
        use crate::harness_runtime::KIMI_VERSION;

        let metadata: serde_json::Value =
            serde_json::from_str(include_str!("../assets/kimi/runtime.json"))
                .expect("parse Kimi runtime metadata");
        assert_eq!(metadata["version"], KIMI_VERSION);
        assert!(metadata["platforms"][current_node_platform()]["sha256"].is_string());
    }

    #[test]
    fn grok_pin_matches_runtime_metadata() {
        use crate::harness_runtime::GROK_VERSION;

        let metadata: serde_json::Value =
            serde_json::from_str(include_str!("../assets/grok/runtime.json"))
                .expect("parse Grok runtime metadata");
        assert_eq!(metadata["version"], GROK_VERSION);
        assert!(metadata["platforms"][current_node_platform()]["sha256"].is_string());
    }

    #[test]
    fn node_download_uses_a_validated_https_mirror() {
        let file = format!("node-v{NODE_VERSION}-linux-x64.tar.gz");
        let empty = BTreeMap::new();
        assert_eq!(
            node_distribution_url(&empty, &file).unwrap(),
            format!("https://nodejs.org/dist/v{NODE_VERSION}/{file}")
        );

        let environment = BTreeMap::from([(
            "NVM_NODEJS_ORG_MIRROR".into(),
            "https://mirror.example/node/".into(),
        )]);
        assert_eq!(
            node_distribution_url(&environment, &file).unwrap(),
            format!("https://mirror.example/node/v{NODE_VERSION}/{file}")
        );

        let environment = BTreeMap::from([
            (
                "NVM_NODEJS_ORG_MIRROR".into(),
                "https://nvm.example/node".into(),
            ),
            (
                "NODEJS_ORG_MIRROR".into(),
                "https://mjolnir.example/node/".into(),
            ),
        ]);
        assert_eq!(
            node_distribution_url(&environment, &file).unwrap(),
            format!("https://mjolnir.example/node/v{NODE_VERSION}/{file}")
        );

        for mirror in [
            "http://mirror.example/node",
            "https://user:secret@mirror.example/node",
            "https://mirror.example/node?token=secret",
        ] {
            let environment = BTreeMap::from([("NODEJS_ORG_MIRROR".into(), mirror.into())]);
            assert!(
                node_distribution_url(&environment, &file).is_err(),
                "{mirror:?} was accepted"
            );
        }
    }

    #[test]
    fn linux_node_artifact_requires_its_program_interpreter() {
        let metadata: serde_json::Value =
            serde_json::from_str(include_str!("../assets/node/runtime.json")).unwrap();
        let mut platform = metadata["platforms"][current_node_platform()].clone();
        platform["interpreter"] = serde_json::Value::String(
            "/definitely/missing/mjolnir-node-program-interpreter".into(),
        );
        let error = ensure_node_artifact_platform(&platform).unwrap_err();
        assert!(error.to_string().contains("required program interpreter"));
        assert!(error.to_string().contains("Install Node.js 22 or newer"));
    }

    #[test]
    fn muse_pin_matches_runtime_metadata() {
        use crate::harness_runtime::{MUSE_ACP_PACKAGE, MUSE_ACP_VERSION, MUSE_VERSION};
        let metadata: serde_json::Value =
            serde_json::from_str(include_str!("../assets/muse/runtime.json"))
                .expect("parse Muse runtime metadata");
        assert_eq!(metadata["muse_version"], MUSE_VERSION);
        let bridge = crate::harness_runtime::npm_bridge(HarnessKind::Muse)
            .expect("Muse installs its adapter from npm");
        assert_eq!(bridge.package, MUSE_ACP_PACKAGE);
        assert_eq!(bridge.version, MUSE_ACP_VERSION);
        let selected = pin(HarnessKind::Muse);
        assert_eq!(
            selected.install_id,
            format!("muse-acp-{MUSE_ACP_VERSION}_muse-{MUSE_VERSION}")
        );
        assert_eq!(
            selected.display_version,
            format!("muse-acp {MUSE_ACP_VERSION} + Muse Code {MUSE_VERSION}")
        );
    }

    #[test]
    fn checksum_failure_removes_the_downloaded_artifact() {
        let temp = tempfile::tempdir().unwrap();
        let server = ArtifactServer::serve(b"corrupted".to_vec());
        let environment = BTreeMap::new();
        let output = temp.path().join("download");
        let error = download_verified(
            &format!("{}/muse", server.url),
            &output,
            &"0".repeat(64),
            &environment,
        )
        .unwrap_err();
        assert!(error.to_string().contains("checksum mismatch"));
        assert!(!output.exists());
    }

    #[test]
    #[ignore = "installs the pinned Muse runtime from its publisher"]
    fn muse_real_install_is_verified_concurrent_and_reusable() {
        let parent =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/muse-integration-inspect");
        std::fs::create_dir_all(&parent).unwrap();
        let temp = tempfile::tempdir_in(parent).unwrap();
        std::thread::scope(|scope| {
            let first = scope.spawn(|| {
                resolve_at(
                    temp.path(),
                    HarnessKind::Muse,
                    ExecutionPolicy::ConfiguredApprovals,
                    &BTreeMap::new(),
                )
                .unwrap()
            });
            let second = scope.spawn(|| {
                resolve_at(
                    temp.path(),
                    HarnessKind::Muse,
                    ExecutionPolicy::ConfiguredApprovals,
                    &BTreeMap::new(),
                )
                .unwrap()
            });
            let first = first.join().unwrap();
            let second = second.join().unwrap();
            assert_eq!(first.command, second.command);
            assert_eq!(first.command, PathBuf::from("sh"));
            let bridge = npm_bridge(HarnessKind::Muse).unwrap();
            assert_eq!(first.args, vec!["-c".to_owned(), bridge.bootstrap_script()]);
            let mut muse = Command::new(&first.environment["MUSE_CLI"]);
            muse.arg("--version");
            run_checked(&mut muse, "verify installed Muse executable").unwrap();
            assert!(entrypoint_is_executable(Path::new(
                &first.environment["MUSE_CLI"]
            )));
        });
    }
    fn executable(path: &Path, body: &str) {
        let parent = path.parent().unwrap();
        std::fs::create_dir_all(parent).unwrap();
        crate::test_hooks::install_fake_command(
            parent,
            path.file_name().unwrap().to_str().unwrap(),
            body,
        );
    }

    fn complete_fake(root: &Path, harness: HarnessKind, install_id: &str, entrypoint: &str) {
        let install = root.join(install_id);
        std::fs::create_dir_all(&install).unwrap();
        executable(&install.join(entrypoint), "#!/bin/sh\nexit 0\n");
        open_lock(&install.join(LEASE_FILE)).unwrap();
        let body =
            serde_json::to_vec_pretty(&ManagedHarnessManifest::for_install(harness, install_id))
                .unwrap();
        crate::config::atomic_write(&install.join(MANIFEST_FILE), &body).unwrap();
    }

    fn fake_node_tools(root: &Path, installs: &Path) -> BTreeMap<String, String> {
        let bin = root.join("bin");
        executable(
            &bin.join("node"),
            "#!/bin/sh\nif [ \"$1\" = --version ]; then echo v24.22.1; fi\n",
        );
        executable(
            &bin.join("npx"),
            "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 11.6.2; fi\n",
        );
        executable(
            &bin.join("npm"),
            &format!(
                "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 12.0.0; exit 0; fi\nif [ \"$1\" = install-scripts ]; then printf '{{\"allowScripts\":[]}}'; exit 0; fi\nprintf x >> '{}'\nmkdir -p node_modules/.bin node_modules/@openai/codex/bin\nprintf '#!/bin/sh\\nexit 0\\n' > node_modules/.bin/codex-acp\nchmod 755 node_modules/.bin/codex-acp\nprintf '// managed codex' > node_modules/@openai/codex/bin/codex.js\n",
                installs.display()
            ),
        );
        BTreeMap::from([(
            "PATH".to_owned(),
            format!("{}:/usr/bin:/bin", bin.display()),
        )])
    }

    fn fake_node_archive(root: &Path) -> (PathBuf, serde_json::Value) {
        let (key, target) = match (std::env::consts::OS, std::env::consts::ARCH) {
            ("linux", "x86_64") => ("linux-x86_64", "linux-x64"),
            ("linux", "aarch64") => ("linux-aarch64", "linux-arm64"),
            ("macos", "x86_64") => ("macos-x86_64", "darwin-x64"),
            ("macos", "aarch64") => ("macos-aarch64", "darwin-arm64"),
            (os, arch) => panic!("managed Node.js test has no artifact for {os}-{arch}"),
        };
        let archive = root.join(format!("node-v{NODE_VERSION}-{target}.tar.gz"));
        let encoder = flate2::write::GzEncoder::new(
            File::create(&archive).unwrap(),
            flate2::Compression::default(),
        );
        let mut builder = tar::Builder::new(encoder);
        let root = format!("node-v{NODE_VERSION}-{target}");
        for (path, body) in [
            (
                format!("{root}/bin/node"),
                format!("#!/bin/sh\necho v{NODE_VERSION}\n"),
            ),
            (
                format!("{root}/lib/node_modules/npm/bin/npm-cli.js"),
                "#!/bin/sh\necho 11.6.2\n".to_owned(),
            ),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder
                .append_data(&mut header, path, body.as_bytes())
                .unwrap();
        }
        for name in ["npm", "npx"] {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_mode(0o777);
            header.set_size(0);
            header
                .set_link_name("../lib/node_modules/npm/bin/npm-cli.js")
                .unwrap();
            header.set_cksum();
            builder
                .append_data(&mut header, format!("{root}/bin/{name}"), std::io::empty())
                .unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap();
        let digest = crate::worker_launch::worker_executable_digest(&archive).unwrap();
        let mut metadata: serde_json::Value =
            serde_json::from_str(include_str!("../assets/node/runtime.json")).unwrap();
        metadata["platforms"][key]["sha256"] = serde_json::Value::String(digest);
        (archive, metadata)
    }

    struct ArtifactServer {
        url: String,
        server: Arc<tiny_http::Server>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl ArtifactServer {
        fn serve(body: Vec<u8>) -> Self {
            let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
            let address = server.server_addr().to_ip().unwrap();
            let task_server = Arc::clone(&server);
            let body = Arc::new(body);
            let thread = std::thread::spawn(move || {
                for request in task_server.incoming_requests() {
                    let response = tiny_http::Response::from_data((*body).clone());
                    request.respond(response).unwrap();
                }
            });
            Self {
                url: format!("http://{address}"),
                server,
                thread: Some(thread),
            }
        }
    }

    impl Drop for ArtifactServer {
        fn drop(&mut self) {
            self.server.unblock();
            if let Some(thread) = self.thread.take() {
                thread.join().unwrap();
            }
        }
    }

    fn fake_node_server(
        archive: &Path,
        root: &Path,
    ) -> (ArtifactServer, PathBuf, BTreeMap<String, String>) {
        let tools = root.join("tools");
        std::fs::create_dir_all(&tools).unwrap();
        let body = std::fs::read(archive).unwrap();
        let server = ArtifactServer::serve(body);
        let mirror = server.url.clone();
        (
            server,
            tools.clone(),
            BTreeMap::from([
                ("PATH".to_owned(), tools.to_string_lossy().into_owned()),
                ("NODEJS_ORG_MIRROR".to_owned(), mirror),
            ]),
        )
    }

    fn run_async<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

    #[test]
    fn compatible_system_node_is_reused_without_downloading() {
        let temp = tempfile::tempdir().unwrap();
        let mut environment = fake_node_tools(temp.path(), &temp.path().join("unused"));
        environment.insert(
            "XDG_CACHE_HOME".into(),
            temp.path().join("cache").to_string_lossy().into_owned(),
        );
        let original_path = environment["PATH"].clone();

        let runtime = run_async(ensure_node_runtime(&mut environment)).unwrap();

        assert!(runtime.is_none());
        assert_eq!(environment["PATH"], original_path);
        assert!(!temp.path().join("cache/mjolnir/node").exists());
    }

    #[test]
    fn missing_node_is_downloaded_verified_and_prepended_once() {
        let temp = tempfile::tempdir().unwrap();
        let (archive, metadata) = fake_node_archive(temp.path());
        let (_server, _tools, mut environment) = fake_node_server(&archive, temp.path());
        environment.insert(
            "XDG_CACHE_HOME".into(),
            temp.path().join("cache").to_string_lossy().into_owned(),
        );
        let root = temp.path().join("node");

        let runtime = run_async(ensure_node_runtime_from_metadata(
            &root,
            &mut environment,
            &metadata,
        ))
        .unwrap()
        .expect("missing Node.js selects the managed installation");
        let install = runtime.path().to_path_buf();
        assert_eq!(
            install.file_name().unwrap().to_string_lossy(),
            format!("node-v{NODE_VERSION}-{}", environment_target())
        );
        for name in ["node", "npm", "npx"] {
            assert!(install.join("bin").join(name).is_file());
        }
        assert_eq!(
            std::env::split_paths(&environment["PATH"]).next(),
            Some(install.join("bin"))
        );
        drop(runtime);

        let first_path = environment["PATH"].clone();
        let runtime = run_async(install_managed_node(&root, &mut environment, &metadata)).unwrap();
        assert_eq!(runtime.path(), install);
        assert_eq!(environment["PATH"], first_path);
        assert!(std::fs::read_dir(&root).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".install-")
        }));
    }

    #[test]
    fn unsupported_system_node_is_replaced_with_the_managed_runtime() {
        let temp = tempfile::tempdir().unwrap();
        let (archive, metadata) = fake_node_archive(temp.path());
        let (_server, tools, mut environment) = fake_node_server(&archive, temp.path());
        executable(
            &tools.join("node"),
            "#!/bin/sh\nif [ \"$1\" = --version ]; then echo v20.19.0; fi\n",
        );
        for command in ["npm", "npx"] {
            executable(
                &tools.join(command),
                "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 11.6.2; fi\n",
            );
        }

        let runtime = run_async(ensure_node_runtime_from_metadata(
            &temp.path().join("node"),
            &mut environment,
            &metadata,
        ))
        .unwrap()
        .expect("Node.js 20 selects the managed installation");

        assert_eq!(
            std::env::split_paths(&environment["PATH"]).next(),
            Some(runtime.path().join("bin"))
        );
    }

    #[test]
    fn corrupt_cached_node_is_repaired_from_the_verified_archive() {
        let temp = tempfile::tempdir().unwrap();
        let (archive, metadata) = fake_node_archive(temp.path());
        let (_server, _tools, mut environment) = fake_node_server(&archive, temp.path());
        let root = temp.path().join("node");
        let install = root.join(format!("node-v{NODE_VERSION}-{}", environment_target()));
        for name in ["node", "npm", "npx"] {
            executable(
                &install.join("bin").join(name),
                "#!/definitely/missing/mjolnir-node-program-interpreter\nexit 1\n",
            );
        }
        open_lock(&install.join(LEASE_FILE)).unwrap();

        let runtime = run_async(install_managed_node(&root, &mut environment, &metadata)).unwrap();

        assert_eq!(runtime.path(), install);
        let output =
            crate::subprocess::run_with_input(&mut Command::new(install.join("bin/node")), &[])
                .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, format!("v{NODE_VERSION}\n").as_bytes());
    }

    #[test]
    fn managed_node_checksum_failure_never_publishes_an_install() {
        let temp = tempfile::tempdir().unwrap();
        let (archive, mut metadata) = fake_node_archive(temp.path());
        let (_server, _tools, mut environment) = fake_node_server(&archive, temp.path());
        metadata["platforms"][current_node_platform()]["sha256"] =
            serde_json::Value::String("0".repeat(64));
        let root = temp.path().join("node");

        let error =
            run_async(install_managed_node(&root, &mut environment, &metadata)).unwrap_err();

        assert!(error.to_string().contains("checksum mismatch"), "{error:#}");
        assert!(
            std::fs::read_dir(&root).unwrap().all(|entry| {
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("node-v")
            }),
            "a corrupt download must not leave a final Node.js install"
        );
    }

    #[test]
    fn concurrent_first_use_publishes_one_managed_node_install() {
        let temp = tempfile::tempdir().unwrap();
        let (archive, metadata) = fake_node_archive(temp.path());
        let (_server, _tools, environment) = fake_node_server(&archive, temp.path());
        let root = temp.path().join("node");
        let barrier = Arc::new(Barrier::new(2));
        let threads = (0..2)
            .map(|_| {
                let root = root.clone();
                let metadata = metadata.clone();
                let environment = environment.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let mut environment = environment;
                    barrier.wait();
                    run_async(install_managed_node(&root, &mut environment, &metadata)).unwrap()
                })
            })
            .collect::<Vec<_>>();
        let runtimes = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();

        assert_eq!(runtimes[0].path(), runtimes[1].path());
        assert!(
            std::fs::read_dir(&root).unwrap().all(|entry| {
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".install-")
            }),
            "concurrent installers must not leave abandoned staging"
        );
    }

    fn current_node_platform() -> &'static str {
        match (std::env::consts::OS, std::env::consts::ARCH) {
            ("linux", "x86_64") => "linux-x86_64",
            ("linux", "aarch64") => "linux-aarch64",
            ("macos", "x86_64") => "macos-x86_64",
            ("macos", "aarch64") => "macos-aarch64",
            (os, arch) => panic!("managed Node.js test has no artifact for {os}-{arch}"),
        }
    }

    fn environment_target() -> &'static str {
        match current_node_platform() {
            "linux-x86_64" => "linux-x64",
            "linux-aarch64" => "linux-arm64",
            "macos-x86_64" => "darwin-x64",
            "macos-aarch64" => "darwin-arm64",
            _ => unreachable!(),
        }
    }

    #[test]
    fn concurrent_first_use_publishes_one_complete_install() {
        let temp = tempfile::tempdir().unwrap();
        let cache = temp.path().join("cache");
        let install_counter = temp.path().join("installs");
        let environment = fake_node_tools(temp.path(), &install_counter);
        let barrier = Arc::new(Barrier::new(2));

        let threads = (0..2)
            .map(|_| {
                let cache = cache.clone();
                let environment = environment.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    resolve_at(
                        &cache,
                        HarnessKind::Codex,
                        ExecutionPolicy::ConfiguredApprovals,
                        &environment,
                    )
                    .unwrap()
                })
            })
            .collect::<Vec<_>>();
        let resolved = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();

        assert_eq!(std::fs::read(&install_counter).unwrap(), b"x");
        assert_eq!(resolved[0].command, resolved[1].command);
        assert!(resolved[0].command.is_absolute());
        assert!(resolved[0].command.exists());
    }

    #[test]
    fn first_use_removes_staging_abandoned_by_a_killed_installer() {
        let temp = tempfile::tempdir().unwrap();
        let cache = temp.path().join("cache");
        let harness_root = cache.join(HarnessKind::Codex.id());
        let abandoned = harness_root.join(".install-killed");
        std::fs::create_dir_all(&abandoned).unwrap();
        std::fs::write(abandoned.join("partial-download"), b"incomplete").unwrap();
        let install_counter = temp.path().join("installs");
        let environment = fake_node_tools(temp.path(), &install_counter);

        let managed = resolve_at(
            &cache,
            HarnessKind::Codex,
            ExecutionPolicy::ConfiguredApprovals,
            &environment,
        )
        .unwrap();

        assert!(!abandoned.exists());
        assert!(managed.command.exists());
    }

    #[test]
    fn obsolete_install_waits_for_the_last_shared_lease() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("codex");
        std::fs::create_dir_all(&root).unwrap();
        complete_fake(&root, HarnessKind::Codex, "old", "bin/bridge");
        complete_fake(&root, HarnessKind::Codex, "current", "bin/bridge");

        let old_lease = open_lock(&root.join("old").join(LEASE_FILE)).unwrap();
        old_lease.lock_shared().unwrap();
        gc_harness_root(&root, "current").unwrap();
        assert!(root.join("old").exists());

        drop(old_lease);
        assert_lease_released(|| {
            gc_harness_root(&root, "current").unwrap();
            !root.join("old").exists()
        });
        assert!(root.join("current").exists());
    }

    #[test]
    fn supervisor_lease_closes_the_worker_shutdown_gap() {
        let temp = tempfile::tempdir().unwrap();
        let cache = temp.path().join("cache");
        let selected = pin(HarnessKind::Codex);
        let root = cache.join(HarnessKind::Codex.id());
        std::fs::create_dir_all(&root).unwrap();
        complete_fake(
            &root,
            HarnessKind::Codex,
            selected.install_id,
            selected.entrypoint,
        );
        let environment = fake_node_tools(temp.path(), &temp.path().join("unused"));
        let managed = resolve_at(
            &cache,
            HarnessKind::Codex,
            ExecutionPolicy::ConfiguredApprovals,
            &environment,
        )
        .unwrap();
        let supervisor = acquire_supervisor_lease(&managed.lease_path).unwrap();
        let lease_path = managed.lease_path.clone();
        drop(managed);

        let exclusive = open_lock(&lease_path).unwrap();
        assert!(matches!(
            exclusive.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        drop(supervisor);
        assert_lease_released(|| match exclusive.try_lock() {
            Ok(()) => true,
            Err(std::fs::TryLockError::WouldBlock) => false,
            Err(std::fs::TryLockError::Error(error)) => panic!("lease lock failed: {error}"),
        });
    }

    // Parallel process-spawning tests can briefly inherit CLOEXEC descriptors
    // between fork and exec. Closing our copy need not release the lock yet.
    fn assert_lease_released(mut released: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !released() {
            assert!(
                std::time::Instant::now() < deadline,
                "lease was not released"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn a_complete_cache_hit_does_not_execute_the_entrypoint() {
        for harness in [HarnessKind::Codex, HarnessKind::Claude] {
            check_complete_cache_hit(harness);
        }
    }

    fn check_complete_cache_hit(harness: HarnessKind) {
        let temp = tempfile::tempdir().unwrap();
        let cache = temp.path().join("cache");
        let selected = pin(harness);
        let root = cache.join(harness.id());
        std::fs::create_dir_all(&root).unwrap();
        complete_fake(&root, harness, selected.install_id, selected.entrypoint);
        let install = root.join(selected.install_id);
        if let Some(entry) = harness.extra_managed_entrypoint() {
            executable(&install.join(entry), "#!/bin/sh\nexit 0\n");
        }
        let marker = temp.path().join("executed");
        executable(
            &root.join(selected.install_id).join(selected.entrypoint),
            &format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
        );
        let environment = fake_node_tools(temp.path(), &temp.path().join("unused"));

        let managed = resolve_at(
            &cache,
            harness,
            ExecutionPolicy::ConfiguredApprovals,
            &environment,
        )
        .unwrap();

        assert!(!marker.exists());
        let (provider_env, entry) = match harness {
            HarnessKind::Codex => ("CODEX_PATH", "node_modules/@openai/codex/bin/codex.js"),
            HarnessKind::Claude => (
                "CLAUDE_CODE_EXECUTABLE",
                harness.extra_managed_entrypoint().unwrap(),
            ),
            _ => unreachable!(),
        };
        let expected_provider = install.join(entry).to_string_lossy().into_owned();
        assert_eq!(
            managed.environment.get(provider_env),
            Some(&expected_provider)
        );
        if harness == HarnessKind::Claude {
            std::fs::remove_file(install.join(entry)).unwrap();
            assert!(
                !complete_install(&install, harness, selected).unwrap(),
                "a cache without the pinned Claude Code must not launch the SDK's bundled CLI"
            );
        }
    }
}
