//! The shared mbx build cache for Rust container sessions.
//!
//! mbx wraps Cargo: a binary named `cargo` that is really `mbx` intercepts the
//! build, looks every compiler action up in a content-addressed store, and
//! restores cached outputs instead of recompiling. Its store is an ordinary
//! directory on the container host, which every mj container on that host
//! mounts read-write at the same absolute path. Nothing is synchronized
//! between hosts and mj never runs mbx garbage collection.
//!
//! Cache discovery can leave new sessions uncached. Once a cache is selected,
//! configuration failures are reported rather than launching with stale policy.

mod configuration;
pub(crate) mod service;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};

use super::cache_host::CacheHost;
use crate::targets::{self, CommandExecutor, CommandOutput, CommandSpec};
use mj_core::config::{Config, TargetBuildCache, TargetTemplate};
use mj_core::state::{
    BuildCacheApplication, BuildCacheLimit, BuildCacheOff, BuildCachePreview, BuildCacheStats,
    SessionBuildCache,
};

/// The mbx release containers run. A native mbx older than this must not share
/// the same store, so a host that has one runs its sessions without the cache.
pub(crate) const MBX_VERSION: &str = "1.16.0";

const MBX_X86_64_SHA256: &str = "be74eb96c62e774d90e8e036ed3d5541bde682802b11dfb1bf340d57276f5ab1";
const MBX_AARCH64_SHA256: &str = "01cce632e7bacacd78935226e778c5199f6942a55789645eb3e56c662f448792";

/// Overrides the download with a local mbx binary for the current machine's
/// architecture. Used for development against an unreleased mbx.
const MBX_BINARY_ENV: &str = "MJ_MBX_BINARY";

const DEFAULT_CACHE_RELATIVE: &str = ".cache/mbx";
/// mbx's running totals, relative to the cache directory.
const TALLY_RELATIVE: &str = "actions/savings/v1/tally.json";
/// The cap on the computed default total budget: 100 GB, in SI bytes.
const DEFAULT_MAX_BYTES: u64 = 100_000_000_000;
const RESOLUTION_LIFETIME: Duration = Duration::from_secs(600);
const LABEL: &str = "hel-mbx";
const UNSUPPORTED_HOST: &str = "Mjolnir's shared mbx cache requires a Linux host. Native mbx on macOS must be installed and configured separately.";

/// Ask the cache host, not the controller or a container running on that host.
fn host_supports_cache(host: &CacheHost, executor: &impl CommandExecutor) -> Result<bool> {
    let command = host.command(
        vec!["uname".into(), "-sm".into()],
        "detect build cache host platform",
    );
    let output = checked(executor.execute(&command)?, &command)?;
    let platform = targets::TargetPlatform::parse(
        std::str::from_utf8(&output.stdout).context("decode build cache host platform")?,
    )?;
    Ok(platform.os == targets::TargetOs::Linux)
}

/// What a container target's host offers as a build cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ResolvedBuildCache {
    /// Cache directory on the host, mounted at the same path in the container.
    pub directory: PathBuf,
    /// A `[target] root` the host configuration relocates outside the cache
    /// directory, which the container needs mounted at the same path too.
    pub target_root: Option<PathBuf>,
    /// Desired policy for the shared machine file, never a private session copy.
    pub config_file: Option<String>,
    pub config_directory: PathBuf,
    pub previous_config: Option<String>,
}

/// Cached host inspections, keyed by host and per-target settings. An
/// inspection runs several commands on the host, and a burst of new sessions
/// must not repeat them for each one. A failure is remembered too, so a host
/// that cannot answer is not re-probed by every session in that burst.
type Resolutions = std::collections::BTreeMap<String, (Instant, Result<Inspection, String>)>;

static RESOLUTIONS: std::sync::LazyLock<std::sync::Mutex<Resolutions>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(Resolutions::new()));

type Applications = std::collections::BTreeMap<String, Result<(), String>>;
static APPLICATIONS: std::sync::LazyLock<std::sync::Mutex<Applications>> =
    std::sync::LazyLock::new(Default::default);

/// Whether a caller can be served a memoized answer or needs the host asked
/// again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Freshness {
    /// Provisioning: an answer from the last `RESOLUTION_LIFETIME` will do.
    Memoized,
    /// The settings screen, which is read precisely when somebody has just
    /// changed something on the host. A fresh answer also replaces the
    /// memoized one, so the next session sees the same thing the screen does.
    Fresh,
}

/// The one place a host is inspected. Sessions and the settings screen differ
/// only in the freshness they ask for, so they cannot drift into reporting
/// different things about the same host.
fn inspect(
    host: &CacheHost,
    settings: &TargetBuildCache,
    freshness: Freshness,
    executor: &impl CommandExecutor,
) -> Result<Inspection> {
    let key = format!("{}|{settings:?}", host.key());
    if freshness == Freshness::Memoized
        && let Some((recorded, inspection)) = RESOLUTIONS.lock().expect("mbx resolutions").get(&key)
        && recorded.elapsed() < RESOLUTION_LIFETIME
    {
        return inspection.clone().map_err(|error| anyhow::anyhow!(error));
    }
    let inspection = inspect_host(host, settings, executor);
    let recorded = match &inspection {
        Ok(inspection) => Ok(inspection.clone()),
        Err(error) => Err(format!("{error:#}")),
    };
    RESOLUTIONS
        .lock()
        .expect("mbx resolutions")
        .insert(key, (Instant::now(), recorded));
    inspection
}

/// Resolve the build cache for one container target, or `None` when this
/// target runs without one.
pub(super) fn resolve(
    target: &targets::TargetTemplate,
    executor: &impl CommandExecutor,
) -> Option<ResolvedBuildCache> {
    let (host, settings) = supported_host(target)?;
    let inspection = match inspect(&host, &settings, Freshness::Memoized, executor) {
        Ok(inspection) => inspection,
        Err(error) => {
            tracing::warn!(host = host.key(), "build cache unavailable: {error:#}");
            return None;
        }
    };
    let Some(cache) = inspection.cache else {
        if let Some(reason) = &inspection.preview.off_reason {
            tracing::warn!(
                directory = inspection
                    .preview
                    .directory
                    .as_ref()
                    .map(|directory| directory.display().to_string()),
                "sessions on this target run without the build cache: {reason}"
            );
        }
        return None;
    };
    // Creating the directory is the one side effect a session has and the
    // settings screen does not, so it sits here rather than inside the shared
    // inspection. It runs per session because a memoized inspection says what
    // the host looked like, not that the directory still exists.
    if let Err(error) = create_directory(&host, &cache.directory, executor) {
        tracing::warn!(
            directory = %cache.directory.display(),
            "the build cache directory could not be created: {error:#}"
        );
        return None;
    }
    match apply_cache(&host, &settings, cache, executor) {
        Ok(cache) => Some(cache),
        Err(error) => {
            tracing::warn!(
                host = host.key(),
                "applying machine build cache configuration failed: {error:#}"
            );
            executor.notify_notice(&format!(
                "Build cache configuration could not be applied: {error:#}"
            ));
            None
        }
    }
}

fn apply_cache(
    host: &CacheHost,
    settings: &TargetBuildCache,
    mut cache: ResolvedBuildCache,
    executor: &impl CommandExecutor,
) -> Result<ResolvedBuildCache> {
    let key = format!("{}|{settings:?}", host.key());
    let result = (|| {
        if !configuration::apply(host, &cache, executor)? {
            // Another application won. Accept it only if it already installs
            // this policy; never replay an older desired value over a newer one.
            cache = inspect(host, settings, Freshness::Fresh, executor)?
                .cache
                .context("build cache became unavailable during application")?;
            ensure!(
                cache.previous_config == cache.config_file,
                "machine build cache policy changed during application; retry with current machine settings"
            );
        }
        cache.previous_config = cache.config_file.clone();
        if let Some((_, Ok(inspection))) =
            RESOLUTIONS.lock().expect("mbx resolutions").get_mut(&key)
        {
            inspection.cache = Some(cache.clone());
            inspection.preview.application = BuildCacheApplication::Applied;
        }
        Ok(cache)
    })();
    APPLICATIONS.lock().expect("mbx applications").insert(
        key,
        result
            .as_ref()
            .map(|_| ())
            .map_err(|error| format!("{error:#}")),
    );
    result
}

/// The targets that can share a host build cache. Apple `container` runs each
/// container in its own virtual machine, where file locks across the shared
/// store are unverified, and bare and EC2 targets are out of scope.
fn supported_host(target: &targets::TargetTemplate) -> Option<(CacheHost, TargetBuildCache)> {
    let settings = match target {
        targets::TargetTemplate::LocalPodman(container)
        | targets::TargetTemplate::LocalDocker(container)
        | targets::TargetTemplate::SshPodman { container, .. }
        | targets::TargetTemplate::SshDocker { container, .. } => {
            container.build_cache.clone().unwrap_or_default()
        }
        targets::TargetTemplate::AppleContainer(_)
        | targets::TargetTemplate::LocalBare
        | targets::TargetTemplate::AwsEc2(_)
        | targets::TargetTemplate::SshBare { .. } => return None,
    };
    Some((CacheHost::for_target(target)?, settings))
}

/// What the settings screen shows for one machine's blank build cache fields:
/// the same host inspection a session runs, without creating the directory.
/// `None` when the machine has no standing host to share a cache on.
pub fn preview_build_cache(
    machine: &mj_core::config::Machine,
    executor: &impl CommandExecutor,
) -> Result<Option<BuildCachePreview>> {
    let Some(host) = CacheHost::for_machine(machine) else {
        return Ok(None);
    };
    let settings = machine.build_cache().cloned().unwrap_or_default();
    inspect(&host, &settings, Freshness::Fresh, executor).map(|inspection| Some(inspection.preview))
}

