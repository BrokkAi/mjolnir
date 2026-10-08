//! The shared mbx build cache for Rust container sessions.
//!
//! mbx wraps Cargo: a binary named `cargo` that is really `mbx` intercepts the
//! build, looks every compiler action up in a content-addressed store, and
//! restores cached outputs instead of recompiling. Its store is an ordinary
//! directory on the container host, which every mj container on that host
//! mounts read-write at the same absolute path. Nothing is synchronized
//! between hosts and mj never runs mbx garbage collection; it only tells mbx
//! when a workspace it removed will never build again (see [`release`]).
//!
//! Cache discovery can leave new sessions uncached. Once a cache is selected,
//! configuration failures are reported rather than launching with stale policy.

mod configuration;
pub(crate) mod install;
pub(crate) mod release;
pub(crate) mod service;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};

use super::cache_host::CacheHost;
use crate::targets::{self, CommandExecutor, CommandOutput, CommandSpec};
use mj_core::config::{Config, TargetBuildCache, TargetTemplate};
use mj_core::state::{
    BuildCacheApplication, BuildCacheLimit, BuildCacheOff, BuildCachePreview, BuildCacheStats,
    SessionBuildCache,
};

/// The minimum mbx version whose cache format Mjolnir can share.
pub(crate) const MBX_VERSION: &str = "1.22.0";

const MBX_COPY_RELATIVE: &str = ".mjolnir/bin/mbx";
/// mbx's running totals, relative to the cache directory.
const TALLY_RELATIVE: &str = "actions/savings/v1/tally.json";
const RESOLUTION_LIFETIME: Duration = Duration::from_secs(600);
const LABEL: &str = "hel-mbx";
const UNSUPPORTED_HOST: &str = "Mjolnir's shared mbx cache requires a Linux host. Native mbx on macOS must be installed and configured separately.";

/// Ask the cache host, not the controller or a container running on that host.
fn host_supports_cache(host: &CacheHost, executor: &impl CommandExecutor) -> Result<bool> {
    // Windows is no Linux host and has no `uname` to ask; its container
    // engine runs in a VM this machine's paths do not reach.
    if host.ssh().is_none() && !cfg!(unix) {
        return Ok(false);
    }
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
    /// The compatible native mbx executable used to refresh the cache copy.
    pub native_mbx: NativeMbx,
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

type MbxSyncLocks = std::collections::BTreeMap<String, std::sync::Arc<std::sync::Mutex<()>>>;
static MBX_SYNC_LOCKS: std::sync::LazyLock<std::sync::Mutex<MbxSyncLocks>> =
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
    let mut preview = inspect(&host, &settings, Freshness::Fresh, executor)?.preview;
    if install::install_kind(&preview).is_some() {
        let profile = install::login_profile_details(&host, executor)?;
        preview.mbx_profile_file = Some(profile.file);
        preview.mbx_profile_warning = profile.warning;
        preview.mbx_manual_path_line = profile.manual_path_line;
    }
    Ok(Some(preview))
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
    if matches!(
        &inspected.preview.off_reason,
        Some(BuildCacheOff::Unavailable(reason)) if reason == UNSUPPORTED_HOST
    ) {
        return Ok(());
    }
    let cache = inspected
        .cache
        .map(|cache| apply_cache(&host, &settings, cache, executor))
        .transpose()?;

    let native = if let Some(cache) = &cache {
        Some(cache.native_mbx.clone())
    } else {
        match classify_native_mbx(probe_native_version(&host, executor)?) {
            NativeMbxStatus::Compatible(native) => Some(native),
            NativeMbxStatus::Absent
            | NativeMbxStatus::TooOld(_)
            | NativeMbxStatus::Unknown { .. } => None,
        }
    };
    let Some(native) = native else {
        // Existing copies remain usable by their mounted containers until the
        // host has a compatible native mbx again.
        return Ok(());
    };

    let mut directories = mounted_directories.to_vec();
    if let Some(cache) = &cache
        && !directories.contains(&cache.directory)
    {
        directories.push(cache.directory.clone());
    }
    for directory in directories {
        // Existing containers retain their mounts if placement changes. Apply
        // policy there as before, then refresh every copy those mounts expose.
        if let Some(cache) = &cache
            && directory != cache.directory
        {
            publish_at(&host, cache, &directory, executor)?;
        }
        sync_mbx_binary_from_native(&host, &native, &directory, executor)?;
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

/// The configuration file reachable through a session's shared cache mount.
pub(super) fn shared_configuration_file(directory: &Path) -> PathBuf {
    configuration::shared_directory(directory).join("config.toml")
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

/// One classification of a host probe, shared by doctor and session preview.
enum NativeMbxStatus {
    Absent,
    Compatible(NativeMbx),
    TooOld(NativeMbx),
    Unknown { version: String, reason: String },
}

fn classify_native_mbx(native: Option<NativeMbx>) -> NativeMbxStatus {
    let Some(native) = native else {
        return NativeMbxStatus::Absent;
    };
    match semver::Version::parse(&native.version) {
        Ok(_) if version_at_least(&native.version, MBX_VERSION) => {
            NativeMbxStatus::Compatible(native)
        }
        Ok(_) => NativeMbxStatus::TooOld(native),
        Err(_) => NativeMbxStatus::Unknown {
            reason: format!(
                "the host reported an unrecognized mbx version {:?}",
                native.version
            ),
            version: native.version,
        },
    }
}

pub(super) fn version_at_least(found: &str, required: &str) -> bool {
    matches!(
        (semver::Version::parse(found), semver::Version::parse(required)),
        (Ok(found), Ok(required)) if found >= required
    )
}

fn cache_unavailable_reason(status: &NativeMbxStatus) -> String {
    match status {
        NativeMbxStatus::Absent => {
            "mbx is not installed on the container host. Install mbx from Settings › Setup › Machines to enable the shared build cache.".into()
        }
        NativeMbxStatus::TooOld(native) => format!(
            "host mbx {} is older than the minimum supported version {}; upgrade mbx from Settings › Setup › Machines to enable the shared build cache",
            native.version, MBX_VERSION
        ),
        NativeMbxStatus::Unknown { reason, .. } => format!(
            "{reason}; install or upgrade mbx from Settings › Setup › Machines to enable the shared build cache"
        ),
        NativeMbxStatus::Compatible(_) => unreachable!("compatible mbx enables the cache"),
    }
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
            Ok(
                match classify_native_mbx(probe_native_version(&host, executor)?) {
                    NativeMbxStatus::Absent => DoctorHostMbxStatus::Absent,
                    NativeMbxStatus::Compatible(native) => {
                        DoctorHostMbxStatus::Compatible(native.version)
                    }
                    NativeMbxStatus::TooOld(native) => DoctorHostMbxStatus::TooOld(native.version),
                    NativeMbxStatus::Unknown { reason, .. } => DoctorHostMbxStatus::Unknown(reason),
                },
            )
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
                mbx_profile_file: None,
                mbx_profile_warning: None,
                mbx_manual_path_line: None,
                directory: None,
                max_total_size: None,
                user_managed: false,
                application: BuildCacheApplication::Pending,
                budget_note: None,
                stats: None,
                off_reason: Some(BuildCacheOff::Unavailable(UNSUPPORTED_HOST.into())),
            },
            cache: None,
        });
    }
    let off = |preview: BuildCachePreview| Inspection {
        preview,
        cache: None,
    };
    let compatibility = classify_native_mbx(probe_native_version(host, executor)?);
    let native = match compatibility {
        NativeMbxStatus::Compatible(native) => native,
        status => {
            let native_version = match &status {
                NativeMbxStatus::TooOld(native) => Some(native.version.clone()),
                NativeMbxStatus::Unknown { version, .. } => Some(version.clone()),
                NativeMbxStatus::Absent | NativeMbxStatus::Compatible(_) => None,
            };
            return Ok(off(BuildCachePreview {
                native_mbx: native_version,
                mbx_profile_file: None,
                mbx_profile_warning: None,
                mbx_manual_path_line: None,
                directory: None,
                max_total_size: None,
                user_managed: false,
                application: BuildCacheApplication::Pending,
                budget_note: None,
                stats: None,
                off_reason: Some(BuildCacheOff::Unavailable(cache_unavailable_reason(
                    &status,
                ))),
            }));
        }
    };
    let native_version = Some(native.version.clone());
    let directory = native_cache_directory(host, &native, executor)?;
    ensure!(
        directory.is_absolute(),
        "build cache directory {} is not absolute",
        directory.display()
    );

    let user_managed = true;
    let config_directory = configuration::shared_directory(&directory);
    let previous_config =
        configuration::read_file(host, &config_directory.join("config.toml"), executor)?;
    let text = host_config_file(host, executor)?;
    let limit = match configuration::configured_limit(text.as_deref(), "gc", "max_total_size")? {
        Some(size) => BuildCacheLimit::HostConfiguration(Some(size)),
        None => BuildCacheLimit::MbxDefault(None),
    };
    let config_file = Some(text.unwrap_or_default());
    let target_root = config_file
        .as_deref()
        .and_then(|text| relocated_target_root(text, &directory));
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
    let preview = |off_reason: Option<BuildCacheOff>| BuildCachePreview {
        native_mbx: native_version.clone(),
        mbx_profile_file: None,
        mbx_profile_warning: None,
        mbx_manual_path_line: None,
        directory: Some(directory.clone()),
        max_total_size: Some(limit.clone()),
        user_managed,
        application: application.clone(),
        budget_note: Some(
            "One budget covers shared compiler outputs, managed worktrees, and incremental state."
                .into(),
        ),
        stats: stats.clone(),
        off_reason,
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
            native_mbx: native,
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

/// The host's own mbx: its absolute resolved path and version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeMbx {
    pub program: PathBuf,
    pub version: String,
}

