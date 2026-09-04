//! Exact, target-local harness installations for remote bare workers.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use hel::hel_config::{ExecutionPolicy, HarnessKind};
use hel::hel_harness_runtime::{GROK_VERSION, HarnessPin, KIMI_VERSION, pin};
use hel::hel_worker_launch::HarnessRuntimePolicy;
use serde::{Deserialize, Serialize};

const MANIFEST_FILE: &str = "mj-harness.json";
const LEASE_FILE: &str = ".lease";
const INSTALL_LOCK_FILE: &str = ".install.lock";
const CACHE_DIR: &str = "mjolnir/harnesses";

const CODEX_PACKAGE_JSON: &[u8] = include_bytes!("../../assets/harnesses/codex/package.json");
const CODEX_PACKAGE_LOCK: &[u8] = include_bytes!("../../assets/harnesses/codex/package-lock.json");
const CLAUDE_PACKAGE_JSON: &[u8] = include_bytes!("../../assets/harnesses/claude/package.json");
const CLAUDE_PACKAGE_LOCK: &[u8] =
    include_bytes!("../../assets/harnesses/claude/package-lock.json");
const DEEPSEEK_PACKAGE_JSON: &[u8] = include_bytes!("../../assets/harnesses/deepseek/package.json");
const DEEPSEEK_PACKAGE_LOCK: &[u8] =
    include_bytes!("../../assets/harnesses/deepseek/package-lock.json");

#[derive(Debug)]
pub(crate) struct ManagedHarness {
    pub command: PathBuf,
    pub args: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub lease_path: PathBuf,
    pub cache_root: PathBuf,
    _lease: File,
}

pub(crate) fn spawn_gc(root: PathBuf, harness: HarnessKind) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
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

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct InstallManifest {
    schema: u32,
    harness: HarnessKind,
    install_id: String,
}

pub(crate) async fn resolve(
    runtime: HarnessRuntimePolicy,
    harness: HarnessKind,
    execution_policy: ExecutionPolicy,
    environment: &BTreeMap<String, String>,
) -> Result<Option<ManagedHarness>> {
    if runtime == HarnessRuntimePolicy::Ambient {
        return Ok(None);
    }
    let environment = environment.clone();
    tokio::task::spawn_blocking(move || {
        let root = cache_root()?;
        resolve_at(&root, harness, execution_policy, &environment)
    })
    .await
    .context("managed harness preparation task failed")?
    .map(Some)
}

fn cache_root() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_CACHE_HOME") {
        Some(path) if !path.is_empty() => PathBuf::from(path),
        _ => PathBuf::from(
            std::env::var_os("HOME")
                .filter(|path| !path.is_empty())
                .context("managed harness installation needs HOME or XDG_CACHE_HOME")?,
        )
        .join(".cache"),
    };
    if !base.is_absolute() {
        bail!(
            "managed harness cache root must be absolute: {}",
            base.display()
        );
    }
    Ok(base.join(CACHE_DIR))
}