/// Apply one machine's desired policy. Both provisioning and the daemon use
/// the same compare-and-replace operation; native settings are only read.
pub(crate) fn apply_machine_build_cache(
    machine: &mj_core::config::Machine,
    mounted_directories: &[PathBuf],
    executor: &impl CommandExecutor,
) -> Result<()> {
    let Some(host) = CacheHost::for_machine(machine) else {
        return Ok(());
    };
    let settings = machine.build_cache().cloned().unwrap_or_default();
    let inspected = inspect(&host, &settings, Freshness::Fresh, executor)?;
    if let Some(cache) = inspected.cache {
        let cache = apply_cache(&host, &settings, cache, executor)?;
        // Existing containers retain their mounts if placement changes. Publish
        // the same machine policy to each still-mounted cache, once per path.
        for directory in mounted_directories {
            if directory != &cache.directory {
                publish_at(&host, &cache, directory, executor)?;
            }
        }
    }
    Ok(())
}

fn publish_at(
    host: &CacheHost,
    cache: &ResolvedBuildCache,
    directory: &Path,
    executor: &impl CommandExecutor,
) -> Result<PathBuf> {
    let mut projected = cache.clone();
    projected.config_directory = configuration::shared_directory(directory);
    projected.previous_config = configuration::read_file(
        host,
        &projected.config_directory.join("config.toml"),
        executor,
    )?;
    ensure!(
        configuration::apply(host, &projected, executor)?,
        "machine configuration changed during application; retry with current settings"
    );
    Ok(projected.config_directory)
}

/// Upgrade an existing container through its existing cache mount. Resolving
/// ownership uses its actual host, never a target name that can be reassigned.
pub(super) fn prepare_session_configuration(
    config: &Config,
    backend: &targets::TargetLocator,
    recorded: &SessionBuildCache,
    executor: &impl CommandExecutor,
) -> Result<PathBuf> {
    let host = host_for_locator(backend).context("build cache has no container host")?;
    let mut settings = config
        .machines
        .values()
        .filter(|machine| {
            CacheHost::for_machine(machine).is_some_and(|candidate| candidate.key() == host.key())
        })
        .filter_map(|machine| machine.build_cache())
        .next()
        .cloned()
        .unwrap_or_default();
    settings.directory = Some(recorded.directory.clone());
    // A disabled cache still exists in already provisioned containers. Keep
    // its policy current until the session no longer mounts it.
    settings.enabled = Some(true);
    let inspected = inspect_host(&host, &settings, executor)?;
    let cache = inspected.cache.with_context(|| {
        format!(
            "build cache configuration unavailable: {}",
            inspected
                .preview
                .off_reason
                .map(|reason| reason.to_string())
                .unwrap_or_else(|| "host inspection returned no cache".into())
        )
    })?;
    publish_at(&host, &cache, &recorded.directory, executor)
}

/// Native mbx compatibility for a host used by configured container targets.
pub(crate) struct DoctorHostMbx {
    pub host: String,
    pub targets: Vec<String>,
    pub status: DoctorHostMbxStatus,
}

pub(crate) enum DoctorHostMbxStatus {
    Unsupported(String),
    Absent,
    Compatible(String),
    TooOld(String),
    Unknown(String),
}

/// Check each relevant host once. The cache is optional, so disabled caches
/// and hosts with no Podman or Docker target need no compatibility check.
pub(crate) fn doctor_host_mbx(
    config: &Config,
    executor: &impl CommandExecutor,
) -> Vec<DoctorHostMbx> {
    let mut hosts: std::collections::BTreeMap<String, (CacheHost, Vec<String>)> =
        std::collections::BTreeMap::new();
    let mut checks = Vec::new();
    for (id, target) in &config.targets {
        let container = match target {
            TargetTemplate::LocalPodman { container }
            | TargetTemplate::LocalDocker { container }
            | TargetTemplate::SshPodman { container, .. }
            | TargetTemplate::SshDocker { container, .. } => container,
            _ => continue,
        };
        if container
            .build_cache
            .as_ref()
            .and_then(|cache| cache.enabled)
            == Some(false)
        {
            continue;
        }
        match CacheHost::for_path_target(target) {
            Ok(host) => {
                let key = host.key();
                hosts
                    .entry(key)
                    .or_insert_with(|| (host, Vec::new()))
                    .1
                    .push(id.clone());
            }
            Err(error) => checks.push(DoctorHostMbx {
                host: id.clone(),
                targets: vec![id.clone()],
                status: DoctorHostMbxStatus::Unknown(format!("{error:#}")),
            }),
        }
    }
    checks.extend(hosts.into_iter().map(|(key, (host, targets))| {
        let status = (|| -> Result<DoctorHostMbxStatus> {
            if !host_supports_cache(&host, executor)? {
                return Ok(DoctorHostMbxStatus::Unsupported(UNSUPPORTED_HOST.into()));
            }
            Ok(match probe_native_version(&host, executor)? {
                None => DoctorHostMbxStatus::Absent,
                Some(native) if semver::Version::parse(&native.version).is_err() => {
                    DoctorHostMbxStatus::Unknown(format!(
                        "the host reported an unrecognized mbx version {:?}",
                        native.version
                    ))
                }
                Some(native) if version_at_least(&native.version, MBX_VERSION) => {
                    DoctorHostMbxStatus::Compatible(native.version)
                }
                Some(native) => DoctorHostMbxStatus::TooOld(native.version),
            })
        })()
        .unwrap_or_else(|error| DoctorHostMbxStatus::Unknown(format!("{error:#}")));
        DoctorHostMbx {
            host: key,
            targets,
            status,
        }
    }));
    checks
}

/// Everything the host says about a target's build cache, read without
/// changing the host.
#[derive(Clone)]
struct Inspection {
    preview: BuildCachePreview,
    /// The cache a session would mount, or `None` when it runs without one.
    cache: Option<ResolvedBuildCache>,
}

fn inspect_host(
    host: &CacheHost,
    settings: &TargetBuildCache,
    executor: &impl CommandExecutor,
) -> Result<Inspection> {
    if !host_supports_cache(host, executor)? {
        return Ok(Inspection {
            preview: BuildCachePreview {
                native_mbx: None,
                directory: None,
                max_size: None,
                target_max_size: None,
                user_managed: false,
                application: BuildCacheApplication::Pending,
                budget_note: None,
                stats: None,
                off_reason: Some(BuildCacheOff::Unavailable(UNSUPPORTED_HOST.into())),
            },
            cache: None,
        });
    }
    let native = probe_native_version(host, executor)?;
    let native_version = native.as_ref().map(|native| native.version.clone());
    let off = |preview: BuildCachePreview| Inspection {
        preview,
        cache: None,
    };
    if let Some(version) = &native_version
        && !version_at_least(version, MBX_VERSION)
    {
        return Ok(off(BuildCachePreview {
            native_mbx: native_version.clone(),
            directory: None,
            max_size: None,
            target_max_size: None,
            user_managed: true,
            application: BuildCacheApplication::Pending,
            budget_note: None,
            stats: None,
            off_reason: Some(BuildCacheOff::Unavailable(format!(
                "the host's mbx {version} is older than the {MBX_VERSION} Mjolnir installs, \
                 so they cannot share a store"
            ))),
        }));
    }
    let directory = match &native {
        Some(native) => native_cache_directory(host, native, executor)?,
        None => settings
            .directory
            .clone()
            .unwrap_or(host.home(executor)?.join(DEFAULT_CACHE_RELATIVE)),
    };
    ensure!(
        directory.is_absolute(),
        "build cache directory {} is not absolute",
        directory.display()
    );

    let user_managed = native.is_some();
    let config_directory = configuration::shared_directory(&directory);
    let previous_config =
        configuration::read_file(host, &config_directory.join("config.toml"), executor)?;
    let (config_file, limit) = if user_managed {
        let text = host_config_file(host, executor)?;
        let limit = match configuration::configured_limit(text.as_deref(), "gc", "max_total_size")?
        {
            Some(size) => BuildCacheLimit::HostConfiguration(Some(size)),
            None => BuildCacheLimit::MbxDefault(None),
        };
        (Some(text.unwrap_or_default()), limit)
    } else {
        let automatic = match configuration::automatic_total(previous_config.as_deref()) {
            Some(size) => size,
            None => default_max_size(host, &directory, executor)?,
        };
        let limit = match &settings.max_size {
            Some(size) => BuildCacheLimit::Size(size.clone()),
            None => BuildCacheLimit::MjDefault(automatic.clone()),
        };
        (
            Some(configuration::managed_document(settings, &automatic)?),
            limit,
        )
    };
    let target_root = config_file
        .as_deref()
        .and_then(|text| relocated_target_root(text, &directory));
    let configured_target =
        configuration::configured_limit(config_file.as_deref(), "target", "max_size")?;
    let target_limit = match configured_target {
        Some(size) if user_managed => Some(BuildCacheLimit::HostConfiguration(Some(size))),
        Some(size) => Some(BuildCacheLimit::Size(size)),
        None => {
            let disk = configuration::disk_total(
                host,
                target_root.as_deref().unwrap_or(&directory),
                executor,
            );
            match disk {
                Ok(total) => Some(BuildCacheLimit::MbxDefault(Some(
                    configuration::scaled_budget(Some(total), 10, 10, 100, 30),
                ))),
                Err(error) => {
                    tracing::warn!(
                        host = host.key(),
                        "could not resolve the worktree default: {error:#}"
                    );
                    None
                }
            }
        }
    };
    let mut application =
        configuration::application(previous_config.as_deref(), config_file.as_deref());
    if application == BuildCacheApplication::Pending
        && let Some(Err(error)) = APPLICATIONS
            .lock()
            .expect("mbx applications")
            .get(&format!("{}|{settings:?}", host.key()))
    {
        application = BuildCacheApplication::Failed(error.clone());
    }
    // Read before the checks below, so a host that cannot share the cache
    // right now still reports what the cache did while it could.
    let stats = read_stats(host, &directory, executor);
    let preview = |off_reason: Option<BuildCacheOff>| {
        BuildCachePreview {
        native_mbx: native_version.clone(),
        directory: Some(directory.clone()),
        max_size: Some(limit.clone()),
        target_max_size: target_limit.clone(),
        user_managed,
        application: application.clone(),
        budget_note: Some("The combined budget also reserves shared compiler outputs and incremental state; worktrees may be collected below their own limit.".into()),
        stats: stats.clone(),
        off_reason,
    }
    };

    // The directory may not exist yet; its filesystem is its nearest
    // existing ancestor's.
    let volume = nearest_existing_ancestor(host, &directory, executor)?;
    // A machine that is not turned off still has to support the cache: an
    // explicit `enabled = true` cannot make a volume without reflinks usable.
    if !settings.enabled.unwrap_or(true) {
        return Ok(off(preview(Some(BuildCacheOff::TurnedOff))));
    }
    if !reflinks_supported(host, &volume, executor)? {
        return Ok(off(preview(Some(BuildCacheOff::Unavailable(format!(
            "the filesystem under {} does not support reflinks, so restoring cached \
             outputs would copy every byte",
            directory.display()
        ))))));
    }
    if let Some(reason) = unusable_filesystem(host, &volume, executor)? {
        return Ok(off(preview(Some(BuildCacheOff::Unavailable(format!(
            "{} is on a {reason}, where mbx's file locks are unreliable",
            directory.display()
        ))))));
    }

    // A relocated target root is a separate mount, and a restore into it is a
    // clone only when it shares one with the store. Copying instead is correct
    // and much slower, and mbx's materializer falls back to it without saying
    // so, which makes this the only place it can be noticed. It is reported
    // rather than disqualifying: a slow cache still beats no cache.
    if let Some(root) = &target_root {
        let root_volume = nearest_existing_ancestor(host, root, executor)?;
        if !cross_reflinks_supported(host, &volume, &root_volume, executor)? {
            tracing::warn!(
                cache = %directory.display(),
                target_root = %root.display(),
                "the host's mbx target root does not share a mount with the build cache, \
                 so restoring a cached output copies every byte instead of cloning it"
            );
        }
    }

    Ok(Inspection {
        preview: preview(None),
        cache: Some(ResolvedBuildCache {
            directory,
            target_root,
            config_file,
            config_directory,
            previous_config,
        }),
    })
}