/// Find the same native executable whether it is on PATH or in a common
/// per-user install directory, then resolve symlinks before reporting it.
pub(super) const NATIVE_VERSION_SCRIPT: &str = r#"for candidate in "$(command -v mbx 2>/dev/null || true)" "$HOME/.local/bin/mbx" "$HOME/.cargo/bin/mbx"; do
    [ -n "$candidate" ] && [ -x "$candidate" ] || continue
    resolved=$(readlink -f -- "$candidate" 2>/dev/null) || continue
    case "$resolved" in /*) ;; *) continue ;; esac
    if version=$("$resolved" --version 2>/dev/null); then
        printf '%s
%s' "$resolved" "$version"
        exit 0
    fi
done
exit 1"#;

/// The host's own mbx, or `None` when no supported candidate can run.
pub(super) fn probe_native_version(
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
    parse_native_probe_output(&output.stdout).map(Some)
}

fn parse_native_probe_output(stdout: &[u8]) -> Result<NativeMbx> {
    let text = String::from_utf8_lossy(stdout);
    let (program, version) = text
        .trim()
        .split_once('\n')
        .context("mbx version probe gave no version")?;
    let version = version
        .split_whitespace()
        .next_back()
        .context("mbx version probe gave an empty version")?;
    ensure!(
        Path::new(program).is_absolute(),
        "mbx version probe returned a non-absolute executable path {program:?}"
    );
    Ok(NativeMbx {
        program: PathBuf::from(program),
        version: version.to_owned(),
    })
}

/// The current native mbx copy visible through the shared cache mount.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CachedMbxBinary {
    pub path: PathBuf,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum CachedMbxSync {
    Ready(CachedMbxBinary),
    Unavailable(String),
}

const SYNC_MBX_BINARY_SCRIPT: &str = r#"set -eu
source=$1 destination=$2 expected=$3 probe_script=$4
directory=$(dirname -- "$destination")
mkdir -p -- "$directory"
if command -v flock >/dev/null 2>&1; then
    exec 9>"$directory/.mbx.lock"
    flock -x 9
fi
probe_matches() {
    current=$(sh -c "$probe_script" 2>/dev/null) || return 1
    newline='
'
    current_program=${current%%"$newline"*}
    current_version=${current##* }
    [ "$current_program" = "$source" ] && [ "$current_version" = "$expected" ]
}
if ! probe_matches; then
    echo "native mbx changed while cache synchronization was waiting" >&2
    exit 75
fi
source_output=$("$source" --version 2>/dev/null) || {
    echo "native mbx stopped working during cache synchronization" >&2
    exit 2
}
source_version=${source_output##* }
[ "$source_version" = "$expected" ] || {
    echo "native mbx changed version during cache synchronization" >&2
    exit 75
}
source_size=$(wc -c < "$source")
if [ -f "$destination" ] && [ ! -L "$destination" ] && [ -x "$destination" ]; then
    installed_output=$("$destination" --version 2>/dev/null) || installed_output=
    installed_version=${installed_output##* }
    installed_size=$(wc -c < "$destination")
    if [ "$installed_version" = "$expected" ] && [ "$source_size" -eq "$installed_size" ]; then
        if ! probe_matches; then
            echo "native mbx changed during cache synchronization" >&2
            exit 75
        fi
        printf 'unchanged\n'
        exit 0
    fi
fi
temporary=$(mktemp "${destination}.mjolnir.XXXXXX")
trap 'rm -f -- "$temporary"' EXIT HUP INT TERM
cp -- "$source" "$temporary"
chmod 755 -- "$temporary"
temporary_output=$("$temporary" --version 2>/dev/null) || {
    echo "copied mbx cannot run on the container host" >&2
    exit 2
}
temporary_version=${temporary_output##* }
temporary_size=$(wc -c < "$temporary")
[ "$temporary_version" = "$expected" ] && [ "$source_size" -eq "$temporary_size" ] || {
    echo "copied mbx changed during cache synchronization" >&2
    exit 2
}
mv -f -- "$temporary" "$destination"
trap - EXIT HUP INT TERM
if ! probe_matches; then
    echo "native mbx changed during cache synchronization" >&2
    exit 75
fi
printf 'synced\n'"#;

pub(super) fn cache_binary_path(directory: &Path) -> PathBuf {
    directory.join(MBX_COPY_RELATIVE)
}

/// Copy the current compatible host executable into a cache directory that is
/// already mounted read-write in its containers. A missing or old native mbx
/// leaves any previous copy untouched.
pub(super) fn sync_current_mbx_binary(
    host: &CacheHost,
    directory: &Path,
    executor: &impl CommandExecutor,
) -> Result<CachedMbxSync> {
    match classify_native_mbx(probe_native_version(host, executor)?) {
        NativeMbxStatus::Compatible(native) => Ok(CachedMbxSync::Ready(
            sync_mbx_binary_from_native(host, &native, directory, executor)?,
        )),
        status => Ok(CachedMbxSync::Unavailable(cache_unavailable_reason(
            &status,
        ))),
    }
}

/// Atomically refresh one cache copy from a version that has already been
/// classified as compatible. Version and size avoid copying an unchanged
/// multi-megabyte executable on every resume or reconciliation tick.
pub(super) fn sync_mbx_binary_from_native(
    host: &CacheHost,
    native: &NativeMbx,
    directory: &Path,
    executor: &impl CommandExecutor,
) -> Result<CachedMbxBinary> {
    let mut native = match classify_native_mbx(Some(native.clone())) {
        NativeMbxStatus::Compatible(native) => native,
        status => bail!("{}", cache_unavailable_reason(&status)),
    };
    ensure!(
        directory.is_absolute(),
        "build cache directory is not absolute: {}",
        directory.display()
    );
    let path = cache_binary_path(directory);
    let key = format!("{}|{}", host.key(), path.display());
    let lock = MBX_SYNC_LOCKS
        .lock()
        .expect("mbx sync locks")
        .entry(key)
        .or_insert_with(|| std::sync::Arc::new(std::sync::Mutex::new(())))
        .clone();
    let _guard = lock.lock().unwrap_or_else(|error| error.into_inner());
    for attempt in 0..3 {
        let command = host.shell_command(
            SYNC_MBX_BINARY_SCRIPT,
            LABEL,
            [
                native.program.to_string_lossy().into_owned(),
                path.to_string_lossy().into_owned(),
                native.version.clone(),
                NATIVE_VERSION_SCRIPT.to_owned(),
            ],
            "synchronize native mbx into the shared cache",
        );
        let output = executor.execute(&command)?;
        if output.status == 0 {
            return Ok(CachedMbxBinary {
                path,
                version: native.version,
            });
        }
        ensure!(
            output.status == 75,
            "{} failed with status {}: {}",
            command.purpose,
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
        let current = probe_native_version(host, executor)?;
        native = match classify_native_mbx(current) {
            NativeMbxStatus::Compatible(native) => native,
            status => bail!("{}", cache_unavailable_reason(&status)),
        };
        if attempt == 2 {
            bail!("native mbx kept changing during cache synchronization");
        }
    }
    unreachable!("the bounded synchronization retry loop returns or fails")
}

/// The available column of a `df -P` report (the third field of its data row).
/// Callers decide the block size used by the report they requested.
pub(super) fn available_bytes(report: &str) -> Option<u64> {
    let row = report
        .lines()
        .filter(|line| !line.trim().is_empty())
        .nth(1)?;
    let fields = row.split_whitespace().collect::<Vec<_>>();
    fields.get(fields.len().checked_sub(3)?)?.parse().ok()
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
            native.program.to_string_lossy().into_owned(),
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

/// The local directories mbx uses for its cache and managed targets.
pub(crate) fn local_storage_paths() -> Vec<PathBuf> {
    let config_file = dirs::config_dir()
        .map(|directory| directory.join("mbx/config.toml"))
        .and_then(|path| std::fs::read_to_string(path).ok());
    let cache_override = std::env::var_os("MBX_CACHE_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let target_root_override = std::env::var_os("MBX_TARGET_ROOT")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let default_cache = dirs::cache_dir().map(|directory| directory.join("mbx"));

    resolve_local_storage_paths(
        cache_override,
        target_root_override,
        config_file.as_deref(),
        default_cache,
    )
}

fn resolve_local_storage_paths(
    cache_override: Option<PathBuf>,
    target_root_override: Option<PathBuf>,
    config_file: Option<&str>,
    default_cache: Option<PathBuf>,
) -> Vec<PathBuf> {
    let document = config_file.and_then(|text| toml::from_str::<toml::Value>(text).ok());
    let configured_cache = document
        .as_ref()
        .and_then(|document| document.get("cache_dir"))
        .and_then(toml::Value::as_str)
        .map(PathBuf::from);
    let Some(cache) = cache_override.or(configured_cache).or(default_cache) else {
        return Vec::new();
    };
    let configured_target_root = document
        .as_ref()
        .and_then(|document| document.get("target"))
        .and_then(|target| target.get("root"))
        .and_then(toml::Value::as_str)
        .map(PathBuf::from);
    let target_root = target_root_override
        .or(configured_target_root)
        .map(|root| {
            if root.is_absolute() {
                root
            } else {
                cache.join(root)
            }
        })
        .unwrap_or_else(|| cache.join("targets"));
    vec![cache, target_root]
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
    match sync_current_mbx_binary(&host, &resolved.directory, executor) {
        Ok(CachedMbxSync::Ready(_)) => {}
        Ok(CachedMbxSync::Unavailable(reason)) => {
            tracing::warn!(
                host = host.key(),
                "sessions run without the shared build cache: {reason}"
            );
            executor.notify_notice(&format!(
                "The shared Rust build cache is unavailable: {reason}. The session will start without it."
            ));
            return None;
        }
        Err(error) => {
            tracing::warn!(
                host = host.key(),
                "the shared mbx binary could not be synchronized; the session runs without the build cache: {error:#}"
            );
            executor.notify_notice(&format!(
                "The shared Rust build cache is unavailable: {error:#}. The session will start without it."
            ));
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
    let mut wanted = vec![build_cache.directory.clone()];
    wanted.extend(build_cache.target_root.iter().cloned());
    for destination in &wanted {
        if mounts.iter().any(|mount| {
            mount.destination.starts_with(destination)
                || destination.starts_with(&mount.destination)
        }) {
            tracing::warn!(
                destination = %destination.display(),
                "an attached directory overlaps the build cache, so this session runs without it"
            );
            return false;
        }
    }
    for directory in std::iter::once(&build_cache.directory).chain(build_cache.target_root.iter()) {
        mounts.push(targets::AdditionalMount {
            source: directory.clone(),
            destination: directory.clone(),
            access: targets::MountAccess::Rw,
        });
    }
    true
}

/// Read the configuration on the host that actually owns this container.
/// The named template may have been removed or reassigned since creation.
pub(super) fn host_for_locator(target: &targets::TargetLocator) -> Option<CacheHost> {
    match target {
        targets::TargetLocator::LocalPodman { .. } | targets::TargetLocator::LocalDocker { .. } => {
            Some(CacheHost::Local)
        }
        targets::TargetLocator::SshPodman { ssh, .. }
        | targets::TargetLocator::SshDocker { ssh, .. } => Some(CacheHost::Ssh(ssh.clone())),
        _ => None,
    }
}

/// Refresh a new-scheme session's copy from the host that owns its container.
pub(super) fn sync_mbx_binary_for_container(
    target: &targets::TargetLocator,
    directory: &Path,
    executor: &impl CommandExecutor,
) -> Result<CachedMbxSync> {
    let host = host_for_locator(target).context("build cache has no container host")?;
    sync_current_mbx_binary(&host, directory, executor)
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
            if searchable.contains("hel-mbx-profile") {
                return Ok(CommandOutput {
                    status: 0,
                    stdout: b"preview\n~/.profile\nposix\n/home/jonathan/.local/share/mbx/bin"
                        .to_vec(),
                    stderr: Vec::new(),
                });
            }
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

    /// A native mbx's `--version` answer at the release containers run.
    fn current_native_mbx() -> String {
        format!("/usr/local/bin/mbx\nmbx {MBX_VERSION}")
    }

    fn native_host() -> Vec<(&'static str, i32, &'static str)> {
        vec![
            ("$resolved\" --version", 0, "/usr/local/bin/mbx\nmbx 1.22.0"),
            (
                "mbx cache dir --json",
                0,
                r#"{"version":1,"store":"/mnt/fast/mbx-cache/actions"}"#,
            ),
            ("XDG_CONFIG_HOME", 0, "/home/dev/.config/mbx"),
            ("[ -f \"$1\" ]", 3, ""),
            ("while [ ! -d", 0, "/mnt/fast/mbx-cache"),
            ("mj-reflink", 0, ""),
            ("mkdir -p", 0, ""),
            ("stat -f -c %T", 0, "xfs"),
            ("source=$1 destination=$2 expected=$3", 0, "synced\n"),
        ]
    }

    fn absent_host() -> Vec<(&'static str, i32, &'static str)> {
        vec![("$resolved\" --version", 1, "")]
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
                    ("$resolved\" --version", 0, "mbx\nmbx 1.16.0"),
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
        let executor = ProbeExecutor::new(&native_host());
        let target = TargetTemplate::SshDocker {
            ssh: SshTarget {
                destination: "builder@linux.test".into(),
                ssh_args: vec![],
            },
            container: container(None),
        };
        let cache = resolve(&target, &executor).unwrap();
        assert_eq!(cache.directory, PathBuf::from("/mnt/fast/mbx-cache"));
        assert_eq!(cache.previous_config, cache.config_file);
        assert!(
            executor.ran().iter().all(
                |command| command.starts_with("ssh ") && command.contains("builder@linux.test")
            )
        );
    }

    #[test]
    fn reconciliation_refreshes_active_mounts_when_cache_policy_is_disabled() {
        let _isolated = isolated();
        let machine = mj_core::config::Machine::Local {
            build_cache: Some(TargetBuildCache {
                enabled: Some(false),
                ..Default::default()
            }),
        };
        let executor = ProbeExecutor::new(&native_host());
        let mounted = PathBuf::from("/existing/cache");

        apply_machine_build_cache(&machine, std::slice::from_ref(&mounted), &executor).unwrap();

        assert!(executor.ran().iter().any(|command| {
            command.contains("source=$1 destination=$2 expected=$3")
                && command.contains("/existing/cache/.mjolnir/bin/mbx")
        }));
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
            ("$resolved\" --version", 0, current_native_mbx().as_str()),
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
    // Hard-won: 24f3d72c: settings refresh left sessions using a stale failed host inspection
    #[test]
    fn looking_at_the_settings_page_lets_the_next_session_see_a_repaired_host() {
        let _isolated = isolated();
        let broken = ProbeExecutor::new(
            &native_host()
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
        let repaired = ProbeExecutor::new(&native_host());
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
    fn a_target_root_that_cannot_be_cloned_into_still_gets_the_cache() {
        let _isolated = isolated();
        let executor = ProbeExecutor::new(&[
            ("$resolved\" --version", 0, current_native_mbx().as_str()),
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
    fn local_storage_paths_follow_mbx_cache_and_target_root_resolution() {
        let default_cache = PathBuf::from("/home/dev/.cache/mbx");
        assert_eq!(
            resolve_local_storage_paths(
                None,
                None,
                Some(
                    "cache_dir = \"/mnt/optane/mbx-cache\"\n\
                     [target]\nroot = \"/mnt/optane/mbx-targets\"\n",
                ),
                Some(default_cache.clone()),
            ),
            vec![
                PathBuf::from("/mnt/optane/mbx-cache"),
                PathBuf::from("/mnt/optane/mbx-targets"),
            ]
        );
        assert_eq!(
            resolve_local_storage_paths(
                None,
                None,
                Some("cache_dir = \"/mnt/optane/mbx-cache\"\n[target]\nroot = \"views\"\n"),
                Some(default_cache.clone()),
            ),
            vec![
                PathBuf::from("/mnt/optane/mbx-cache"),
                PathBuf::from("/mnt/optane/mbx-cache/views"),
            ]
        );
        assert_eq!(
            resolve_local_storage_paths(None, None, None, Some(default_cache.clone())),
            vec![default_cache.clone(), default_cache.join("targets")]
        );
        assert_eq!(
            resolve_local_storage_paths(
                Some(PathBuf::from("/env/cache")),
                Some(PathBuf::from("views")),
                Some("cache_dir = \"/file/cache\"\n[target]\nroot = \"file-views\"\n"),
                Some(default_cache),
            ),
            vec![
                PathBuf::from("/env/cache"),
                PathBuf::from("/env/cache/views")
            ]
        );
    }

    // Hard-won: ecab42fb: non-login SSH PATH hid cargo-installed mbx and selected the wrong cache store
    #[test]
    fn a_cargo_installed_mbx_off_the_path_is_queried_where_it_was_found() {
        let _isolated = isolated();
        let found = format!("/home/dev/.cargo/bin/mbx\nmbx {MBX_VERSION}");
        let mut answers: Vec<(&'static str, i32, &str)> = native_host();
        answers.retain(|(needle, _, _)| *needle != "$resolved\" --version");
        answers.push(("$resolved\" --version", 0, found.as_str()));
        answers.push((
            "/home/dev/.cargo/bin/mbx cache dir --json",
            0,
            r#"{"version":1,"store":"/mnt/fast/mbx-cache/actions"}"#,
        ));
        let executor = ProbeExecutor::new(&answers);
        let resolved = resolve(&podman(None), &executor).unwrap();
        assert_eq!(resolved.directory, PathBuf::from("/mnt/fast/mbx-cache"));
        assert_eq!(
            resolved.native_mbx.program,
            PathBuf::from("/home/dev/.cargo/bin/mbx")
        );
    }

    #[test]
    fn an_older_native_mbx_must_not_share_the_store() {
        let _isolated = isolated();
        let executor = ProbeExecutor::new(&[
            (
                "$resolved\" --version",
                0,
                "/home/jonathan/.local/bin/mbx\nmbx 1.15.0",
            ),
            ("hel-mbx-home", 0, "/home/jonathan"),
        ]);
        assert!(resolve(&podman(None), &executor).is_none());
        let preview = preview_build_cache(&configured_local_machine(), &executor)
            .unwrap()
            .unwrap();
        assert!(matches!(
            preview.off_reason,
            Some(BuildCacheOff::Unavailable(reason))
                if reason.contains("1.15.0") && reason.contains("Settings › Setup › Machines")
        ));
    }

    #[test]
    fn a_host_without_mbx_has_no_shared_cache_and_preview_shows_setup_guidance() {
        let _isolated = isolated();
        let mut answers = absent_host();
        answers.push(("hel-mbx-home", 0, "/home/jonathan"));
        let executor = ProbeExecutor::new(&answers);
        assert!(resolve(&podman(None), &executor).is_none());
        let preview = preview_build_cache(&configured_local_machine(), &executor)
            .unwrap()
            .unwrap();
        assert_eq!(preview.native_mbx, None);
        assert_eq!(preview.directory, None);
        assert!(matches!(
            preview.off_reason,
            Some(BuildCacheOff::Unavailable(reason))
                if reason.contains("Settings › Setup › Machines")
        ));
        // The shared profile script contains a mkdir branch, but preview must
        // not invoke a separate host mkdir command for the cache directory.
        assert!(!executor.ran().iter().any(|line| line.starts_with("mkdir ")));
    }

    #[test]
    fn a_native_installation_owns_the_budget_despite_saved_mj_overrides() {
        let _isolated = isolated();
        let native = current_native_mbx();
        let mut answers = vec![
            ("$resolved\" --version", 0, native.as_str()),
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
        answers.extend(native_host());
        let executor = ProbeExecutor::new(&answers);
        let settings = TargetBuildCache {
            directory: Some("/ignored".into()),
            max_total_size: Some("1GB".into()),
            ..Default::default()
        };
        let inspection = inspect_host(&CacheHost::Local, &settings, &executor).unwrap();
        assert!(inspection.preview.user_managed);
        assert_eq!(inspection.preview.directory, Some("/native/cache".into()));
        assert_eq!(
            inspection.preview.max_total_size,
            Some(BuildCacheLimit::HostConfiguration(Some("400GiB".into())))
        );
        assert!(
            !executor
                .ran()
                .iter()
                .any(|command| command.contains(".mj-apply.lock"))
        );
    }

    /// Turning the cache on cannot override the host: without reflinks a
    /// restore would copy every byte, so sessions still run without it.
    #[test]
    fn an_enabled_setting_does_not_survive_a_volume_without_reflinks() {
        let _isolated = isolated();
        let mut answers = native_host();
        answers.retain(|(needle, _, _)| *needle != "mj-reflink");
        answers.push(("mj-reflink", 1, ""));
        let executor = ProbeExecutor::new(&answers);
        assert_eq!(
            resolve(
                &podman(Some(TargetBuildCache {
                    enabled: Some(true),
                    directory: None,
                    max_total_size: None,
                    scheduler: Default::default(),
                })),
                &executor,
            ),
            None
        );
        // An unset target preference is overridden by the same host capability.
        assert_eq!(resolve(&podman(None), &executor), None);
    }

    #[test]
    fn a_network_filesystem_runs_without_the_cache() {
        let _isolated = isolated();
        let mut answers = native_host();
        answers.retain(|(needle, _, _)| *needle != "stat -f -c %T");
        answers.push(("stat -f -c %T", 0, "nfs4"));
        let executor = ProbeExecutor::new(&answers);
        assert_eq!(resolve(&podman(None), &executor), None);
    }

    #[test]
    fn local_podman_and_local_docker_inspect_one_machine_once() {
        let _isolated = isolated();
        let executor = ProbeExecutor::new(&native_host());
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
        answers.extend(native_host());
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
        // `native_host` answers every `[ -f ]` with 3: no configuration file
        // and no tally beside the store.
        let executor = ProbeExecutor::new(&native_host());
        let preview = preview_build_cache(&configured_local_machine(), &executor)
            .expect("the host answers")
            .expect("a local machine can hold a cache");
        assert_eq!(preview.stats, None);
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
    fn a_rust_session_mounts_the_native_cache_and_records_its_synchronized_binary() {
        let _isolated = isolated();
        let mut answers = native_host();
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
        assert_eq!(build_cache.directory, PathBuf::from("/mnt/fast/mbx-cache"));
        assert_eq!(
            cache_binary_path(&build_cache.directory),
            PathBuf::from("/mnt/fast/mbx-cache/.mjolnir/bin/mbx")
        );
        assert_eq!(
            mounts,
            vec![targets::AdditionalMount {
                source: PathBuf::from("/mnt/fast/mbx-cache"),
                destination: PathBuf::from("/mnt/fast/mbx-cache"),
                access: targets::MountAccess::Rw,
            }]
        );
    }

    #[test]
    fn a_session_at_the_legacy_shared_workspace_runs_without_the_cache() {
        let _isolated = isolated();
        let mut answers = native_host();
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
    fn a_resumed_session_uses_current_machine_policy_instead_of_its_saved_budget() {
        let _isolated = isolated();
        let executor = ProbeExecutor::new(&native_host());
        let mut record = session(Some("/workspace/session-1"));
        record.build_cache = Some(SessionBuildCache {
            host: "local".into(),
            directory: PathBuf::from("/mnt/fast/mbx-cache"),
            max_size: Some("1GB".into()),
            target_root: None,
        });
        let mut mounts = Vec::new();
        let build_cache =
            prepare(&podman(None), &record, None, None, &mut mounts, &executor).unwrap();
        assert_eq!(build_cache.max_size, None);
        assert_eq!(build_cache.directory, PathBuf::from("/mnt/fast/mbx-cache"));
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
        let mut answers = native_host();
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
        assert_eq!(build_cache.directory, PathBuf::from("/mnt/fast/mbx-cache"));
        assert_eq!(build_cache.target_root, None);
        assert_eq!(
            mounts
                .iter()
                .map(|mount| mount.destination.clone())
                .collect::<Vec<_>>(),
            vec![PathBuf::from("/mnt/fast/mbx-cache")]
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
        let native = |version: &str| NativeMbx {
            program: "/usr/local/bin/mbx".into(),
            version: version.into(),
        };
        assert!(matches!(
            classify_native_mbx(Some(native(MBX_VERSION))),
            NativeMbxStatus::Compatible(_)
        ));
        assert!(matches!(
            classify_native_mbx(Some(native("1.23.0"))),
            NativeMbxStatus::Compatible(_)
        ));
        assert!(matches!(
            classify_native_mbx(Some(native("1.15.0"))),
            NativeMbxStatus::TooOld(_)
        ));
        assert!(matches!(
            classify_native_mbx(Some(native("not-a-version"))),
            NativeMbxStatus::Unknown { .. }
        ));
        assert!(matches!(classify_native_mbx(None), NativeMbxStatus::Absent));
    }

    #[test]
    fn available_bytes_reads_the_df_available_column() {
        assert_eq!(
            available_bytes(
                "Filesystem 1K-blocks Used Available Capacity Mounted on\n\
                 /dev/sda1 1000 400 600 40% /\n"
            ),
            Some(600)
        );
        assert_eq!(available_bytes("Filesystem 1K-blocks\n"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_probe_prefers_path_then_user_bins_and_canonicalizes_symlinks() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let root = tempfile::tempdir().unwrap();
        let path_bin = root.path().join("path-bin");
        let home = root.path().join("home");
        let local_bin = home.join(".local/bin");
        let cargo_bin = home.join(".cargo/bin");
        let real_bin = root.path().join("real");
        for directory in [&path_bin, &local_bin, &cargo_bin, &real_bin] {
            std::fs::create_dir_all(directory).unwrap();
        }
        let install = |path: &Path, version: &str| {
            std::fs::write(path, format!("#!/bin/sh\nprintf 'mbx {version}\\n'\n")).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        let real_mbx = real_bin.join("mbx-real");
        install(&real_mbx, "1.30.0");
        symlink(&real_mbx, path_bin.join("mbx")).unwrap();
        install(&local_bin.join("mbx"), "1.31.0");
        install(&cargo_bin.join("mbx"), "1.32.0");
        let readlink = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|directory| directory.join("readlink"))
            .find(|candidate| candidate.is_file())
            .expect("readlink utility is available");
        symlink(readlink, path_bin.join("readlink")).unwrap();

        let mut command = CommandSpec::new("/bin/sh", ["-c", NATIVE_VERSION_SCRIPT])
            .purpose("test native mbx path resolution");
        command.clear_env = true;
        command
            .env
            .insert("PATH".into(), path_bin.display().to_string());
        command
            .env
            .insert("HOME".into(), home.display().to_string());
        let executor = targets::ProcessExecutor;

        let output = executor.execute(&command).unwrap();
        assert_eq!(output.status, 0);
        assert_eq!(
            parse_native_probe_output(&output.stdout).unwrap(),
            NativeMbx {
                program: real_mbx.clone(),
                version: "1.30.0".into(),
            }
        );

        std::fs::remove_file(path_bin.join("mbx")).unwrap();
        let output = executor.execute(&command).unwrap();
        assert_eq!(output.status, 0);
        assert_eq!(
            parse_native_probe_output(&output.stdout).unwrap().program,
            local_bin.join("mbx")
        );

        std::fs::remove_file(local_bin.join("mbx")).unwrap();
        let output = executor.execute(&command).unwrap();
        assert_eq!(output.status, 0);
        assert_eq!(
            parse_native_probe_output(&output.stdout).unwrap().program,
            cargo_bin.join("mbx")
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cache_copy_refresh_is_atomic_and_skips_unchanged_binaries() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("native mbx");
        let native_bin = root.path().join("native-bin");
        let home = root.path().join("home");
        let cache = root.path().join("cache with spaces");
        std::fs::create_dir_all(&native_bin).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let install = |version: &str| {
            std::fs::write(&source, format!("#!/bin/sh\nprintf 'mbx {version}\\n'\n")).unwrap();
            std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        let native = |version: &str| NativeMbx {
            program: source.clone(),
            version: version.to_owned(),
        };
        std::os::unix::fs::symlink(&source, native_bin.join("mbx")).unwrap();
        struct IsolatedHostExecutor {
            path: String,
            home: String,
        }
        impl CommandExecutor for IsolatedHostExecutor {
            fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
                let mut command = command.clone();
                command.env.insert("PATH".into(), self.path.clone());
                command.env.insert("HOME".into(), self.home.clone());
                targets::ProcessExecutor.execute(&command)
            }
        }
        let executor = IsolatedHostExecutor {
            path: format!("{}:/usr/bin:/bin", native_bin.display()),
            home: home.display().to_string(),
        };

        install("1.22.0");
        let first =
            sync_mbx_binary_from_native(&CacheHost::Local, &native("1.22.0"), &cache, &executor)
                .unwrap();
        let copy = first.path;
        assert_eq!(copy, cache.join(".mjolnir/bin/mbx"));
        assert_eq!(
            std::fs::read(&copy).unwrap(),
            std::fs::read(&source).unwrap()
        );
        assert_eq!(
            std::fs::metadata(&copy).unwrap().permissions().mode() & 0o777,
            0o755
        );
        let original_inode = std::fs::metadata(&copy).unwrap().ino();

        let unchanged =
            sync_mbx_binary_from_native(&CacheHost::Local, &native("1.22.0"), &cache, &executor)
                .unwrap();
        assert_eq!(unchanged.path, copy);
        assert_eq!(std::fs::metadata(&copy).unwrap().ino(), original_inode);

        install("1.23.0");
        sync_mbx_binary_from_native(&CacheHost::Local, &native("1.23.0"), &cache, &executor)
            .unwrap();
        assert_ne!(std::fs::metadata(&copy).unwrap().ino(), original_inode);
        let version = executor
            .execute(&CommandSpec::new(
                copy.to_string_lossy().into_owned(),
                ["--version"],
            ))
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&version.stdout).trim(),
            "mbx 1.23.0"
        );
        assert_eq!(
            std::fs::read_dir(cache.join(".mjolnir/bin"))
                .unwrap()
                .count(),
            2,
            "publication leaves only the copy and its cross-process lock file"
        );
    }

    #[test]
    fn cache_sync_reprobes_and_retries_after_another_process_changes_the_host_binary() {
        struct ReprobeExecutor {
            syncs: std::sync::atomic::AtomicUsize,
            commands: Mutex<Vec<CommandSpec>>,
        }

        impl CommandExecutor for ReprobeExecutor {
            fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
                self.commands.lock().unwrap().push(command.clone());
                let (status, stdout, stderr) = match command.purpose.as_str() {
                    "synchronize native mbx into the shared cache"
                        if self.syncs.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 =>
                    {
                        (75, Vec::new(), b"native mbx changed".to_vec())
                    }
                    "synchronize native mbx into the shared cache" => {
                        (0, b"synced\n".to_vec(), Vec::new())
                    }
                    "read the container host mbx version" => {
                        (0, b"/opt/mbx/current\nmbx 1.23.0".to_vec(), Vec::new())
                    }
                    purpose => bail!("unexpected command purpose {purpose}"),
                };
                Ok(CommandOutput {
                    status,
                    stdout,
                    stderr,
                })
            }
        }

        let directory = PathBuf::from("/tmp/mjolnir-mbx-retry");
        let executor = ReprobeExecutor {
            syncs: std::sync::atomic::AtomicUsize::new(0),
            commands: Mutex::new(Vec::new()),
        };
        let binary = sync_mbx_binary_from_native(
            &CacheHost::Local,
            &NativeMbx {
                program: PathBuf::from("/opt/mbx/old"),
                version: "1.22.0".into(),
            },
            &directory,
            &executor,
        )
        .unwrap();

        assert_eq!(binary.version, "1.23.0");
        assert_eq!(
            executor
                .commands
                .lock()
                .unwrap()
                .iter()
                .map(|command| command.purpose.as_str())
                .collect::<Vec<_>>(),
            [
                "synchronize native mbx into the shared cache",
                "read the container host mbx version",
                "synchronize native mbx into the shared cache",
            ]
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cache_sync_without_flock_rechecks_after_rename_and_resynchronizes() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let root = tempfile::tempdir().unwrap();
        let tools = root.path().join("tools");
        let home = root.path().join("home");
        let cache = root.path().join("cache");
        let old_source = root.path().join("mbx-1.22.0");
        let new_source = root.path().join("mbx-1.23.0");
        let native_path = tools.join("mbx");
        let flipped = root.path().join("flipped");
        std::fs::create_dir_all(&tools).unwrap();
        std::fs::create_dir_all(&home).unwrap();

        for name in [
            "sh", "dirname", "mkdir", "readlink", "mktemp", "chmod", "wc", "mv", "rm", "ln",
        ] {
            let executable = std::env::split_paths(&std::env::var_os("PATH").unwrap())
                .map(|directory| directory.join(name))
                .find(|candidate| candidate.is_file())
                .unwrap_or_else(|| panic!("could not find test utility {name}"));
            symlink(executable, tools.join(name)).unwrap();
        }

        let real_cp = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|directory| directory.join("cp"))
            .find(|candidate| candidate.is_file())
            .unwrap();
        let cp_wrapper = tools.join("cp");
        std::fs::write(
            &cp_wrapper,
            r#"#!/bin/sh
"$MBX_TEST_REAL_CP" "$@" || exit $?
if [ ! -e "$MBX_TEST_FLIPPED" ]; then
    : > "$MBX_TEST_FLIPPED"
    rm -f -- "$MBX_TEST_NATIVE"
    ln -s -- "$MBX_TEST_NEW_SOURCE" "$MBX_TEST_NATIVE"
fi"#,
        )
        .unwrap();
        std::fs::set_permissions(&cp_wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
        for (path, version) in [(&old_source, "1.22.0"), (&new_source, "1.23.0")] {
            std::fs::write(path, format!("#!/bin/sh\nprintf 'mbx {version}\\n'\n")).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        symlink(&old_source, &native_path).unwrap();

        struct FallbackExecutor {
            tools: PathBuf,
            home: PathBuf,
            real_cp: PathBuf,
            flipped: PathBuf,
            native: PathBuf,
            new_source: PathBuf,
        }
        impl CommandExecutor for FallbackExecutor {
            fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
                let mut command = command.clone();
                command
                    .env
                    .insert("PATH".into(), self.tools.display().to_string());
                command
                    .env
                    .insert("HOME".into(), self.home.display().to_string());
                command.env.insert(
                    "MBX_TEST_REAL_CP".into(),
                    self.real_cp.display().to_string(),
                );
                command.env.insert(
                    "MBX_TEST_FLIPPED".into(),
                    self.flipped.display().to_string(),
                );
                command
                    .env
                    .insert("MBX_TEST_NATIVE".into(), self.native.display().to_string());
                command.env.insert(
                    "MBX_TEST_NEW_SOURCE".into(),
                    self.new_source.display().to_string(),
                );
                targets::ProcessExecutor.execute(&command)
            }
        }
        let executor = FallbackExecutor {
            tools,
            home,
            real_cp,
            flipped: flipped.clone(),
            native: native_path,
            new_source,
        };

        let binary = sync_mbx_binary_from_native(
            &CacheHost::Local,
            &NativeMbx {
                program: old_source,
                version: "1.22.0".into(),
            },
            &cache,
            &executor,
        )
        .unwrap();

        assert!(
            flipped.exists(),
            "the test copy did not switch host versions"
        );
        assert_eq!(binary.version, "1.23.0");
        let copied = targets::ProcessExecutor
            .execute(&CommandSpec::new(
                binary.path.to_string_lossy().into_owned(),
                ["--version"],
            ))
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&copied.stdout).trim(), "mbx 1.23.0");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cross_process_cache_sync_cannot_publish_a_stale_host_binary_last() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        use std::sync::atomic::{AtomicBool, Ordering};

        let root = tempfile::tempdir().unwrap();
        let source_dir = root.path().join("sources");
        let native_bin = root.path().join("native-bin");
        let wrapper_bin = root.path().join("wrapper-bin");
        let home = root.path().join("home");
        let cache = root.path().join("cache");
        let lock = cache.join(".mjolnir/bin/.mbx.lock");
        let ready = root.path().join("lock-ready");
        let release = root.path().join("release-lock");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::create_dir_all(&native_bin).unwrap();
        std::fs::create_dir_all(&wrapper_bin).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(lock.parent().unwrap()).unwrap();

        let old_source = source_dir.join("mbx-1.22.0");
        let new_source = source_dir.join("mbx-1.23.0");
        for (path, version) in [(&old_source, "1.22.0"), (&new_source, "1.23.0")] {
            std::fs::write(path, format!("#!/bin/sh\nprintf 'mbx {version}\\n'\n")).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let native_path = native_bin.join("mbx");
        symlink(&old_source, &native_path).unwrap();

        // Mark the old process as soon as it reaches flock, then delegate to
        // the real utility. This proves it is waiting on the held lock before
        // the native install path changes.
        let flock_wrapper = wrapper_bin.join("flock");
        std::fs::write(
            &flock_wrapper,
            "#!/bin/sh\n[ -z \"${MBX_TEST_FLOCK_MARKER:-}\" ] || : > \"$MBX_TEST_FLOCK_MARKER\"\nexec /usr/bin/flock \"$@\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&flock_wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();

        let path_value = format!(
            "{}:{}:/usr/bin:/bin",
            wrapper_bin.display(),
            native_bin.display()
        );
        let executor = crate::targets::ProcessExecutor;
        let holder_script = r#"set -eu
exec 9>"$1"
flock -x 9
: > "$2"
while [ ! -e "$3" ]; do sleep 0.01; done"#;
        let mut holder = CommandSpec::new(
            "sh",
            vec![
                "-c".into(),
                holder_script.into(),
                "mbx-lock-holder".into(),
                lock.to_string_lossy().into_owned(),
                ready.to_string_lossy().into_owned(),
                release.to_string_lossy().into_owned(),
            ],
        )
        .purpose("hold test mbx lock");
        holder.env.insert("PATH".into(), path_value.clone());
        holder.env.insert("HOME".into(), home.display().to_string());
        let holder_thread = std::thread::spawn(move || executor.execute(&holder));

        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while !ready.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        if !ready.exists() {
            std::fs::write(&release, "release").unwrap();
            let _ = holder_thread.join();
            panic!("the test lock holder did not acquire flock");
        }

        let make_sync = |source: &Path, version: &str| {
            let mut command = CommandSpec::new(
                "sh",
                [
                    "-c".to_owned(),
                    SYNC_MBX_BINARY_SCRIPT.to_owned(),
                    "test-mbx-sync".to_owned(),
                    source.to_string_lossy().into_owned(),
                    cache_binary_path(&cache).to_string_lossy().into_owned(),
                    version.to_owned(),
                    NATIVE_VERSION_SCRIPT.to_owned(),
                ],
            )
            .purpose("run cross-process mbx sync test");
            command.clear_env = true;
            command.env.insert("PATH".into(), path_value.clone());
            command
                .env
                .insert("HOME".into(), home.display().to_string());
            command
        };

        let old_marker = root.path().join("old-reached-flock");
        let old_command = make_sync(&old_source, "1.22.0");
        let old_executor = crate::targets::ProcessExecutor;
        let old_done = std::sync::Arc::new(AtomicBool::new(false));
        let old_done_thread = old_done.clone();
        let mut old_command = old_command;
        old_command.env.insert(
            "MBX_TEST_FLOCK_MARKER".into(),
            old_marker.display().to_string(),
        );
        let old_thread = std::thread::spawn(move || {
            let result = old_executor.execute(&old_command);
            old_done_thread.store(true, Ordering::SeqCst);
            result
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while !old_marker.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let old_waited = old_marker.exists() && !old_done.load(Ordering::SeqCst);

        std::fs::remove_file(&native_path).unwrap();
        symlink(&new_source, &native_path).unwrap();
        let new_command = make_sync(&new_source, "1.23.0");
        let new_executor = crate::targets::ProcessExecutor;
        let new_thread = std::thread::spawn(move || new_executor.execute(&new_command));
        std::thread::sleep(Duration::from_millis(40));
        std::fs::write(&release, "release").unwrap();

        let holder_output = holder_thread.join().unwrap().unwrap();
        let old_output = old_thread.join().unwrap().unwrap();
        let new_output = new_thread.join().unwrap().unwrap();
        assert_eq!(holder_output.status, 0, "{holder_output:?}");
        assert!(old_waited, "the stale synchronizer did not wait on flock");
        assert_eq!(old_output.status, 75, "{old_output:?}");
        assert_eq!(new_output.status, 0, "{new_output:?}");

        let version = crate::targets::ProcessExecutor
            .execute(&CommandSpec::new(
                cache_binary_path(&cache).to_string_lossy().into_owned(),
                ["--version"],
            ))
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&version.stdout).trim(),
            "mbx 1.23.0"
        );
    }

    #[test]
    fn absent_or_too_old_native_mbx_leaves_the_existing_cache_copy() {
        struct ProbeAnswer {
            status: i32,
            stdout: Vec<u8>,
        }

        impl CommandExecutor for ProbeAnswer {
            fn execute(&self, _command: &CommandSpec) -> Result<CommandOutput> {
                Ok(CommandOutput {
                    status: self.status,
                    stdout: self.stdout.clone(),
                    stderr: Vec::new(),
                })
            }
        }

        let root = tempfile::tempdir().unwrap();
        let cache = root.path().join("cache");
        let copy = cache_binary_path(&cache);
        std::fs::create_dir_all(copy.parent().unwrap()).unwrap();
        std::fs::write(&copy, b"previous compatible copy").unwrap();

        for answer in [
            ProbeAnswer {
                status: 1,
                stdout: Vec::new(),
            },
            ProbeAnswer {
                status: 0,
                stdout: "/opt/old/mbx\nmbx 1.21.0".into(),
            },
        ] {
            let result = sync_current_mbx_binary(&CacheHost::Local, &cache, &answer).unwrap();
            assert!(matches!(result, CachedMbxSync::Unavailable(_)));
            assert_eq!(std::fs::read(&copy).unwrap(), b"previous compatible copy");
        }
    }

    #[test]
    fn the_preview_reports_native_cache_values_without_creating_directories() {
        let _isolated = isolated();
        let executor = ProbeExecutor::new(&native_host());
        let preview = preview_build_cache(&configured_local_machine(), &executor)
            .unwrap()
            .unwrap();
        assert_eq!(preview.native_mbx.as_deref(), Some(MBX_VERSION));
        assert_eq!(preview.mbx_profile_file, None);
        assert_eq!(
            preview.directory,
            Some(PathBuf::from("/mnt/fast/mbx-cache"))
        );
        assert_eq!(
            preview.max_total_size,
            Some(BuildCacheLimit::MbxDefault(None))
        );
        assert_eq!(preview.off_reason, None);
        // Profile previews carry the shared script, including its update-only
        // mkdir branch, but do not execute a host mkdir command.
        assert!(!executor.ran().iter().any(|line| line.starts_with("mkdir ")));
    }
}