fn resolve_at(
    root: &Path,
    harness: HarnessKind,
    execution_policy: ExecutionPolicy,
    environment: &BTreeMap<String, String>,
) -> Result<ManagedHarness> {
    let selected = pin(harness);
    let harness_root = root.join(harness.id());
    std::fs::create_dir_all(&harness_root)
        .with_context(|| format!("create managed {} cache", harness.display_name()))?;
    let install_lock = open_lock(&harness_root.join(INSTALL_LOCK_FILE))?;
    install_lock
        .lock()
        .with_context(|| format!("lock managed {} installer", harness.display_name()))?;
    remove_abandoned_staging(&harness_root)?;

    let install = harness_root.join(selected.install_id);
    if !complete_install(&install, harness, selected)? {
        if install.exists() {
            std::fs::remove_dir_all(&install).with_context(|| {
                format!("remove incomplete managed harness {}", install.display())
            })?;
        }
        install_into(&harness_root, &install, harness, selected, environment)?;
    }
    gc_harness_root(&harness_root, selected.install_id)?;

    let lease_path = install.join(LEASE_FILE);
    let lease = open_lock(&lease_path)?;
    lease
        .lock_shared()
        .with_context(|| format!("lease managed harness {}", install.display()))?;
    drop(install_lock);

    let mut launch_environment = BTreeMap::new();
    if harness == HarnessKind::Codex {
        launch_environment.insert(
            "CODEX_PATH".to_owned(),
            install
                .join("node_modules/@openai/codex/bin/codex.js")
                .to_string_lossy()
                .into_owned(),
        );
    }
    Ok(ManagedHarness {
        command: install.join(selected.entrypoint),
        args: harness
            .bridge_args(execution_policy)
            .into_iter()
            .map(str::to_owned)
            .collect(),
        environment: launch_environment,
        lease_path,
        cache_root: root.to_path_buf(),
        _lease: lease,
    })
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

fn install_into(
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
        HarnessKind::Codex => install_npm(
            staging.path(),
            CODEX_PACKAGE_JSON,
            CODEX_PACKAGE_LOCK,
            environment,
        )?,
        HarnessKind::Claude => install_npm(
            staging.path(),
            CLAUDE_PACKAGE_JSON,
            CLAUDE_PACKAGE_LOCK,
            environment,
        )?,
        HarnessKind::Deepseek => install_npm(
            staging.path(),
            DEEPSEEK_PACKAGE_JSON,
            DEEPSEEK_PACKAGE_LOCK,
            environment,
        )?,
        HarnessKind::Kimi => install_kimi(staging.path(), environment)?,
        HarnessKind::Grok => install_grok(staging.path(), environment)?,
    }
    validate_entrypoint(staging.path(), selected, harness)?;
    open_lock(&staging.path().join(LEASE_FILE))?;
    let manifest = InstallManifest {
        schema: 1,
        harness,
        install_id: selected.install_id.to_owned(),
    };
    let body = serde_json::to_vec_pretty(&manifest)?;
    hel::hel_config::atomic_write(&staging.path().join(MANIFEST_FILE), &body)?;

    let staging_path = staging.keep();
    match std::fs::rename(&staging_path, final_path) {
        Ok(()) => Ok(()),
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

fn install_npm(
    staging: &Path,
    package_json: &[u8],
    package_lock: &[u8],
    environment: &BTreeMap<String, String>,
) -> Result<()> {
    require_node_22(environment)?;
    std::fs::write(staging.join("package.json"), package_json)
        .context("write managed harness package.json")?;
    std::fs::write(staging.join("package-lock.json"), package_lock)
        .context("write managed harness package-lock.json")?;
    let mut command = Command::new("npm");
    command
        .args([
            "ci",
            "--omit=dev",
            "--no-audit",
            "--no-fund",
            "--legacy-peer-deps",
        ])
        .current_dir(staging);
    apply_path(&mut command, environment);
    run_checked(&mut command, "install exact managed npm harness")
}

fn require_node_22(environment: &BTreeMap<String, String>) -> Result<()> {
    let mut node = Command::new("node");
    node.args([
        "-e",
        "process.exit(Number(process.versions.node.split('.')[0]) >= 22 ? 0 : 1)",
    ]);
    apply_path(&mut node, environment);
    run_checked(
        &mut node,
        "verify Node.js 22 or newer for managed harness installation",
    )?;
    let mut npm = Command::new("npm");
    npm.arg("--version");
    apply_path(&mut npm, environment);
    run_checked(&mut npm, "verify npm for managed harness installation")
}

fn install_kimi(staging: &Path, environment: &BTreeMap<String, String>) -> Result<()> {
    let script = download_installer(
        "https://code.kimi.com/kimi-code/install.sh",
        environment,
        "download Kimi Code installer",
    )?;
    let mut bash = Command::new("bash");
    bash.env("KIMI_VERSION", KIMI_VERSION)
        .env("KIMI_INSTALL_DIR", staging)
        .env("KIMI_CODE_HOME", staging)
        .env("KIMI_NO_MODIFY_PATH", "1")
        .current_dir(staging);
    apply_path(&mut bash, environment);
    run_with_input_checked(&mut bash, &script, "install exact managed Kimi Code")
}

fn install_grok(staging: &Path, environment: &BTreeMap<String, String>) -> Result<()> {
    let script = download_installer(
        "https://x.ai/cli/install.sh",
        environment,
        "download Grok installer",
    )?;
    let isolated_home = staging.join("installer-home");
    std::fs::create_dir_all(&isolated_home).context("create isolated Grok installer home")?;
    let mut bash = Command::new("bash");
    bash.arg("-s")
        .arg(GROK_VERSION)
        .env("HOME", &isolated_home)
        .env("GROK_BIN_DIR", staging.join("bin"))
        .current_dir(staging);
    apply_path(&mut bash, environment);
    run_with_input_checked(&mut bash, &script, "install exact managed Grok")
}

fn download_installer(
    url: &str,
    environment: &BTreeMap<String, String>,
    operation: &str,
) -> Result<Vec<u8>> {
    let mut curl = Command::new("curl");
    curl.args(["-fsSL", url]);
    apply_path(&mut curl, environment);
    let output = hel::hel_subprocess::run_with_input(&mut curl, &[])
        .with_context(|| operation.to_owned())?;
    if !output.status.success() {
        bail!("{operation} failed: {}", output_summary(&output));
    }
    if output.stdout.is_empty() {
        bail!("{operation} returned an empty installer");
    }
    Ok(output.stdout)
}

fn run_checked(command: &mut Command, operation: &str) -> Result<()> {
    run_with_input_checked(command, &[], operation)
}

fn run_with_input_checked(command: &mut Command, input: &[u8], operation: &str) -> Result<()> {
    let output = hel::hel_subprocess::run_with_input(command, input)
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

fn apply_path(command: &mut Command, environment: &BTreeMap<String, String>) {
    if let Some(path) = environment.get("PATH") {
        command.env("PATH", path);
    }
}

fn complete_install(path: &Path, harness: HarnessKind, selected: HarnessPin) -> Result<bool> {
    let body = match std::fs::read(path.join(MANIFEST_FILE)) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read managed harness manifest in {}", path.display()));
        }
    };
    let manifest: InstallManifest = match serde_json::from_slice(&body) {
        Ok(manifest) => manifest,
        Err(_) => return Ok(false),
    };
    if manifest
        != (InstallManifest {
            schema: 1,
            harness,
            install_id: selected.install_id.to_owned(),
        })
    {
        return Ok(false);
    }
    Ok(entrypoint_is_executable(&path.join(selected.entrypoint)))
}

fn validate_entrypoint(path: &Path, selected: HarnessPin, harness: HarnessKind) -> Result<()> {
    let entrypoint = path.join(selected.entrypoint);
    if !entrypoint_is_executable(&entrypoint) {
        bail!(
            "{} installer did not create executable {}",
            harness.display_name(),
            entrypoint.display()
        );
    }
    Ok(())
}

fn entrypoint_is_executable(path: &Path) -> bool {
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

pub(crate) fn acquire_supervisor_lease(path: &Path) -> Result<File> {
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
    use hel::hel_harness_runtime::{
        CLAUDE_ACP_VERSION, CODEX_ACP_VERSION, CODEX_CLI_VERSION, DEEPSEEK_ACP_VERSION,
        DEEPSEEK_DSH_VERSION,
    };

    fn executable(path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn complete_fake(root: &Path, harness: HarnessKind, install_id: &str, entrypoint: &str) {
        let install = root.join(install_id);
        std::fs::create_dir_all(&install).unwrap();
        executable(&install.join(entrypoint), "#!/bin/sh\nexit 0\n");
        open_lock(&install.join(LEASE_FILE)).unwrap();
        let body = serde_json::to_vec_pretty(&InstallManifest {
            schema: 1,
            harness,
            install_id: install_id.to_owned(),
        })
        .unwrap();
        hel::hel_config::atomic_write(&install.join(MANIFEST_FILE), &body).unwrap();
    }

    fn fake_node_tools(root: &Path, installs: &Path) -> BTreeMap<String, String> {
        let bin = root.join("bin");
        executable(&bin.join("node"), "#!/bin/sh\nexit 0\n");
        executable(
            &bin.join("npm"),
            &format!(
                "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 12.0.0; exit 0; fi\nprintf x >> '{}'\nmkdir -p node_modules/.bin node_modules/@openai/codex/bin\nprintf '#!/bin/sh\\nexit 0\\n' > node_modules/.bin/codex-acp\nchmod 755 node_modules/.bin/codex-acp\nprintf '// managed codex' > node_modules/@openai/codex/bin/codex.js\n",
                installs.display()
            ),
        );
        BTreeMap::from([(
            "PATH".to_owned(),
            format!("{}:/usr/bin:/bin", bin.display()),
        )])
    }

    #[test]
    fn embedded_npm_recipes_match_the_runtime_pins() {
        for (body, dependencies) in [
            (
                CODEX_PACKAGE_JSON,
                vec![
                    ("@agentclientprotocol/codex-acp", CODEX_ACP_VERSION),
                    ("@openai/codex", CODEX_CLI_VERSION),
                ],
            ),
            (
                CLAUDE_PACKAGE_JSON,
                vec![("@agentclientprotocol/claude-agent-acp", CLAUDE_ACP_VERSION)],
            ),
            (
                DEEPSEEK_PACKAGE_JSON,
                vec![
                    ("@deepseek-ai/dsh", DEEPSEEK_DSH_VERSION),
                    ("dsh-acp-server", DEEPSEEK_ACP_VERSION),
                ],
            ),
        ] {
            let package: serde_json::Value = serde_json::from_slice(body).unwrap();
            for (name, version) in dependencies {
                assert_eq!(package["dependencies"][name], version);
            }
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
        gc_harness_root(&root, "current").unwrap();
        assert!(!root.join("old").exists());
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
        let managed = resolve_at(
            &cache,
            HarnessKind::Codex,
            ExecutionPolicy::ConfiguredApprovals,
            &BTreeMap::new(),
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
        exclusive.try_lock().unwrap();
    }

    #[test]
    fn a_complete_cache_hit_does_not_execute_the_entrypoint() {
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
        let marker = temp.path().join("executed");
        executable(
            &root.join(selected.install_id).join(selected.entrypoint),
            &format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
        );

        let managed = resolve_at(
            &cache,
            HarnessKind::Codex,
            ExecutionPolicy::ConfiguredApprovals,
            &BTreeMap::new(),
        )
        .unwrap();

        assert!(!marker.exists());
        let expected_codex = root
            .join(selected.install_id)
            .join("node_modules/@openai/codex/bin/codex.js")
            .to_string_lossy()
            .into_owned();
        assert_eq!(managed.environment.get("CODEX_PATH"), Some(&expected_codex));
    }
}