/// mbx's running totals for this cache, or `None` when it has none yet.
///
/// Read from the tally file rather than by running `mbx stats`, which also
/// walks the content-addressed store to size it: that took 90 seconds on a
/// 540 GB cache here, where the tally is a few hundred bytes. It also means
/// the numbers need no mbx binary on the host.
///
/// A cache that has never been used has no tally, which is not a failure.
fn read_stats(
    host: &CacheHost,
    directory: &Path,
    executor: &impl CommandExecutor,
) -> Option<BuildCacheStats> {
    #[derive(Default, serde::Deserialize)]
    #[serde(default)]
    struct Tally {
        builds: u64,
        cached_compilations: u64,
        avoided_compiler_ns: u64,
        reflinked_bytes: u64,
    }

    let path = directory.join(TALLY_RELATIVE);
    let command = host.shell_command(
        READ_CONFIG_SCRIPT,
        LABEL,
        [path.to_string_lossy().into_owned()],
        "read the container host build cache totals",
    );
    let output = executor.execute(&command).ok()?;
    if output.status != 0 {
        return None;
    }
    // A newer mbx may add counters; unknown ones are ignored rather than
    // costing the whole report, exactly as mbx reads the file itself.
    let tally: Tally = serde_json::from_slice(&output.stdout)
        .inspect_err(|error| {
            tracing::debug!(
                path = %path.display(),
                "the build cache totals could not be read: {error}"
            );
        })
        .ok()?;
    Some(BuildCacheStats {
        builds: tally.builds,
        cached_compilations: tally.cached_compilations,
        avoided_compiler_ns: tally.avoided_compiler_ns,
        reflinked_bytes: tally.reflinked_bytes,
    })
}

/// `true` when `found` is at least `required`, comparing release versions.
fn version_at_least(found: &str, required: &str) -> bool {
    let parse = |text: &str| semver::Version::parse(text.trim()).ok();
    match (parse(found), parse(required)) {
        (Some(found), Some(required)) => found >= required,
        // An unparsable version is not evidence of a new enough mbx.
        _ => false,
    }
}

/// The host's own mbx: the program that runs it and its version.
#[derive(Debug, Clone, PartialEq, Eq)]
struct NativeMbx {
    program: String,
    version: String,
}

/// An SSH command runs in a non-login shell whose `PATH` lacks the user's
/// Cargo bin directory, so a `cargo install`ed mbx is looked up there too.
const NATIVE_VERSION_SCRIPT: &str = r#"for m in mbx "$HOME/.cargo/bin/mbx"; do
    if v=$("$m" --version 2>/dev/null); then
        printf '%s
%s' "$m" "$v"
        exit 0
    fi
done
exit 1"#;

/// The host's own mbx, or `None` when neither `PATH` nor `~/.cargo/bin`
/// has one.
fn probe_native_version(
    host: &CacheHost,
    executor: &impl CommandExecutor,
) -> Result<Option<NativeMbx>> {
    let command = host.shell_command(
        NATIVE_VERSION_SCRIPT,
        LABEL,
        [],
        "read the container host mbx version",
    );
    let output = executor.execute(&command)?;
    if output.status == 1 {
        return Ok(None);
    }
    ensure!(
        output.status == 0,
        "mbx version probe exited with status {}",
        output.status
    );
    let text = String::from_utf8_lossy(&output.stdout);
    let (program, version) = text
        .trim()
        .split_once('\n')
        .context("mbx version probe gave no version")?;
    let version = version
        .split_whitespace()
        .next_back()
        .context("mbx version probe gave an empty version")?;
    Ok(Some(NativeMbx {
        program: program.to_owned(),
        version: version.to_owned(),
    }))
}

/// The host's own cache directory. `mbx cache dir` prints the store, which is
/// the `actions` directory inside the cache directory.
fn native_cache_directory(
    host: &CacheHost,
    native: &NativeMbx,
    executor: &impl CommandExecutor,
) -> Result<PathBuf> {
    let command = host.command(
        vec![
            native.program.clone(),
            "cache".to_owned(),
            "dir".to_owned(),
            "--json".to_owned(),
        ],
        "read the container host mbx cache directory",
    );
    let output = checked(executor.execute(&command)?, &command)?;
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("parse the mbx cache directory report")?;
    let store = report
        .get("store")
        .and_then(serde_json::Value::as_str)
        .context("the mbx cache directory report has no store path")?;
    Path::new(store)
        .parent()
        .map(Path::to_path_buf)
        .with_context(|| format!("mbx store path {store:?} has no parent"))
}

const READ_CONFIG_SCRIPT: &str = r#"[ -f "$1" ] || exit 3
cat -- "$1""#;

/// The host's `~/.config/mbx/config.toml`, which containers receive verbatim
/// so their mbx uses the host's own limits. mbx has no command that prints its
/// effective configuration, so the file itself is the only accurate source.
fn host_config_file(host: &CacheHost, executor: &impl CommandExecutor) -> Result<Option<String>> {
    let directory = configuration::host_directory(host, executor)?;
    configuration::read_file(host, &directory.join("config.toml"), executor)
}

/// The `[target] root` a host configuration sets, when it lies outside the
/// cache directory and therefore needs its own mount.
fn relocated_target_root(config_file: &str, directory: &Path) -> Option<PathBuf> {
    let document: toml::Value = toml::from_str(config_file)
        .map_err(|error| tracing::warn!("the host mbx configuration is unreadable: {error}"))
        .ok()?;
    let root = document.get("target")?.get("root")?.as_str()?;
    let root = directory.join(root);
    (!root.starts_with(directory)).then_some(root)
}

const NEAREST_ANCESTOR_SCRIPT: &str = r#"d=$1
while [ ! -d "$d" ]; do
    parent=$(dirname -- "$d")
    if [ "$parent" = "$d" ]; then
        break
    fi
    d=$parent
done
printf '%s' "$d""#;

/// The deepest existing directory at or above `directory`. The cache directory
/// may not exist yet, and both `df` and the reflink probe need a real one.
fn nearest_existing_ancestor(
    host: &CacheHost,
    directory: &Path,
    executor: &impl CommandExecutor,
) -> Result<PathBuf> {
    let command = host.shell_command(
        NEAREST_ANCESTOR_SCRIPT,
        LABEL,
        [directory.to_string_lossy().into_owned()],
        "locate the build cache volume",
    );
    let output = checked(executor.execute(&command)?, &command)?;
    let path = PathBuf::from(String::from_utf8(output.stdout).context("decode cache ancestor")?);
    ensure!(
        path.is_absolute(),
        "build cache volume {} is not absolute",
        path.display()
    );
    Ok(path)
}

/// The budget mj gives a host that has no mbx configuration of its own: the
/// smaller of 100 GB and a quarter of the free space on the cache volume.
///
/// It is written as `gc.max_total_size`, so it bounds the whole cache
/// rather than the action store alone. mbx's own per-part budgets still apply
/// underneath it; they are fractions of the disk and this total is the
/// binding constraint whenever it is the smaller number.
fn default_max_size(
    host: &CacheHost,
    directory: &Path,
    executor: &impl CommandExecutor,
) -> Result<String> {
    let volume = nearest_existing_ancestor(host, directory, executor)?;
    let command = host.command(
        vec![
            "df".to_owned(),
            "-B1".to_owned(),
            "-P".to_owned(),
            "--".to_owned(),
            volume.to_string_lossy().into_owned(),
        ],
        "measure the build cache volume",
    );
    let output = checked(executor.execute(&command)?, &command)?;
    let available = available_bytes(&String::from_utf8_lossy(&output.stdout))
        .context("read the free space on the build cache volume")?;
    Ok(format!("{}B", DEFAULT_MAX_BYTES.min(available / 4)))
}

