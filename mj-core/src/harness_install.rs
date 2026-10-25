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
const MAX_INSTALLER_BYTES: usize = 8 * 1024 * 1024;

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
    // provides resolves the target's own `dsh-acp` or fetches the pinned
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
/// resolves the target's own `dsh-acp` or the pinned npm package, so there is
/// no install to lease and nothing to cache.
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
    let mut tar = tokio::process::Command::new("tar");
    tar.arg("-xzf")
        .arg(&archive)
        .arg("-C")
        .arg(staging)
        .arg("package/bin/opencode")
        .env_clear()
        .envs(environment);
    run_bounded_checked(
        &mut tar,
        "extract verified OpenCode package",
        HARNESS_COMMAND_TIMEOUT,
    )
    .await?;
    std::fs::remove_file(&archive)?;
    let entrypoint = staging.join("opencode");
    std::fs::rename(staging.join("package/bin/opencode"), &entrypoint)
        .context("move the OpenCode executable out of its npm package")?;
    std::fs::remove_dir_all(staging.join("package"))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&entrypoint, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

async fn download_verified_async(
    url: &str,
    destination: &Path,
    expected: &str,
    environment: &BTreeMap<String, String>,
) -> Result<()> {
    let mut curl = tokio::process::Command::new("curl");
    curl.args([
        "--proto",
        "=https",
        "--tlsv1.2",
        "-fLsS",
        "--connect-timeout",
        "30",
        "--max-time",
        "600",
        "-o",
    ])
    .arg(destination)
    .arg(url)
    .env_clear()
    .envs(environment);
    run_bounded_checked(
        &mut curl,
        "download pinned harness runtime",
        HARNESS_COMMAND_TIMEOUT,
    )
    .await?;
    let actual = crate::worker_launch::worker_executable_digest(destination)?;
    anyhow::ensure!(
        actual == expected,
        "harness runtime checksum mismatch for {}: expected {expected}, got {actual}",
        destination.display()
    );
    Ok(())
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

    std::fs::create_dir_all(root)
        .with_context(|| format!("create managed Node.js cache {}", root.display()))?;
    let install_lock = open_lock(&root.join(INSTALL_LOCK_FILE))?;
    lock_file(&install_lock, false, "managed Node.js installer").await?;
    remove_abandoned_staging(root)?;

    if !managed_node_is_usable(&install, environment).await? {
        if install.exists() {
            let lease = open_lock(&install.join(LEASE_FILE))?;
            match lease.try_lock() {
                Ok(()) => {}
                Err(std::fs::TryLockError::WouldBlock) => bail!(
                    "managed Node.js repair deferred: {} is still in use",
                    install.display()
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
        let url = format!("https://nodejs.org/dist/v{NODE_VERSION}/{file}");
        download_verified_async(&url, &archive, field("sha256")?, environment).await?;
        let staging_root = staging.path().to_path_buf();
        let folder_for_extract = folder.clone();
        tokio::task::spawn_blocking(move || {
            extract_node_archive(&archive, &staging_root, &folder_for_extract)
        })
        .await
        .context("Node.js extraction task failed")??;
        let extracted = staging.path().join(&folder);
        if !managed_node_is_usable(&extracted, environment).await? {
            bail!(
                "downloaded Node.js archive did not contain usable node, npm, and npx executables"
            );
        }
        open_lock(&extracted.join(LEASE_FILE))?;
        match std::fs::rename(&extracted, &install) {
            Ok(()) => {}
            Err(error) if install.exists() => {
                if !managed_node_is_usable(&install, environment).await? {
                    return Err(error)
                        .with_context(|| format!("publish managed Node.js {}", install.display()));
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

async fn system_node_is_compatible(environment: &BTreeMap<String, String>) -> bool {
    let Ok(output) = bounded_env_output(
        tokio::process::Command::new("node").arg("--version"),
        environment,
    )
    .await
    else {
        return false;
    };
    if !output.status.success()
        || !node_version_is_supported(String::from_utf8_lossy(&output.stdout).trim())
    {
        return false;
    }
    for command in ["npm", "npx"] {
        let Ok(output) = bounded_env_output(
            tokio::process::Command::new(command).arg("--version"),
            environment,
        )
        .await
        else {
            return false;
        };
        if !output.status.success() {
            return false;
        }
    }
    true
}

async fn managed_node_is_usable(
    install: &Path,
    environment: &BTreeMap<String, String>,
) -> Result<bool> {
    let bin = install.join("bin");
    if ["node", "npm", "npx"].iter().any(|name| {
        let path = bin.join(name);
        !entrypoint_is_executable(&path)
    }) {
        return Ok(false);
    }
    let mut probe_environment = environment.clone();
    prepend_path(&mut probe_environment, &bin)?;
    let mut node = tokio::process::Command::new(bin.join("node"));
    node.arg("--version");
    let output = bounded_env_output(&mut node, &probe_environment).await?;
    if !output.status.success()
        || !node_version_is_supported(String::from_utf8_lossy(&output.stdout).trim())
    {
        return Ok(false);
    }
    for name in ["npm", "npx"] {
        let mut command = tokio::process::Command::new(bin.join(name));
        command.arg("--version");
        let output = bounded_env_output(&mut command, &probe_environment).await?;
        if !output.status.success() {
            return Ok(false);
        }
    }
    Ok(true)
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
    let script = download_installer(
        "https://code.kimi.com/kimi-code/install.sh",
        environment,
        "download Kimi Code installer",
    )
    .await?;
    let mut bash = tokio::process::Command::new("bash");
    bash.env_clear()
        .envs(environment)
        .env("KIMI_VERSION", KIMI_VERSION)
        .env("KIMI_INSTALL_DIR", staging)
        .env("KIMI_CODE_HOME", staging)
        .env("KIMI_NO_MODIFY_PATH", "1")
        .current_dir(staging);
    run_bounded_with_input_checked(
        &mut bash,
        &script,
        "install exact managed Kimi Code",
        MANAGED_INSTALL_TIMEOUT,
    )
    .await
}

async fn install_grok(staging: &Path, environment: &BTreeMap<String, String>) -> Result<()> {
    let script = download_installer(
        "https://x.ai/cli/install.sh",
        environment,
        "download Grok installer",
    )
    .await?;
    let isolated_home = staging.join("installer-home");
    std::fs::create_dir_all(&isolated_home).context("create isolated Grok installer home")?;
    let mut bash = tokio::process::Command::new("bash");
    bash.arg("-s")
        .arg(GROK_VERSION)
        .env_clear()
        .envs(environment)
        .env("HOME", &isolated_home)
        .env("GROK_BIN_DIR", staging.join("bin"))
        .current_dir(staging);
    run_bounded_with_input_checked(
        &mut bash,
        &script,
        "install exact managed Grok",
        MANAGED_INSTALL_TIMEOUT,
    )
    .await
}

async fn download_installer(
    url: &str,
    environment: &BTreeMap<String, String>,
    operation: &str,
) -> Result<Vec<u8>> {
    let mut curl = tokio::process::Command::new("curl");
    curl.args(["-fsSL", "--connect-timeout", "30", "--max-time", "600", url])
        .env_clear()
        .envs(environment);
    let output =
        crate::subprocess::run_bounded(&mut curl, MAX_INSTALLER_BYTES, HARNESS_COMMAND_TIMEOUT)
            .await
            .with_context(|| operation.to_owned())?;
    if !output.status.success() {
        bail!("{operation} failed: {}", output_summary(&output));
    }
    if output.stdout.is_empty() {
        bail!("{operation} returned an empty installer");
    }
    Ok(output.stdout)
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

async fn run_bounded_with_input_checked(
    command: &mut tokio::process::Command,
    input: &[u8],
    operation: &str,
    timeout: Duration,
) -> Result<()> {
    let output =
        crate::subprocess::run_bounded_with_input(command, input, 4 * 1024 * 1024, timeout)
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
    #[ignore = "downloads the pinned Grok installer into a disposable cache"]
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

    fn fake_grok_installer(root: &Path) -> BTreeMap<String, String> {
        let tools = root.join("tools");
        executable(
            &tools.join("curl"),
            r#"#!/bin/sh
cat <<'INSTALLER'
set -eu
mkdir -p "$HOME/.grok/downloads" "$GROK_BIN_DIR"
printf '#!/bin/sh\necho grok-fixture\n' > "$HOME/.grok/downloads/grok"
chmod +x "$HOME/.grok/downloads/grok"
ln -s "$HOME/.grok/downloads/grok" "$GROK_BIN_DIR/grok"
ln -s "$HOME/.grok/downloads/grok" "$GROK_BIN_DIR/agent"
INSTALLER
"#,
        );
        BTreeMap::from([("PATH".into(), format!("{}:/usr/bin:/bin", tools.display()))])
    }

    #[test]
    fn grok_launchers_execute_after_relocation_and_invalid_cache_repairs_itself() {
        let temp = tempfile::tempdir().unwrap();
        let environment = fake_grok_installer(temp.path());
        let cache = temp.path().join("cache");
        let selected = pin(HarnessKind::Grok);
        let install = cache.join("grok").join(selected.install_id);
        for attempt in 0..2 {
            let managed = if attempt == 0 {
                resolve_at(
                    &cache,
                    HarnessKind::Grok,
                    ExecutionPolicy::ConfiguredApprovals,
                    &environment,
                )
                .unwrap()
            } else {
                resolve_after_lease_release(&cache, &environment)
            };
            assert!(complete_install(&install, HarnessKind::Grok, selected).unwrap());
            assert!(std::fs::read_dir(cache.join("grok")).unwrap().all(|entry| {
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".install-")
            }));
            for launcher in ["grok", "agent"] {
                let path = install.join("bin").join(launcher);
                assert!(!std::fs::read_link(&path).unwrap().is_absolute());
                let output =
                    crate::subprocess::run_with_input(&mut Command::new(path), &[]).unwrap();
                assert!(output.status.success());
                assert_eq!(output.stdout, b"grok-fixture\n");
            }
            drop(managed);
            if attempt == 0 {
                std::fs::remove_file(install.join("bin/agent")).unwrap();
                assert!(!complete_install(&install, HarnessKind::Grok, selected).unwrap());
            }
        }
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
        let environment = fake_grok_installer(temp.path());
        let managed = resolve_at(
            &cache,
            HarnessKind::Grok,
            ExecutionPolicy::ConfiguredApprovals,
            &environment,
        )
        .unwrap();
        std::fs::remove_file(&managed.command).unwrap();
        let error = resolve_at(
            &cache,
            HarnessKind::Grok,
            ExecutionPolicy::ConfiguredApprovals,
            &environment,
        )
        .unwrap_err();
        assert!(error.to_string().contains("repair deferred"));
        assert!(managed.lease_path.exists());
        drop(managed);
        resolve_after_lease_release(&cache, &environment);
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
    fn muse_download_rejects_corrupt_payload_before_publication() {
        let temp = tempfile::tempdir().unwrap();
        let tools = temp.path().join("tools");
        executable(
            &tools.join("curl"),
            "#!/bin/sh\nwhile [ \"$1\" != '-o' ]; do shift; done\nshift\nprintf corrupted > \"$1\"\n",
        );
        executable(
            &tools.join("node"),
            "#!/bin/sh\nif [ \"$1\" = --version ]; then echo v24.22.1; fi\n",
        );
        for command in ["npm", "npx"] {
            executable(
                &tools.join(command),
                "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 11.6.2; fi\n",
            );
        }
        let environment = BTreeMap::from([("PATH".into(), tools.to_string_lossy().into_owned())]);
        let output = temp.path().join("download");
        let error = download_verified(
            "https://example.invalid/muse",
            &output,
            &"0".repeat(64),
            &environment,
        )
        .unwrap_err();
        assert!(error.to_string().contains("checksum mismatch"));
        let cache = temp.path().join("cache");
        let error = resolve_at(
            &cache,
            HarnessKind::Muse,
            ExecutionPolicy::ConfiguredApprovals,
            &environment,
        )
        .unwrap_err();
        assert!(error.to_string().contains("checksum mismatch"));
        assert!(
            !cache
                .join("muse")
                .join(pin(HarnessKind::Muse).install_id)
                .exists()
        );
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

    fn fake_curl_copying(archive: &Path, root: &Path) -> (PathBuf, BTreeMap<String, String>) {
        let tools = root.join("tools");
        executable(
            &tools.join("curl"),
            "#!/bin/sh\nwhile [ \"$1\" != '-o' ]; do shift; done\nshift\ncp \"$MJ_NODE_ARCHIVE\" \"$1\"\n",
        );
        (
            tools.clone(),
            BTreeMap::from([
                (
                    "PATH".to_owned(),
                    format!("{}:/usr/bin:/bin", tools.display()),
                ),
                (
                    "MJ_NODE_ARCHIVE".to_owned(),
                    archive.to_string_lossy().into_owned(),
                ),
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
        let marker = temp.path().join("downloaded");
        executable(
            &temp.path().join("bin/curl"),
            &format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
        );
        let mut environment = fake_node_tools(temp.path(), &temp.path().join("unused"));
        let original_path = environment["PATH"].clone();

        let runtime = run_async(ensure_node_runtime(&mut environment)).unwrap();

        assert!(runtime.is_none());
        assert_eq!(environment["PATH"], original_path);
        assert!(!marker.exists());
    }

    #[test]
    fn missing_node_is_downloaded_verified_and_prepended_once() {
        let temp = tempfile::tempdir().unwrap();
        let (archive, metadata) = fake_node_archive(temp.path());
        let (_, mut environment) = fake_curl_copying(&archive, temp.path());
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
        let (tools, curl_environment) = fake_curl_copying(&archive, temp.path());
        let mut environment = curl_environment;
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
    fn managed_node_checksum_failure_never_publishes_an_install() {
        let temp = tempfile::tempdir().unwrap();
        let (archive, mut metadata) = fake_node_archive(temp.path());
        let (_, mut environment) = fake_curl_copying(&archive, temp.path());
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
        let (_, environment) = fake_curl_copying(&archive, temp.path());
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

    fn resolve_after_lease_release(
        cache: &Path,
        environment: &BTreeMap<String, String>,
    ) -> ManagedHarness {
        let mut resolved = None;
        assert_lease_released(|| {
            match resolve_at(
                cache,
                HarnessKind::Grok,
                ExecutionPolicy::ConfiguredApprovals,
                environment,
            ) {
                Ok(managed) => {
                    resolved = Some(managed);
                    true
                }
                Err(error) if error.to_string().contains("repair deferred") => false,
                Err(error) => panic!("managed harness repair failed: {error:#}"),
            }
        });
        resolved.unwrap()
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