/// The available column of `df -B1 -P` output, which is the fourth field of
/// the row after the header. A long device name wraps in some `df`
/// implementations, so the fields are counted from the end of the last row.
fn available_bytes(report: &str) -> Option<u64> {
    let row = report
        .lines()
        .filter(|line| !line.trim().is_empty())
        .nth(1)?;
    let fields = row.split_whitespace().collect::<Vec<_>>();
    // ... size used available capacity mounted-on
    let available = fields.get(fields.len().checked_sub(3)?)?;
    available.parse().ok()
}

const REFLINK_SCRIPT: &str = r#"dir=$1
d=$(mktemp -d "$dir/.mj-reflink.XXXXXX") || exit 1
printf x > "$d/a" && cp --reflink=always "$d/a" "$d/b"
status=$?
rm -rf -- "$d"
exit $status"#;

/// Whether the cache volume can clone files instead of copying their bytes.
/// Reflinks are what make restoring a cached output nearly free, so a host
/// without them defaults to running without the cache.
fn reflinks_supported(
    host: &CacheHost,
    volume: &Path,
    executor: &impl CommandExecutor,
) -> Result<bool> {
    let command = host.shell_command(
        REFLINK_SCRIPT,
        LABEL,
        [volume.to_string_lossy().into_owned()],
        "probe the build cache volume for reflinks",
    );
    Ok(executor.execute(&command)?.status == 0)
}

const CROSS_REFLINK_SCRIPT: &str = r#"src=$1
dst=$2
s=$(mktemp -d "$src/.mj-reflink.XXXXXX") || exit 1
d=$(mktemp -d "$dst/.mj-reflink.XXXXXX") || { rm -rf -- "$s"; exit 1; }
printf x > "$s/a" && cp --reflink=always "$s/a" "$d/b"
status=$?
rm -rf -- "$s" "$d"
exit $status"#;

/// Whether a cached output can be cloned from the store into the managed
/// target root instead of copied. `FICLONE` fails across two mounts even when
/// both are the same filesystem, so this asks the pair rather than each side.
fn cross_reflinks_supported(
    host: &CacheHost,
    store: &Path,
    target_root: &Path,
    executor: &impl CommandExecutor,
) -> Result<bool> {
    let command = host.shell_command(
        CROSS_REFLINK_SCRIPT,
        LABEL,
        [
            store.to_string_lossy().into_owned(),
            target_root.to_string_lossy().into_owned(),
        ],
        "probe the managed target root for reflinks from the build cache",
    );
    Ok(executor.execute(&command)?.status == 0)
}

fn create_directory(
    host: &CacheHost,
    directory: &Path,
    executor: &impl CommandExecutor,
) -> Result<()> {
    let command = host.command(
        vec![
            "mkdir".to_owned(),
            "-p".to_owned(),
            "--".to_owned(),
            directory.to_string_lossy().into_owned(),
        ],
        "create the build cache directory",
    );
    checked(executor.execute(&command)?, &command).map(|_| ())
}

/// A filesystem mbx cannot use. It refuses NFS outright, and file locks over
/// FUSE, virtiofs, and 9p are unreliable, which a shared store depends on.
fn unusable_filesystem(
    host: &CacheHost,
    directory: &Path,
    executor: &impl CommandExecutor,
) -> Result<Option<&'static str>> {
    let filesystems =
        targets::probe_filesystem_types(host.ssh(), &[directory.to_path_buf()], executor)?;
    let filesystem = filesystems
        .first()
        .context("the filesystem probe named no filesystem")?;
    // `overlay_unsupported_filesystem` already groups virtiofs and 9p with the
    // network filesystems. The other reasons it gives are about stacking an
    // overlay, which a plain read-write bind mount does not do.
    Ok(targets::overlay_unsupported_filesystem(filesystem)
        .filter(|reason| matches!(*reason, "network filesystem" | "FUSE filesystem")))
}

fn checked(output: CommandOutput, command: &CommandSpec) -> Result<CommandOutput> {
    if output.status == 0 {
        return Ok(output);
    }
    bail!(
        "{} failed with status {}: {}",
        command.purpose,
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

// -- the pinned mbx binary ------------------------------------------------

/// The mbx binary to install in a container of this architecture, downloading
/// and verifying the pinned release on first use.
pub(super) fn binary_for(
    locator: &targets::TargetLocator,
    executor: &impl CommandExecutor,
) -> Result<PathBuf> {
    let triple = super::worker_binary::target_architecture(locator, executor)?;
    if let Some(path) = std::env::var_os(MBX_BINARY_ENV) {
        let path = PathBuf::from(path);
        ensure!(
            path.is_file(),
            "{MBX_BINARY_ENV} does not name a file: {}",
            path.display()
        );
        if triple == host_architecture() {
            return Ok(path);
        }
        tracing::warn!(
            triple,
            "{MBX_BINARY_ENV} is for this machine's architecture; downloading the pinned mbx \
             for the target instead"
        );
    }
    download(triple)
}

/// This machine's architecture in the same spelling `target_architecture`
/// reports, so a local override is not handed to a foreign container.
fn host_architecture() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "x86_64"
    }
}

fn release_url(triple: &str) -> String {
    format!(
        "https://github.com/jdx/mr-boxington/releases/download/v{MBX_VERSION}/mbx-{triple}-unknown-linux-musl.tar.gz"
    )
}

fn expected_digest(triple: &str) -> Result<&'static str> {
    match triple {
        "x86_64" => Ok(MBX_X86_64_SHA256),
        "aarch64" => Ok(MBX_AARCH64_SHA256),
        _ => bail!("no pinned mbx release for {triple}"),
    }
}

/// Download the pinned release once into the data directory. The archive is
/// verified against the release checksum before anything is extracted.
fn download(triple: &str) -> Result<PathBuf> {
    let expected = expected_digest(triple)?;
    let directory = mj_core::config::data_dir()
        .join("mbx")
        .join(MBX_VERSION)
        .join(triple);
    let destination = directory.join("mbx");
    if destination.is_file() {
        return Ok(destination);
    }
    std::fs::create_dir_all(&directory)
        .with_context(|| format!("create the mbx cache {}", directory.display()))?;
    let url = release_url(triple);
    let archive = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()?
        .get(&url)
        .send()
        .with_context(|| format!("download {url}"))?
        .error_for_status()
        .with_context(|| format!("download {url}"))?
        .bytes()?;
    let actual = mj_core::hex::lower_hex(Sha256::digest(&archive));
    ensure!(
        actual.eq_ignore_ascii_case(expected),
        "downloaded mbx checksum mismatch: expected {expected}, got {actual}"
    );
    let binary = extract_binary(&archive)?;
    let mut temporary = tempfile::NamedTempFile::new_in(&directory)?;
    std::io::Write::write_all(&mut temporary, &binary)?;
    temporary.as_file_mut().sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(temporary.path(), std::fs::Permissions::from_mode(0o700))?;
    }
    match temporary.persist_noclobber(&destination) {
        Ok(_) => Ok(destination),
        Err(error) if destination.is_file() => {
            drop(error);
            Ok(destination)
        }
        Err(error) => Err(error.error)
            .with_context(|| format!("publish the mbx binary {}", destination.display())),
    }
}

/// The single `mbx` file from the release archive, which also carries its
/// licence texts.
fn extract_binary(archive: &[u8]) -> Result<Vec<u8>> {
    let mut reader = tar::Archive::new(flate2::read::GzDecoder::new(archive));
    for entry in reader.entries().context("read the mbx release archive")? {
        let mut entry = entry.context("read the mbx release archive")?;
        if entry.path().context("read an mbx archive path")?.as_ref() != Path::new("mbx") {
            continue;
        }
        let mut bytes = Vec::new();
        entry
            .read_to_end(&mut bytes)
            .context("read the mbx binary from its release archive")?;
        return Ok(bytes);
    }
    bail!("the mbx release archive contains no mbx binary")
}

// -- per-session decision -------------------------------------------------

/// Whether the primary repository is a Cargo workspace, read from the host
/// mirror the clone cache prepared. A repository whose manifest is not at its
/// root, and a session whose clone cache was not prepared, run without mbx.
pub(super) fn primary_repository_is_rust(
    host: &CacheHost,
    mirror: &Path,
    executor: &impl CommandExecutor,
) -> bool {
    let command = host.command(
        vec![
            "git".to_owned(),
            "--git-dir".to_owned(),
            mirror.to_string_lossy().into_owned(),
            "cat-file".to_owned(),
            "-e".to_owned(),
            "HEAD:Cargo.toml".to_owned(),
        ],
        "detect a Cargo workspace in the session repository",
    );
    matches!(executor.execute(&command), Ok(output) if output.status == 0)
}

/// Decide the build cache for one session and attach its mounts, returning the
/// placement to record on the session. Resumes and moves resolve current
/// machine policy instead of reviving a saved session budget.
pub(super) fn prepare(
    target: &targets::TargetTemplate,
    session: &mj_core::state::SessionRecord,
    bundle: Option<&targets::ProjectBundleSpec>,
    clone_cache: Option<&super::git_cache::PreparedCloneCache>,
    mounts: &mut Vec<targets::AdditionalMount>,
    executor: &impl CommandExecutor,
) -> Option<SessionBuildCache> {
    // This function prepares container mounts. Budgets always come from the
    // machine; a previously recorded budget is never revived on recreation.
    // A session at the legacy shared `/workspace` would collide with every
    // other legacy session in mbx's path-keyed records.
    session.container_workspace.as_ref()?;
    let resolved = resolve(target, executor)?;
    let host = supported_host(target)?.0;
    if session.build_cache.is_none() {
        let mirror = clone_cache?.mirror_for(&bundle?.primary)?;
        if !primary_repository_is_rust(&host, mirror, executor) {
            return None;
        }
    }
    let build_cache = SessionBuildCache {
        host: host.key(),
        directory: resolved.directory,
        max_size: None,
        target_root: resolved.target_root,
    };
    attach_mounts(&build_cache, mounts).then_some(build_cache)
}

/// Mount the cache, and a relocated target root, read-write at the same
/// absolute paths the host uses. An attached directory that already covers one
/// of those paths wins, and the session runs without the cache.
fn attach_mounts(
    build_cache: &SessionBuildCache,
    mounts: &mut Vec<targets::AdditionalMount>,
) -> bool {
    let wanted = std::iter::once(&build_cache.directory)
        .chain(build_cache.target_root.iter())
        .collect::<Vec<_>>();
    for directory in &wanted {
        if mounts.iter().any(|mount| {
            mount.destination.starts_with(directory) || directory.starts_with(&mount.destination)
        }) {
            tracing::warn!(
                directory = %directory.display(),
                "an attached directory overlaps the build cache, so this session runs without it"
            );
            return false;
        }
    }
    for directory in wanted {
        mounts.push(targets::AdditionalMount {
            source: directory.clone(),
            destination: directory.clone(),
            access: targets::MountAccess::Rw,
        });
    }
    true
}

/// `attach_mounts` for the provisioning tests, which check the container
/// arguments the mounts produce.
#[cfg(test)]
pub(super) fn attach_mounts_for_tests(
    build_cache: &SessionBuildCache,
    mounts: &mut Vec<targets::AdditionalMount>,
) -> bool {
    attach_mounts(build_cache, mounts)
}

/// Read the configuration on the host that actually owns this container.
/// The named template may have been removed or reassigned since creation.
fn host_for_locator(target: &targets::TargetLocator) -> Option<CacheHost> {
    match target {
        targets::TargetLocator::LocalPodman { .. } | targets::TargetLocator::LocalDocker { .. } => {
            Some(CacheHost::Local)
        }
        targets::TargetLocator::SshPodman { ssh, .. }
        | targets::TargetLocator::SshDocker { ssh, .. } => Some(CacheHost::Ssh(ssh.clone())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::targets::{ContainerTemplate, SshTarget, TargetTemplate};
    use mj_core::config::ImagePullPolicy;
    use std::sync::Mutex;

    /// The resolution cache is process-wide, so tests that exercise it run one
    /// at a time and start from an empty cache.
    static ISOLATED: Mutex<()> = Mutex::new(());

    fn isolated() -> std::sync::MutexGuard<'static, ()> {
        let guard = ISOLATED.lock().unwrap_or_else(|error| error.into_inner());
        RESOLUTIONS.lock().expect("mbx resolutions").clear();
        APPLICATIONS.lock().expect("mbx applications").clear();
        guard
    }

    /// Answers canned commands by a substring of their joined argument list.
    #[derive(Default)]
    struct ProbeExecutor {
        answers: Vec<(&'static str, i32, String)>,
        seen: Mutex<Vec<String>>,
    }

    impl ProbeExecutor {
        fn new(answers: &[(&'static str, i32, &str)]) -> Self {
            Self {
                answers: answers
                    .iter()
                    .map(|(needle, status, stdout)| (*needle, *status, (*stdout).to_owned()))
                    .chain(std::iter::once(("uname -sm", 0, "Linux x86_64\n".into())))
                    .collect(),
                seen: Mutex::new(Vec::new()),
            }
        }

        fn ran(&self) -> Vec<String> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl CommandExecutor for ProbeExecutor {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            let line = format!("{} {}", command.program, command.args.join(" "));
            self.seen.lock().unwrap().push(line.clone());
            // Undo the quoting added by join_remote_command for fixture matching.
            let searchable = if command.program == "ssh" {
                line.replace("'\\''", "'").replace("' '", " ")
            } else {
                line.clone()
            };
            for (needle, status, stdout) in &self.answers {
                if searchable.contains(needle) {
                    return Ok(CommandOutput {
                        status: *status,
                        stdout: stdout.clone().into_bytes(),
                        stderr: Vec::new(),
                    });
                }
            }
            Ok(CommandOutput {
                status: 127,
                stdout: Vec::new(),
                stderr: format!("no canned answer for {line}").into_bytes(),
            })
        }
    }

    fn container(build_cache: Option<TargetBuildCache>) -> ContainerTemplate {
        ContainerTemplate {
            image: "example/image:latest".into(),
            pull_policy: ImagePullPolicy::Missing,
            extra_run_args: Vec::new(),
            workspace_storage: Default::default(),
            build_cache,
        }
    }

    fn podman(build_cache: Option<TargetBuildCache>) -> TargetTemplate {
        TargetTemplate::LocalPodman(container(build_cache))
    }

    fn docker(build_cache: Option<TargetBuildCache>) -> TargetTemplate {
        TargetTemplate::LocalDocker(container(build_cache))
    }

    /// The settings draft's view of this machine with blank build cache
    /// fields.
    fn configured_local_machine() -> mj_core::config::Machine {
        serde_json::from_value(serde_json::json!({"kind": "local"})).unwrap()
    }

    /// Where a local host with no mbx configuration of its own keeps the
    /// cache: this machine's home, which the controller reads directly.
    fn default_cache_directory() -> PathBuf {
        dirs::home_dir()
            .expect("a home directory")
            .join(DEFAULT_CACHE_RELATIVE)
    }

    /// The canned answers a host with no native mbx and a reflink-capable
    /// home directory gives.
    /// A native mbx's `--version` answer at the release containers run.
    fn current_native_mbx() -> String {
        format!("mbx\nmbx {MBX_VERSION}")
    }

    fn plain_host() -> Vec<(&'static str, i32, &'static str)> {
        vec![
            ("$m\" --version", 1, ""),
            (r#"printf '%s' "$HOME""#, 0, "/home/dev"),
            ("[ -f \"$1\" ]", 3, ""),
            ("while [ ! -d", 0, "/home/dev"),
            (
                "df -B1 -P",
                0,
                "Filesystem 1B-blocks Used Available Capacity Mounted\n/dev/sda1 1000000000000 0 800000000000 20% /home\n",
            ),
            ("mj-reflink", 0, ""),
            ("mkdir -p", 0, ""),
            ("stat -f -c %T", 0, "xfs"),
        ]
    }

    #[test]
    fn darwin_hosts_skip_cache_inspection_provisioning_and_reconciliation() {
        let _isolated = isolated();
        for remote in [false, true] {
            for enabled in [None, Some(true)] {
                let settings = TargetBuildCache {
                    enabled,
                    ..Default::default()
                };
                let machine: mj_core::config::Machine = if remote {
                    serde_json::from_value(serde_json::json!({
                        "kind": "ssh", "host": "mac.test", "user": "builder", "build_cache": settings,
                    }))
                    .unwrap()
                } else {
                    mj_core::config::Machine::Local {
                        build_cache: Some(settings.clone()),
                    }
                };
                let target = if remote {
                    TargetTemplate::SshPodman {
                        ssh: SshTarget {
                            destination: "builder@mac.test".into(),
                            ssh_args: vec![],
                        },
                        container: container(Some(settings)),
                    }
                } else {
                    podman(Some(settings))
                };
                // An installed native mbx must not enable Mjolnir's integration.
                let executor = ProbeExecutor::new(&[
                    ("uname -sm", 0, "Darwin arm64\n"),
                    ("$m\" --version", 0, "mbx\nmbx 1.16.0"),
                ]);
                let preview = preview_build_cache(&machine, &executor).unwrap().unwrap();
                assert_eq!(
                    preview.off_reason,
                    Some(BuildCacheOff::Unavailable(UNSUPPORTED_HOST.into()))
                );
                assert!(preview.directory.is_none());
                assert!(resolve(&target, &executor).is_none());
                apply_machine_build_cache(&machine, &[PathBuf::from("/existing/cache")], &executor)
                    .unwrap();
                let commands = executor.ran();
                assert!(!commands.is_empty());
                assert!(
                    commands
                        .iter()
                        .all(|command| command.contains("uname") && command.contains("-sm")),
                    "{commands:?}"
                );
                assert!(
                    commands
                        .iter()
                        .all(|command| command.starts_with(if remote { "ssh " } else { "uname " }))
                );
            }
        }
    }

    #[test]
    fn linux_ssh_cache_remains_available_on_any_controller_platform() {
        let _isolated = isolated();
        let executor = ProbeExecutor::new(&plain_host());
        let target = TargetTemplate::SshDocker {
            ssh: SshTarget {
                destination: "builder@linux.test".into(),
                ssh_args: vec![],
            },
            container: container(None),
        };
        let cache = resolve(&target, &executor).unwrap();
        assert_eq!(cache.directory, PathBuf::from("/home/dev/.cache/mbx"));
        assert_eq!(cache.previous_config, cache.config_file);
        assert!(
            executor.ran().iter().all(
                |command| command.starts_with("ssh ") && command.contains("builder@linux.test")
            )
        );
    }

    #[test]
    fn recorded_darwin_cache_fails_explicitly_without_writing() {
        let executor = ProbeExecutor::new(&[("uname -sm", 0, "Darwin x86_64")]);
        let recorded = SessionBuildCache {
            host: "local".into(),
            directory: PathBuf::from("/existing/cache"),
            max_size: None,
            target_root: None,
        };
        let backend = targets::TargetLocator::LocalPodman {
            container_id: "saved-container".into(),
            workspace_storage: Default::default(),
            borrowed_from: None,
        };
        let error =
            prepare_session_configuration(&Config::default(), &backend, &recorded, &executor)
                .unwrap_err();
        assert!(format!("{error:#}").contains(UNSUPPORTED_HOST));
        assert_eq!(executor.ran(), ["uname -sm"]);
    }

    #[test]
    fn failed_platform_probe_does_not_attempt_cache_operations() {
        let executor = ProbeExecutor::new(&[("uname -sm", 1, "")]);
        assert!(inspect_host(&CacheHost::Local, &TargetBuildCache::default(), &executor).is_err());
        assert_eq!(executor.ran(), ["uname -sm"]);
    }

    #[test]
    fn installed_worker_reads_cache_configuration_from_its_recorded_host() {
        let executor = ProbeExecutor::new(&[
            ("XDG_CONFIG_HOME", 0, "/home/builder/.config/mbx"),
            ("$HOME", 0, "/home/builder"),
            ("[ -f \"$1\" ]", 0, "[gc]\nmax_total_size = '50GB'\n"),
        ]);
        let target = targets::TargetLocator::SshPodman {
            ssh: SshTarget {
                destination: "builder@recorded-cache.test".into(),
                ssh_args: vec![],
            },
            container_id: "saved-container".into(),
            workspace_storage: Default::default(),
            borrowed_from: None,
        };
        let config = host_config_file(&host_for_locator(&target).unwrap(), &executor)
            .unwrap()
            .unwrap();
        assert!(config.contains("50GB"));
        assert!(
            executor
                .seen
                .lock()
                .unwrap()
                .iter()
                .all(|command| command.contains("builder@recorded-cache.test"))
        );
    }

    #[test]
    fn a_native_mbx_supplies_the_cache_directory_and_its_own_limits() {
        let _isolated = isolated();
        let executor = ProbeExecutor::new(&[
            ("$m\" --version", 0, current_native_mbx().as_str()),
            (
                "mbx cache dir --json",
                0,
                r#"{"version":1,"store":"/mnt/fast/mbx-cache/actions"}"#,
            ),
            (r#"printf '%s' "$HOME""#, 0, "/home/dev"),
            (
                "[ -f \"$1\" ]",
                0,
                "cache_dir = \"/mnt/fast/mbx-cache\"\n[gc]\nmax_size = \"500GiB\"\n",
            ),
            ("while [ ! -d", 0, "/mnt/fast/mbx-cache"),
            ("mj-reflink", 0, ""),
            ("mkdir -p", 0, ""),
            ("stat -f -c %T", 0, "xfs"),
        ]);
        let resolved = resolve(&podman(None), &executor).unwrap();
        assert_eq!(resolved.directory, PathBuf::from("/mnt/fast/mbx-cache"));
        // The host's own configuration file carries the budget.
        assert_eq!(
            configuration::configured_limit(
                resolved.config_file.as_deref(),
                "gc",
                "max_total_size"
            )
            .unwrap(),
            None
        );
        assert_eq!(resolved.target_root, None);
        assert!(resolved.config_file.unwrap().contains("500GiB"));
        assert!(
            !executor
                .ran()
                .iter()
                .any(|line| line.contains("apply machine")),
            "a host configuration is never rewritten"
        );
    }

    #[test]
    fn looking_at_the_settings_page_lets_the_next_session_see_a_repaired_host() {
        let _isolated = isolated();
        let broken = ProbeExecutor::new(
            &plain_host()
                .into_iter()
                .map(|(needle, status, stdout)| match needle {
                    "mj-reflink" => (needle, 1, stdout),
                    _ => (needle, status, stdout),
                })
                .collect::<Vec<_>>(),
        );
        assert!(
            resolve(&podman(None), &broken).is_none(),
            "a volume that cannot clone runs without the cache"
        );

        // The host is repaired, and the user opens the machine's build cache
        // page to check.
        let repaired = ProbeExecutor::new(&plain_host());
        let preview = preview_build_cache(&configured_local_machine(), &repaired)
            .expect("the host answers")
            .expect("a local machine can hold a cache");
        assert_eq!(preview.off_reason, None);

        assert!(
            resolve(&podman(None), &repaired).is_some(),
            "the next session asks the repaired host again instead of reusing the old verdict"
        );
    }

    #[test]
    fn a_relocated_target_root_is_reported_for_its_own_mount() {
        let _isolated = isolated();
        let executor = ProbeExecutor::new(&[
            ("$m\" --version", 0, current_native_mbx().as_str()),
            (
                "mbx cache dir --json",
                0,
                r#"{"version":1,"store":"/mnt/fast/mbx-cache/actions"}"#,
            ),
            (r#"printf '%s' "$HOME""#, 0, "/home/dev"),
            (
                "[ -f \"$1\" ]",
                0,
                "[target]\nroot = \"/mnt/fast/mbx-targets\"\n",
            ),
            ("while [ ! -d", 0, "/mnt/fast/mbx-cache"),
            ("mj-reflink", 0, ""),
            ("mkdir -p", 0, ""),
            ("stat -f -c %T", 0, "xfs"),
        ]);
        let resolved = resolve(&podman(None), &executor).unwrap();
        assert_eq!(
            resolved.target_root,
            Some(PathBuf::from("/mnt/fast/mbx-targets"))
        );
    }

    #[test]
    fn a_target_root_that_cannot_be_cloned_into_still_gets_the_cache() {
        let _isolated = isolated();
        let executor = ProbeExecutor::new(&[
            ("$m\" --version", 0, current_native_mbx().as_str()),
            (
                "mbx cache dir --json",
                0,
                r#"{"version":1,"store":"/mnt/fast/mbx-cache/actions"}"#,
            ),
            (r#"printf '%s' "$HOME""#, 0, "/home/dev"),
            (
                "[ -f \"$1\" ]",
                0,
                "[target]\nroot = \"/mnt/slow/mbx-targets\"\n",
            ),
            ("while [ ! -d", 0, "/mnt/fast/mbx-cache"),
            // Cloning from the store into the relocated root fails; cloning
            // within the store still works.
            ("src=$1", 1, ""),
            ("mj-reflink", 0, ""),
            ("mkdir -p", 0, ""),
            ("stat -f -c %T", 0, "xfs"),
        ]);
        let resolved = resolve(&podman(None), &executor)
            .expect("a target root that copies instead of cloning is slower, not unusable");
        assert_eq!(
            resolved.target_root,
            Some(PathBuf::from("/mnt/slow/mbx-targets"))
        );
        assert!(
            executor.ran().iter().any(|line| line.contains("src=$1")),
            "the store and the target root are probed as a pair: {:?}",
            executor.ran()
        );
    }

    #[test]
    fn a_target_root_inside_the_cache_directory_needs_no_second_mount() {
        assert_eq!(
            relocated_target_root("[target]\nroot = \"targets\"\n", Path::new("/cache")),
            None
        );
        assert_eq!(
            relocated_target_root("[target]\nroot = \"/cache/targets\"\n", Path::new("/cache")),
            None
        );
    }

    #[test]
    fn a_cargo_installed_mbx_off_the_path_is_queried_where_it_was_found() {
        let _isolated = isolated();
        let found = format!("/home/dev/.cargo/bin/mbx\nmbx {MBX_VERSION}");
        let mut answers: Vec<(&'static str, i32, &str)> = plain_host();
        answers.retain(|(needle, _, _)| *needle != "$m\" --version");
        answers.push(("$m\" --version", 0, found.as_str()));
        answers.push((
            "/home/dev/.cargo/bin/mbx cache dir --json",
            0,
            r#"{"version":1,"store":"/mnt/fast/mbx-cache/actions"}"#,
        ));
        let executor = ProbeExecutor::new(&answers);
        let resolved = resolve(&podman(None), &executor).unwrap();
        assert_eq!(resolved.directory, PathBuf::from("/mnt/fast/mbx-cache"));
    }

    #[test]
    fn an_older_native_mbx_must_not_share_the_store() {
        let _isolated = isolated();
        let executor = ProbeExecutor::new(&[("$m\" --version", 0, "mbx\nmbx 1.15.0")]);
        assert_eq!(resolve(&podman(None), &executor), None);
    }

    #[test]
    fn a_host_without_mbx_falls_back_to_the_default_cache_directory() {
        let _isolated = isolated();
        let executor = ProbeExecutor::new(&plain_host());
        let resolved = resolve(&podman(None), &executor).unwrap();
        assert_eq!(resolved.directory, default_cache_directory());
        // min(100 GB, 800 GB / 4) is the 100 GB cap.
        assert_eq!(
            configuration::configured_limit(
                resolved.config_file.as_deref(),
                "gc",
                "max_total_size"
            )
            .unwrap()
            .as_deref(),
            Some("100000000000B")
        );
    }

    #[test]
    fn a_small_volume_takes_a_quarter_of_its_free_space() {
        let _isolated = isolated();
        let mut answers = plain_host();
        answers.retain(|(needle, _, _)| *needle != "df -B1 -P");
        answers.push((
            "df -B1 -P",
            0,
            "Filesystem 1B-blocks Used Available Capacity Mounted\n/dev/sda1 100000000 60000000 40000000 60% /home\n",
        ));
        let executor = ProbeExecutor::new(&answers);
        let resolved = resolve(&podman(None), &executor).unwrap();
        assert_eq!(
            configuration::configured_limit(
                resolved.config_file.as_deref(),
                "gc",
                "max_total_size"
            )
            .unwrap()
            .as_deref(),
            Some("10000000B")
        );
    }

    #[test]
    fn target_overrides_win_over_every_default() {
        let _isolated = isolated();
        let mut answers = plain_host();
        answers.push(("mbx cache dir", 0, r#"{"store":"/other/actions"}"#));
        let executor = ProbeExecutor::new(&answers);
        let resolved = resolve(
            &podman(Some(TargetBuildCache {
                enabled: Some(true),
                directory: Some(PathBuf::from("/mnt/nvme/mbx")),
                max_size: Some("250GiB".into()),
                target_max_size: None,
            })),
            &executor,
        )
        .unwrap();
        assert_eq!(resolved.directory, PathBuf::from("/mnt/nvme/mbx"));
        assert_eq!(
            configuration::configured_limit(
                resolved.config_file.as_deref(),
                "gc",
                "max_total_size"
            )
            .unwrap()
            .as_deref(),
            Some("250GiB")
        );
    }

    #[test]
    fn total_and_worktree_budgets_resolve_into_one_machine_document() {
        let _isolated = isolated();
        let executor = ProbeExecutor::new(&plain_host());
        let settings = TargetBuildCache {
            max_size: Some("500GiB".into()),
            target_max_size: Some("250GiB".into()),
            ..Default::default()
        };
        let inspection = inspect_host(&CacheHost::Local, &settings, &executor).unwrap();
        assert!(!inspection.preview.user_managed);
        assert_eq!(
            inspection.preview.max_size,
            Some(BuildCacheLimit::Size("500GiB".into()))
        );
        assert_eq!(
            inspection.preview.target_max_size,
            Some(BuildCacheLimit::Size("250GiB".into()))
        );
        assert_eq!(
            inspection.preview.application,
            BuildCacheApplication::Pending
        );
        let cache = inspection.cache.unwrap();
        assert_eq!(
            configuration::configured_limit(cache.config_file.as_deref(), "target", "max_size")
                .unwrap()
                .as_deref(),
            Some("250GiB")
        );
        assert!(
            !executor
                .ran()
                .iter()
                .any(|command| command.contains(".mj-apply.lock")),
            "preview never applies a setting"
        );
    }

    #[test]
    fn a_native_installation_owns_both_budgets_despite_saved_mj_overrides() {
        let _isolated = isolated();
        let native = current_native_mbx();
        let mut answers = vec![
            ("$m\" --version", 0, native.as_str()),
            (
                "mbx cache dir --json",
                0,
                r#"{"store":"/native/cache/actions"}"#,
            ),
            (
                "[ -f \"$1\" ]",
                0,
                "[gc]\nmax_total_size = '400GiB'\n[target]\nmax_size = 'none'\n",
            ),
        ];
        answers.extend(plain_host());
        let executor = ProbeExecutor::new(&answers);
        let settings = TargetBuildCache {
            directory: Some("/ignored".into()),
            max_size: Some("1GB".into()),
            target_max_size: Some("2GB".into()),
            ..Default::default()
        };
        let inspection = inspect_host(&CacheHost::Local, &settings, &executor).unwrap();
        assert!(inspection.preview.user_managed);
        assert_eq!(inspection.preview.directory, Some("/native/cache".into()));
        assert_eq!(
            inspection.preview.max_size,
            Some(BuildCacheLimit::HostConfiguration(Some("400GiB".into())))
        );
        assert_eq!(
            inspection.preview.target_max_size,
            Some(BuildCacheLimit::HostConfiguration(Some("none".into())))
        );
        assert!(
            !executor
                .ran()
                .iter()
                .any(|command| command.contains(".mj-apply.lock"))
        );
    }

    #[test]
    fn the_automatic_total_is_reused_instead_of_following_free_space() {
        let _isolated = isolated();
        let saved = configuration::managed_document(&TargetBuildCache::default(), "17GB").unwrap();
        let mut answers = vec![(".mjolnir/config/mbx/config.toml", 0, saved.as_str())];
        answers.extend(plain_host());
        let executor = ProbeExecutor::new(&answers);
        let preview = inspect_host(&CacheHost::Local, &TargetBuildCache::default(), &executor)
            .unwrap()
            .preview;
        assert_eq!(
            preview.max_size,
            Some(BuildCacheLimit::MjDefault("17GB".into()))
        );
        assert_eq!(preview.application, BuildCacheApplication::Applied);
    }

    /// Turning the cache on cannot override the host: without reflinks a
    /// restore would copy every byte, so sessions still run without it.
    #[test]
    fn an_enabled_setting_does_not_survive_a_volume_without_reflinks() {
        let _isolated = isolated();
        let mut answers = plain_host();
        answers.retain(|(needle, _, _)| *needle != "mj-reflink");
        answers.push(("mj-reflink", 1, ""));
        let executor = ProbeExecutor::new(&answers);
        assert_eq!(
            resolve(
                &podman(Some(TargetBuildCache {
                    enabled: Some(true),
                    directory: None,
                    max_size: None,
                    target_max_size: None,
                })),
                &executor,
            ),
            None
        );
    }

    #[test]
    fn a_volume_without_reflinks_runs_without_the_cache() {
        let _isolated = isolated();
        let mut answers = plain_host();
        answers.retain(|(needle, _, _)| *needle != "mj-reflink");
        answers.push(("mj-reflink", 1, ""));
        let executor = ProbeExecutor::new(&answers);
        assert_eq!(resolve(&podman(None), &executor), None);
    }

    #[test]
    fn the_preview_names_the_resolved_values_and_the_reason_the_cache_is_off() {
        let _isolated = isolated();
        let mut answers = plain_host();
        answers.retain(|(needle, _, _)| *needle != "mj-reflink");
        answers.push(("mj-reflink", 1, ""));
        let executor = ProbeExecutor::new(&answers);
        let preview = preview_build_cache(&configured_local_machine(), &executor)
            .unwrap()
            .unwrap();
        assert_eq!(preview.native_mbx, None);
        assert_eq!(preview.directory, Some(default_cache_directory()));
        assert_eq!(
            preview.max_size,
            Some(BuildCacheLimit::MjDefault("100000000000B".into()))
        );
        assert!(
            matches!(&preview.off_reason, Some(BuildCacheOff::Unavailable(reason)) if reason.contains("reflinks")),
            "{:?}",
            preview.off_reason
        );
        // A preview reads the host; it never creates the directory.
        assert!(!executor.ran().iter().any(|line| line.contains("mkdir")));

        let executor = ProbeExecutor::new(&[
            ("$m\" --version", 0, current_native_mbx().as_str()),
            (
                "mbx cache dir --json",
                0,
                r#"{"version":1,"store":"/mnt/fast/mbx-cache/actions"}"#,
            ),
            (r#"printf '%s' "$HOME""#, 0, "/home/dev"),
            ("[ -f \"$1\" ]", 0, "[gc]\nmax_size = \"500GiB\"\n"),
            ("while [ ! -d", 0, "/mnt/fast"),
            ("mj-reflink", 0, ""),
            ("stat -f -c %T", 0, "xfs"),
        ]);
        let preview = preview_build_cache(&configured_local_machine(), &executor)
            .unwrap()
            .unwrap();
        assert_eq!(preview.native_mbx.as_deref(), Some(MBX_VERSION));
        assert_eq!(
            preview.directory,
            Some(PathBuf::from("/mnt/fast/mbx-cache"))
        );
        assert_eq!(preview.max_size, Some(BuildCacheLimit::MbxDefault(None)));
        assert_eq!(preview.off_reason, None);
        assert!(!executor.ran().iter().any(|line| line.contains("mkdir")));
    }

    #[test]
    fn a_network_filesystem_runs_without_the_cache() {
        let _isolated = isolated();
        let mut answers = plain_host();
        answers.retain(|(needle, _, _)| *needle != "stat -f -c %T");
        answers.push(("stat -f -c %T", 0, "nfs4"));
        let executor = ProbeExecutor::new(&answers);
        assert_eq!(resolve(&podman(None), &executor), None);
    }

    #[test]
    fn a_machine_opt_out_does_not_disable_default_cache_policy() {
        let _isolated = isolated();
        let executor = ProbeExecutor::new(&plain_host());
        let disabled = podman(Some(TargetBuildCache {
            enabled: Some(false),
            ..Default::default()
        }));
        assert!(resolve(&disabled, &executor).is_none());
        assert!(
            !executor
                .ran()
                .iter()
                .any(|command| command.contains("mkdir -p") || command.contains(".mj-apply.lock"))
        );
        assert!(resolve(&podman(None), &executor).is_some());
        assert!(resolve(&disabled, &executor).is_none());
    }

    #[test]
    fn local_podman_and_local_docker_inspect_one_machine_once() {
        let _isolated = isolated();
        let executor = ProbeExecutor::new(&plain_host());
        let first = resolve(&podman(None), &executor).unwrap();
        let ran = executor.ran().len();
        assert!(ran > 0, "the first resolve inspects the host");
        let second = resolve(&docker(None), &executor).unwrap();
        assert_eq!(
            first, second,
            "both engines on this machine share one cache"
        );
        // The inspection is memoized; creating the directory is not, because a
        // remembered inspection says what the host looked like, not that the
        // directory still exists.
        let added = executor.ran()[ran..].to_vec();
        assert_eq!(
            added.len(),
            1,
            "the second runtime is answered from the machine's recorded inspection: {added:?}"
        );
        assert!(
            added[0].contains("mkdir -p"),
            "the one repeated command creates the directory: {added:?}"
        );
    }

    #[test]
    fn the_preview_reports_what_the_cache_has_already_done() {
        let _isolated = isolated();
        // Ahead of the configuration read, which tests the same `[ -f ]`.
        let mut answers = vec![(
            "tally.json",
            0,
            r#"{"version":1,"since_secs":1789824719,"builds":155,"cached_compilations":12050,"avoided_compiler_ns":6004997818721,"reflinked_bytes":47612059386}"#,
        )];
        answers.extend(plain_host());
        let executor = ProbeExecutor::new(&answers);
        let preview = preview_build_cache(&configured_local_machine(), &executor)
            .expect("the host answers")
            .expect("a local machine can hold a cache");
        assert_eq!(
            preview.stats,
            Some(mj_core::state::BuildCacheStats {
                builds: 155,
                cached_compilations: 12050,
                avoided_compiler_ns: 6_004_997_818_721,
                reflinked_bytes: 47_612_059_386,
            })
        );
    }

    #[test]
    fn a_cache_nothing_has_used_yet_reports_no_totals() {
        let _isolated = isolated();
        // `plain_host` answers every `[ -f ]` with 3: no configuration file
        // and no tally beside the store.
        let executor = ProbeExecutor::new(&plain_host());
        let preview = preview_build_cache(&configured_local_machine(), &executor)
            .expect("the host answers")
            .expect("a local machine can hold a cache");
        assert_eq!(preview.stats, None);
    }

    #[test]
    fn a_machine_without_a_standing_host_has_no_build_cache_preview() {
        let _isolated = isolated();
        let executor = ProbeExecutor::new(&plain_host());
        let fleet: mj_core::config::Machine = serde_json::from_value(serde_json::json!({
            "kind": "aws-ec2",
            "region": "us-east-1",
            "launch_template": "lt-1",
            "ssh_user": "ubuntu",
        }))
        .unwrap();
        assert_eq!(preview_build_cache(&fleet, &executor).unwrap(), None);
        assert!(executor.ran().is_empty());
    }

    #[test]
    fn apple_and_bare_targets_have_no_shared_build_cache() {
        let _isolated = isolated();
        let executor = ProbeExecutor::new(&plain_host());
        for target in [
            TargetTemplate::AppleContainer(container(None)),
            TargetTemplate::LocalBare,
            TargetTemplate::SshBare {
                ssh: SshTarget {
                    destination: "dev@example.test".into(),
                    ssh_args: Vec::new(),
                },
                workspace_prefix: "workspaces".into(),
            },
        ] {
            assert_eq!(resolve(&target, &executor), None, "{target:?}");
        }
        assert!(executor.ran().is_empty());
    }

    fn bundle() -> targets::ProjectBundleSpec {
        targets::ProjectBundleSpec {
            primary: "main".into(),
            repositories: vec![targets::RepositorySpec {
                url: Some("https://github.com/example/main.git".into()),
                push_urls: Vec::new(),
                destination: "main".into(),
                git_ref: None,
                reference: None,
            }],
        }
    }

    fn clone_cache() -> super::super::git_cache::PreparedCloneCache {
        super::super::git_cache::PreparedCloneCache::from_mirrors(
            [(
                "main".to_owned(),
                PathBuf::from("/home/dev/mirror/repo.git"),
            )]
            .into_iter()
            .collect(),
        )
    }

    fn session(container_workspace: Option<&str>) -> mj_core::state::SessionRecord {
        let mut record = crate::controller::test_support::checkpoint_test_session("session-1");
        record.container_workspace = container_workspace.map(PathBuf::from);
        record
    }

    #[test]
    fn a_rust_session_mounts_the_cache_at_the_host_path() {
        let _isolated = isolated();
        let mut answers = plain_host();
        answers.push(("cat-file -e HEAD:Cargo.toml", 0, ""));
        let executor = ProbeExecutor::new(&answers);
        let mut mounts = Vec::new();
        let build_cache = prepare(
            &podman(None),
            &session(Some("/workspace/session-1")),
            Some(&bundle()),
            Some(&clone_cache()),
            &mut mounts,
            &executor,
        )
        .expect("a Rust session uses the build cache");
        assert_eq!(build_cache.directory, default_cache_directory());
        assert_eq!(
            mounts,
            vec![targets::AdditionalMount {
                source: default_cache_directory(),
                destination: default_cache_directory(),
                access: targets::MountAccess::Rw,
            }]
        );
    }

    #[test]
    fn a_repository_without_a_root_manifest_runs_without_the_cache() {
        let _isolated = isolated();
        let mut answers = plain_host();
        answers.push(("cat-file -e HEAD:Cargo.toml", 1, ""));
        let executor = ProbeExecutor::new(&answers);
        let mut mounts = Vec::new();
        assert_eq!(
            prepare(
                &podman(None),
                &session(Some("/workspace/session-1")),
                Some(&bundle()),
                Some(&clone_cache()),
                &mut mounts,
                &executor,
            ),
            None
        );
        assert!(mounts.is_empty());
    }

    #[test]
    fn a_session_at_the_legacy_shared_workspace_runs_without_the_cache() {
        let _isolated = isolated();
        let mut answers = plain_host();
        answers.push(("cat-file -e HEAD:Cargo.toml", 0, ""));
        let executor = ProbeExecutor::new(&answers);
        let mut mounts = Vec::new();
        assert_eq!(
            prepare(
                &podman(None),
                &session(None),
                Some(&bundle()),
                Some(&clone_cache()),
                &mut mounts,
                &executor,
            ),
            None
        );
        assert!(executor.ran().is_empty());
    }

    #[test]
    fn a_session_without_a_prepared_clone_cache_runs_without_the_cache() {
        let _isolated = isolated();
        let mut answers = plain_host();
        answers.push(("cat-file -e HEAD:Cargo.toml", 0, ""));
        let executor = ProbeExecutor::new(&answers);
        let mut mounts = Vec::new();
        assert_eq!(
            prepare(
                &podman(None),
                &session(Some("/workspace/session-1")),
                Some(&bundle()),
                None,
                &mut mounts,
                &executor,
            ),
            None
        );
    }

    #[test]
    fn an_apple_target_never_shares_a_build_cache() {
        let _isolated = isolated();
        let mut answers = plain_host();
        answers.push(("cat-file -e HEAD:Cargo.toml", 0, ""));
        let executor = ProbeExecutor::new(&answers);
        let mut mounts = Vec::new();
        assert_eq!(
            prepare(
                &TargetTemplate::AppleContainer(container(None)),
                &session(Some("/workspace/session-1")),
                Some(&bundle()),
                Some(&clone_cache()),
                &mut mounts,
                &executor,
            ),
            None
        );
        assert!(executor.ran().is_empty());
    }

    #[test]
    fn a_resumed_session_uses_current_machine_policy_instead_of_its_saved_budget() {
        let _isolated = isolated();
        let executor = ProbeExecutor::new(&plain_host());
        let mut record = session(Some("/workspace/session-1"));
        record.build_cache = Some(SessionBuildCache {
            host: "local".into(),
            directory: default_cache_directory(),
            max_size: Some("1GB".into()),
            target_root: None,
        });
        let mut mounts = Vec::new();
        let build_cache =
            prepare(&podman(None), &record, None, None, &mut mounts, &executor).unwrap();
        assert_eq!(build_cache.max_size, None);
        assert_eq!(build_cache.directory, default_cache_directory());
        assert_eq!(mounts.len(), 1);
        assert!(
            executor
                .ran()
                .iter()
                .any(|line| line.contains(".mj-apply.lock"))
        );
    }

    #[test]
    fn a_session_moved_to_another_host_resolves_its_build_cache_again() {
        let _isolated = isolated();
        let mut answers = plain_host();
        answers.push(("cat-file -e HEAD:Cargo.toml", 0, ""));
        let executor = ProbeExecutor::new(&answers);
        let mut record = session(Some("/workspace/session-1"));
        record.build_cache = Some(SessionBuildCache {
            // The host the session was provisioned on, which the target below
            // is not.
            host: "ssh:dev@example.test".into(),
            directory: PathBuf::from("/mnt/fast/mbx-cache"),
            max_size: None,
            target_root: Some(PathBuf::from("/mnt/fast/mbx-targets")),
        });
        let mut mounts = Vec::new();

        let build_cache = prepare(
            &podman(None),
            &record,
            Some(&bundle()),
            Some(&clone_cache()),
            &mut mounts,
            &executor,
        )
        .expect("the destination host qualifies on its own");

        assert_eq!(build_cache.host, "local");
        assert_eq!(build_cache.directory, default_cache_directory());
        assert_eq!(build_cache.target_root, None);
        assert_eq!(
            mounts
                .iter()
                .map(|mount| mount.destination.clone())
                .collect::<Vec<_>>(),
            vec![default_cache_directory()]
        );
        assert!(
            executor
                .ran()
                .iter()
                .any(|line| line.contains("mj-reflink"))
        );
    }

    #[test]
    fn an_attached_directory_over_the_cache_wins() {
        let build_cache = SessionBuildCache {
            host: "local-podman".into(),
            directory: PathBuf::from("/mnt/fast/mbx-cache"),
            max_size: None,
            target_root: None,
        };
        let mut mounts = vec![targets::AdditionalMount {
            source: PathBuf::from("/elsewhere"),
            destination: PathBuf::from("/mnt/fast/mbx-cache/actions"),
            access: targets::MountAccess::Ro,
        }];
        assert!(!attach_mounts(&build_cache, &mut mounts));
        assert_eq!(mounts.len(), 1);
    }

    #[test]
    fn versions_compare_by_release_order() {
        assert!(version_at_least("1.12.0", "1.12.0"));
        assert!(version_at_least("1.12.1", "1.12.0"));
        assert!(version_at_least("2.0.0", "1.12.0"));
        assert!(!version_at_least("1.11.9", "1.12.0"));
        assert!(!version_at_least("1.9.0", "1.12.0"));
        assert!(!version_at_least("not-a-version", "1.12.0"));
    }

    #[test]
    fn free_space_is_read_from_the_available_column() {
        assert_eq!(
            available_bytes(
                "Filesystem 1B-blocks Used Available Capacity Mounted on\n\
                 /dev/sda1 1000 400 600 40% /\n"
            ),
            Some(600)
        );
        assert_eq!(available_bytes("Filesystem 1B-blocks\n"), None);
    }
}
