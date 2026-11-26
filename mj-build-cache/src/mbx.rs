//! The mbx build cache for Rust container sessions.
//!
//! mbx wraps Cargo: a binary named `cargo` that is really `mbx` intercepts the
//! build, looks every compiler action up in a content-addressed store, and
//! restores cached outputs instead of recompiling. Podman and Docker mount
//! the host store read-write at the same path in each container, and stage the
//! host's mbx executable and a snapshot of its configuration under each
//! session's private worker root. Nitro guests
//! keep their store on the guest filesystem. Mjolnir tells host mbx when a
//! workspace it removed will never build again (see [`release`]).
//!
//! Cache discovery can leave new sessions uncached. Once a cache is selected,
//! configuration failures are reported rather than launching with stale policy.

mod configuration;
mod discovery;
pub mod install;
pub mod release;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};

use super::cache_host::{CacheHost, HostPlatform};
use crate::targets::{self, CommandExecutor, CommandOutput, CommandSpec};
use mj_core::config::{Config, TargetBuildCache, TargetTemplate};
use mj_core::state::{
    BuildCacheApplication, BuildCacheLimit, BuildCacheOff, BuildCachePreview, BuildCacheStats,
    SessionBuildCache,
};

pub use mj_core::config::MBX_VERSION;

const NITRO_CACHE_DIRECTORY: &str = "/var/lib/hel/mbx";

enum CachePlacement {
    Shared(CacheHost),
    NitroGuest,
}

/// mbx's running totals, relative to the cache directory.
const TALLY_RELATIVE: &str = "actions/savings/v1/tally.json";
const RESOLUTION_LIFETIME: Duration = Duration::from_secs(600);
const LABEL: &str = "hel-mbx";
const WINDOWS_HOST: &str = "Mjolnir's shared mbx cache requires a Linux host.";

/// Why a host's platform keeps its mbx away from its containers, and the
/// native mbx it found there.
struct PlatformLimit {
    native: Option<NativeMbx>,
    reason: String,
}

/// The one decision on whether a host's platform can share its mbx with
/// containers, or `None` on Linux, where it can. A Mac runs mbx natively, so
/// its own mbx is still found and reported: bare sessions there use it.
fn platform_limit(
    facts: &discovery::HostFacts<impl CommandExecutor>,
) -> Result<Option<PlatformLimit>> {
    let native = match facts.platform()? {
        HostPlatform::Linux => return Ok(None),
        HostPlatform::Windows => {
            return Ok(Some(PlatformLimit {
                native: None,
                reason: WINDOWS_HOST.into(),
            }));
        }
        HostPlatform::Darwin => facts.native()?,
    };
    const CONTAINERS: &str = "Container sessions run without the shared build cache, because \
        containers on macOS run Linux in a virtual machine that cannot run a macOS mbx or share \
        its cache.";
    // The probe also searches user directories a login PATH may omit, so it
    // cannot say that bare sessions find what it found.
    let reason = match &native {
        Some(native) => format!(
            "mbx {} runs natively on this Mac at {}; bare sessions here use it when it is on \
             their login PATH. {CONTAINERS}",
            native.version,
            native.program.display()
        ),
        None => format!(
            "mbx is not installed on this Mac; bare sessions here use it once it is installed \
             natively and on their login PATH. {CONTAINERS}"
        ),
    };
    Ok(Some(PlatformLimit { native, reason }))
}

/// What a container target's host offers as a build cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedBuildCache {
    /// Cache directory on the host, mounted at the same path in the container.
    pub directory: PathBuf,
    /// The effective `[target] root` of the host's mbx, absolute and
    /// normalized: its configured root, or `<directory>/targets`. A root
    /// outside the cache directory is mounted at the same path too.
    pub target_root: PathBuf,
}

/// Host inspections, keyed by host and per-target settings. An inspection
/// runs commands on the host, and a burst of new sessions must not repeat them
/// for each one. A failure is remembered too, so a host that cannot answer is
/// not re-probed by every session in that burst.
///
/// One map holds both the answers and the inspections still running, so a
/// caller that finds an answer pending waits for it instead of asking the host
/// again: lookup and claim happen under one lock.
type Resolutions = std::collections::BTreeMap<String, Resolution>;

/// What an inspection answered, shared by everyone it is handed to.
type Answer = Result<Arc<Inspection>, String>;

enum Resolution {
    /// Someone is asking the host now. Waiters share the answer it publishes.
    InFlight(Arc<Flight>),
    Ready(Instant, Answer),
}

/// One running inspection, and what its waiters wait on.
struct Flight {
    state: Mutex<FlightState>,
    changed: Condvar,
}

enum FlightState {
    Running,
    Done(Answer),
    /// The inspection left no answer for these waiters (it panicked, its caller
    /// was cancelled, or a fresher answer replaced it). Waiters ask again.
    Abandoned,
}

impl Flight {
    fn settle(&self, state: FlightState) {
        *self.state.lock().unwrap_or_else(PoisonError::into_inner) = state;
        self.changed.notify_all();
    }
}

/// Held by the caller that owns an [`Resolution::InFlight`] slot. Dropping it
/// without [`Self::finish`] (a panic in the inspection, or a cancelled owner)
/// frees the slot and wakes the waiters, so a pending slot cannot outlive its
/// owner.
struct FlightOwner<'a> {
    key: &'a str,
    flight: Arc<Flight>,
    finished: bool,
}

impl FlightOwner<'_> {
    /// Publish `result` to the waiters and to later callers. If a fresher
    /// answer replaced this slot meanwhile, publish nothing: the waiters are
    /// told to ask again, and find the fresher answer in the map instead of
    /// this older one.
    fn finish(mut self, result: Answer) {
        self.finished = true;
        let mut resolutions = lock_resolutions();
        let current = matches!(
            resolutions.get(self.key),
            Some(Resolution::InFlight(flight)) if Arc::ptr_eq(flight, &self.flight)
        );
        if current {
            resolutions.insert(
                self.key.to_owned(),
                Resolution::Ready(Instant::now(), result.clone()),
            );
        }
        drop(resolutions);
        self.flight.settle(if current {
            FlightState::Done(result)
        } else {
            FlightState::Abandoned
        });
    }
}

impl Drop for FlightOwner<'_> {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let mut resolutions = lock_resolutions();
        if matches!(
            resolutions.get(self.key),
            Some(Resolution::InFlight(flight)) if Arc::ptr_eq(flight, &self.flight)
        ) {
            resolutions.remove(self.key);
        }
        drop(resolutions);
        self.flight.settle(FlightState::Abandoned);
    }
}

static RESOLUTIONS: std::sync::LazyLock<Mutex<Resolutions>> =
    std::sync::LazyLock::new(|| Mutex::new(Resolutions::new()));

/// How often a caller waiting on another's inspection checks for cancellation.
const FLIGHT_POLL: Duration = Duration::from_millis(100);

fn lock_resolutions() -> std::sync::MutexGuard<'static, Resolutions> {
    // A panic while the map was held leaves it consistent: every insert and
    // remove is a single call.
    RESOLUTIONS.lock().unwrap_or_else(PoisonError::into_inner)
}

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
    if freshness == Freshness::Fresh {
        let inspection = inspect_host(host, settings, executor);
        // A cancelled caller's failure says nothing about the host.
        if inspection.is_ok() || !executor.cancellation_requested() {
            lock_resolutions().insert(
                key,
                Resolution::Ready(Instant::now(), recorded(&inspection)),
            );
        }
        return inspection;
    }
    loop {
        let flight = {
            let mut resolutions = lock_resolutions();
            match resolutions.get(&key) {
                Some(Resolution::Ready(at, answer)) if at.elapsed() < RESOLUTION_LIFETIME => {
                    return answered(answer.clone());
                }
                Some(Resolution::InFlight(flight)) => Arc::clone(flight),
                _ => {
                    let flight = Arc::new(Flight {
                        state: Mutex::new(FlightState::Running),
                        changed: Condvar::new(),
                    });
                    resolutions.insert(key.clone(), Resolution::InFlight(Arc::clone(&flight)));
                    drop(resolutions);
                    let owner = FlightOwner {
                        key: &key,
                        flight,
                        finished: false,
                    };
                    let inspection = inspect_host(host, settings, executor);
                    // A cancelled caller's failure says nothing about the
                    // host: abandon the flight so waiters ask it themselves,
                    // and never remember that error.
                    if inspection.is_err() && executor.cancellation_requested() {
                        drop(owner);
                    } else {
                        owner.finish(recorded(&inspection));
                    }
                    return inspection;
                }
            }
        };
        match await_flight(&flight, executor)? {
            Some(answer) => return answered(answer),
            // The owner left no answer: ask again, as a new owner or a new
            // waiter, and see any fresher answer that replaced its slot.
            None => continue,
        }
    }
}

fn recorded(inspection: &Result<Inspection>) -> Answer {
    match inspection {
        Ok(inspection) => Ok(Arc::new(inspection.clone())),
        Err(error) => Err(format!("{error:#}")),
    }
}

fn answered(answer: Answer) -> Result<Inspection> {
    answer
        .map(|inspection| Inspection::clone(&inspection))
        .map_err(|error| anyhow::anyhow!(error))
}

/// Wait for the inspection another caller is running. `None` when it was
/// abandoned. The wait ends with this caller's own cancellation, so a waiter
/// never outlives the operation that asked.
fn await_flight(flight: &Flight, executor: &impl CommandExecutor) -> Result<Option<Answer>> {
    let mut state = flight.state.lock().unwrap_or_else(PoisonError::into_inner);
    loop {
        match &*state {
            FlightState::Done(answer) => return Ok(Some(answer.clone())),
            FlightState::Abandoned => return Ok(None),
            FlightState::Running => {}
        }
        ensure!(
            !executor.cancellation_requested(),
            "cancelled while waiting for the container host inspection"
        );
        state = flight
            .changed
            .wait_timeout(state, FLIGHT_POLL)
            .unwrap_or_else(PoisonError::into_inner)
            .0;
    }
}

/// Resolve a target's shared host cache. Nitro guest caches are prepared without a host lookup.
pub fn resolve(
    target: &targets::TargetTemplate,
    executor: &impl CommandExecutor,
) -> Option<ResolvedBuildCache> {
    let (CachePlacement::Shared(host), settings) = supported_host(target)? else {
        return None;
    };
    resolve_shared(&host, &settings, executor)
}

fn resolve_shared(
    host: &CacheHost,
    settings: &TargetBuildCache,
    executor: &impl CommandExecutor,
) -> Option<ResolvedBuildCache> {
    let inspection = match inspect(host, settings, Freshness::Memoized, executor) {
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
    if let Err(error) = create_directory(host, &cache.directory, executor) {
        tracing::warn!(
            directory = %cache.directory.display(),
            "the build cache directory could not be created: {error:#}"
        );
        return None;
    }
    Some(cache)
}

/// Where a session can keep its build cache. Apple container's host mount
/// lacks verified shared locks. Nitro keeps its store on the guest filesystem.
fn supported_host(target: &targets::TargetTemplate) -> Option<(CachePlacement, TargetBuildCache)> {
    let (placement, container) = match target {
        targets::TargetTemplate::LocalPodman(container)
        | targets::TargetTemplate::LocalDocker(container)
        | targets::TargetTemplate::SshPodman { container, .. }
        | targets::TargetTemplate::SshDocker { container, .. } => (
            CachePlacement::Shared(CacheHost::for_target(target)?),
            container,
        ),
        targets::TargetTemplate::SshNitro { container, .. } => {
            (CachePlacement::NitroGuest, container)
        }
        _ => return None,
    };
    Some((placement, container.build_cache.clone().unwrap_or_default()))
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
    if install::install_candidate_kind(&preview).is_some()
        && let Some(profile) = install::login_profile_details(&host, executor)?
    {
        preview.mbx_profile_file = Some(profile.file);
        preview.mbx_profile_warning = profile.warning;
        preview.mbx_manual_path_line = profile.manual_path_line;
    }
    Ok(Some(preview))
}

/// Native mbx compatibility for a host used by configured container targets.
pub struct DoctorHostMbx {
    pub host: String,
    pub targets: Vec<String>,
    pub status: DoctorHostMbxStatus,
}

pub enum DoctorHostMbxStatus {
    Unsupported(String),
    Absent,
    Compatible(String),
    Unknown(String),
}

/// One classification of a host probe, shared by doctor and session preview.
enum NativeMbxStatus {
    Absent,
    Compatible(NativeMbx),
    Unknown { version: String, reason: String },
}

fn classify_native_mbx(native: Option<NativeMbx>) -> NativeMbxStatus {
    let Some(native) = native else {
        return NativeMbxStatus::Absent;
    };
    match semver::Version::parse(&native.version) {
        Ok(_) => NativeMbxStatus::Compatible(native),
        Err(_) => NativeMbxStatus::Unknown {
            reason: format!(
                "the host reported an unrecognized mbx version {:?}",
                native.version
            ),
            version: native.version,
        },
    }
}

fn cache_unavailable_reason(status: &NativeMbxStatus) -> String {
    match status {
        NativeMbxStatus::Absent => {
            "mbx is not installed on the container host. Install mbx from Settings › Setup › Machines to enable the shared build cache.".into()
        }
        NativeMbxStatus::Unknown { reason, .. } => format!(
            "{reason}; install or upgrade mbx from Settings › Setup › Machines to enable the shared build cache"
        ),
        NativeMbxStatus::Compatible(_) => unreachable!("compatible mbx enables the cache"),
    }
}

/// Check each relevant host once. The cache is optional, so disabled caches
/// and hosts with no Podman or Docker target need no compatibility check.
pub fn doctor_host_mbx(config: &Config, executor: &impl CommandExecutor) -> Vec<DoctorHostMbx> {
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
            let facts = discovery::HostFacts::direct(&host, executor);
            if let Some(limit) = platform_limit(&facts)? {
                return Ok(DoctorHostMbxStatus::Unsupported(limit.reason));
            }
            Ok(match classify_native_mbx(facts.native()?) {
                NativeMbxStatus::Absent => DoctorHostMbxStatus::Absent,
                NativeMbxStatus::Compatible(native) => {
                    DoctorHostMbxStatus::Compatible(native.version)
                }
                NativeMbxStatus::Unknown { reason, .. } => DoctorHostMbxStatus::Unknown(reason),
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
    // One SSH round trip answers every read-only question a host has; a local
    // host answers each from the machine itself.
    let facts = discovery::HostFacts::gather(host, discovery::Scope::Inspection, executor)?;
    if let Some(limit) = platform_limit(&facts)? {
        return Ok(Inspection {
            preview: BuildCachePreview {
                native_mbx: limit.native.map(|native| native.version),
                mbx_profile_file: None,
                mbx_profile_warning: None,
                mbx_manual_path_line: None,
                directory: None,
                max_total_size: None,
                user_managed: false,
                application: BuildCacheApplication::Pending,
                budget_note: None,
                stats: None,
                off_reason: Some(BuildCacheOff::UnsupportedPlatform(limit.reason)),
            },
            cache: None,
        });
    }
    let off = |preview: BuildCachePreview| Inspection {
        preview,
        cache: None,
    };
    let compatibility = classify_native_mbx(facts.native()?);
    let native = match compatibility {
        NativeMbxStatus::Compatible(native) => native,
        status => {
            let native_version = match &status {
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
    let directory = facts.cache_directory(&native)?;
    ensure!(
        mj_core::target_path::is_absolute_on_host_or_target(&directory),
        "build cache directory {} is not absolute",
        directory.display()
    );

    let user_managed = true;
    let text = facts.config()?;
    let limit = match configuration::configured_limit(text.as_deref(), "gc", "max_total_size")? {
        Some(size) => BuildCacheLimit::HostConfiguration(Some(size)),
        None => BuildCacheLimit::MbxDefault(None),
    };
    let target_root = effective_target_root(text.as_deref().unwrap_or_default(), &directory);
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
        // Sessions stage the host's own configuration when they launch, so
        // there is nothing left to apply to the machine.
        application: BuildCacheApplication::Applied,
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
    if !mj_core::target_path::is_within(&target_root, &directory) {
        let root_volume = nearest_existing_ancestor(host, &target_root, executor)?;
        if !cross_reflinks_supported(host, &volume, &root_volume, executor)? {
            tracing::warn!(
                cache = %directory.display(),
                target_root = %target_root.display(),
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
pub struct NativeMbx {
    pub program: PathBuf,
    pub version: String,
}

/// Find the same native executable whether it is on PATH or in a common
/// per-user install directory, then resolve symlinks before reporting it.
/// Links are followed one at a time because macOS before 12.3 has no
/// `readlink -f`; every path handed to `readlink` starts with `/`.
pub const NATIVE_VERSION_SCRIPT: &str = r#"for candidate in "$(command -v mbx 2>/dev/null || true)" "$HOME/.local/bin/mbx" "$HOME/.cargo/bin/mbx"; do
    [ -n "$candidate" ] && [ -x "$candidate" ] || continue
    case "$candidate" in /*) resolved=$candidate ;; *) resolved=$PWD/$candidate ;; esac
    hops=0
    while [ -L "$resolved" ]; do
        [ "$hops" -lt 40 ] || continue 2
        link=$(readlink "$resolved" 2>/dev/null) || continue 2
        case "$link" in /*) resolved=$link ;; *) resolved=${resolved%/*}/$link ;; esac
        hops=$((hops + 1))
    done
    directory=$(cd -P "${resolved%/*}/" 2>/dev/null && pwd -P) || continue
    resolved=${directory%/}/${resolved##*/}
    case "$resolved" in /*) ;; *) continue ;; esac
    if version=$("$resolved" --version 2>/dev/null); then
        printf '%s
%s' "$resolved" "$version"
        exit 0
    fi
done
exit 1"#;

/// The host's own mbx, or `None` when no supported candidate can run.
pub fn probe_native_version(
    host: &CacheHost,
    executor: &impl CommandExecutor,
) -> Result<Option<NativeMbx>> {
    let command = host.shell_command(
        NATIVE_VERSION_SCRIPT,
        LABEL,
        [],
        "read the container host mbx version",
    );
    native_from_probe(&executor.execute(&command)?)
}

/// Read the native-mbx probe's answer, however the probe was run: on its own
/// or framed in a batch with other probes.
fn native_from_probe(output: &CommandOutput) -> Result<Option<NativeMbx>> {
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
        mj_core::target_path::is_absolute(Path::new(program)),
        "mbx version probe returned a non-absolute executable path {program:?}"
    );
    Ok(NativeMbx {
        program: PathBuf::from(program),
        version: version.to_owned(),
    })
}

/// The host executable Podman and Docker sessions stage into their private
/// worker root, or why this host cannot supply one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeMbxLookup {
    Ready(NativeMbx),
    Unavailable(String),
}

/// Look up the mbx on the host that owns a container. The host is the
/// container's own, never a target name that can be reassigned.
pub fn native_mbx_for_container(
    target: &targets::TargetLocator,
    executor: &impl CommandExecutor,
) -> Result<NativeMbxLookup> {
    let host = host_for_locator(target).context("build cache has no container host")?;
    Ok(native_lookup(probe_native_version(&host, executor)?))
}

fn native_lookup(native: Option<NativeMbx>) -> NativeMbxLookup {
    match classify_native_mbx(native) {
        NativeMbxStatus::Compatible(native) => NativeMbxLookup::Ready(native),
        status => NativeMbxLookup::Unavailable(cache_unavailable_reason(&status)),
    }
}

/// The container host's own `mbx/config.toml` as `cache`'s container must see
/// it (see [`configuration::reconcile_container_config`]); an empty document
/// when the host has none. Each session stages this text as its private policy
/// snapshot.
pub fn native_config_for_container(
    target: &targets::TargetLocator,
    cache: &SessionBuildCache,
    executor: &impl CommandExecutor,
) -> Result<String> {
    let host = host_for_locator(target).context("build cache has no container host")?;
    let text = host_config_file(&host, executor)?.unwrap_or_default();
    configuration::reconcile_container_config(&text, cache)
}

/// [`native_config_for_container`] and [`native_mbx_for_container`] together.
/// An SSH host answers both in one round trip, where each used to take its
/// own: the configuration's location and its content, then the mbx version.
///
/// A host whose mbx probe fails is `Unavailable` with the failure as its
/// reason, not an error, so the caller can still stage the policy and find a
/// private legacy mbx before it gives up on the host's own.
pub fn native_mbx_and_config_for_container(
    target: &targets::TargetLocator,
    cache: &SessionBuildCache,
    executor: &impl CommandExecutor,
) -> Result<(String, NativeMbxLookup)> {
    let host = host_for_locator(target).context("build cache has no container host")?;
    let facts = discovery::HostFacts::gather(&host, discovery::Scope::Staging, executor)?;
    let text = facts.config()?.unwrap_or_default();
    let configuration = configuration::reconcile_container_config(&text, cache)?;
    let native = match facts.native() {
        Ok(native) => native_lookup(native),
        Err(error) => NativeMbxLookup::Unavailable(format!("{error:#}")),
    };
    Ok((configuration, native))
}

/// The available column of a `df -P` report (the third field of its data row).
/// Callers decide the block size used by the report they requested.
pub fn available_bytes(report: &str) -> Option<u64> {
    let row = report
        .lines()
        .filter(|line| !line.trim().is_empty())
        .nth(1)?;
    let fields = row.split_whitespace().collect::<Vec<_>>();
    fields.get(fields.len().checked_sub(3)?)?.parse().ok()
}

/// What the cache directory probe is called in a failure.
const CACHE_DIRECTORY_PURPOSE: &str = "read the container host mbx cache directory";

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
        CACHE_DIRECTORY_PURPOSE,
    );
    cache_directory_from_probe(executor.execute(&command)?)
}

/// Read `mbx cache dir --json`'s answer, however the probe was run.
fn cache_directory_from_probe(output: CommandOutput) -> Result<PathBuf> {
    let output = checked_as(output, CACHE_DIRECTORY_PURPOSE)?;
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
    let path = mj_core::target_path::join(&directory, Path::new("config.toml"));
    configuration::read_file(host, &path, executor)
}

#[derive(Default)]
struct MbxStorageConfig {
    cache: Option<PathBuf>,
    target_root: Option<PathBuf>,
}

fn parse_storage_config(text: &str) -> Result<MbxStorageConfig> {
    let document: toml::Value = toml::from_str(text).map_err(|error: toml::de::Error| {
        if let Some(span) = error.span() {
            let line = text.as_bytes()[..span.start.min(text.len())]
                .iter()
                .filter(|&&byte| byte == b'\n')
                .count()
                + 1;
            anyhow::anyhow!("parse mbx storage configuration at line {line}: invalid TOML")
        } else {
            anyhow::anyhow!("parse mbx storage configuration: invalid TOML")
        }
    })?;
    let string_path = |value: &toml::Value, name: &str| -> Result<PathBuf> {
        value
            .as_str()
            .map(PathBuf::from)
            .with_context(|| format!("{name} must be a string"))
    };
    let cache = document
        .get("cache_dir")
        .map(|v| string_path(v, "cache_dir"))
        .transpose()?;
    let target_root = if let Some(target) = document.get("target") {
        let target = target.as_table().context("target must be a table")?;
        target
            .get("root")
            .map(|v| string_path(v, "target.root"))
            .transpose()?
    } else {
        None
    };
    Ok(MbxStorageConfig { cache, target_root })
}

fn read_local_storage_config(path: &Path) -> Result<Option<String>> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
    };
    std::fs::read_to_string(path).map(Some).map_err(|error| {
        let dangling =
            meta.file_type().is_symlink() && error.kind() == std::io::ErrorKind::NotFound;
        let context = if dangling {
            format!("read {}: dangling symlink", path.display())
        } else {
            format!("read {}", path.display())
        };
        anyhow::Error::new(error).context(context)
    })
}

/// The local directories mbx uses for its cache and managed targets.
pub fn local_storage_paths() -> Result<Vec<PathBuf>> {
    let config_path = dirs::config_dir().map(|directory| directory.join("mbx/config.toml"));
    let config_file = config_path
        .as_deref()
        .map(read_local_storage_config)
        .transpose()?
        .flatten();
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
    .with_context(|| {
        format!(
            "resolve mbx config {}",
            config_path
                .as_deref()
                .unwrap_or(Path::new("<unavailable>"))
                .display()
        )
    })
}

fn resolve_local_storage_paths(
    cache_override: Option<PathBuf>,
    target_root_override: Option<PathBuf>,
    config_file: Option<&str>,
    default_cache: Option<PathBuf>,
) -> Result<Vec<PathBuf>> {
    let config = config_file
        .map(parse_storage_config)
        .transpose()?
        .unwrap_or_default();
    let Some(cache) = cache_override.or(config.cache).or(default_cache) else {
        return Ok(Vec::new());
    };
    let configured_target_root = config.target_root;
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
    Ok(vec![cache, target_root])
}

/// The `[target] root` mbx uses under a host configuration: the configured
/// root (relative to the cache directory unless absolute), or
/// `<directory>/targets`, normalized so `..` cannot hide where it points. An
/// unreadable configuration is reported and takes the default, as mbx would
/// refuse to start on it anyway.
pub fn effective_target_root(config_file: &str, directory: &Path) -> PathBuf {
    let configured = parse_storage_config(config_file)
        .map_err(|error| tracing::warn!("the host mbx configuration is unreadable: {error:#}"))
        .ok()
        .and_then(|config| config.target_root);
    let path = mj_core::target_path::normalize(&match configured {
        Some(root) => mj_core::target_path::join(directory, &root),
        None => directory.join("targets"),
    });
    PathBuf::from(mj_core::target_path::text(&path))
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
        mj_core::target_path::is_absolute(&path),
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
    checked_as(output, &command.purpose)
}

fn checked_as(output: CommandOutput, purpose: &str) -> Result<CommandOutput> {
    if output.status == 0 {
        return Ok(output);
    }
    bail!(
        "{purpose} failed with status {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

// -- per-session decision -------------------------------------------------

/// Decide the cache placement to record on the session and attach any shared
/// mounts. Resumes and moves resolve current policy.
pub fn prepare(
    target: &targets::TargetTemplate,
    session: &mj_core::state::SessionRecord,
    mounts: &mut Vec<targets::AdditionalMount>,
    executor: &impl CommandExecutor,
) -> Option<SessionBuildCache> {
    let (placement, settings) = supported_host(target)?;
    let CachePlacement::Shared(host) = placement else {
        // Use the session's identity so this guest directory is never taken
        // for a host mount.
        return (settings.enabled != Some(false)).then(|| SessionBuildCache {
            host: format!("nitro:{}", session.id),
            directory: PathBuf::from(NITRO_CACHE_DIRECTORY),
            max_size: None,
            target_root: None,
        });
    };
    // Sessions at the legacy shared `/workspace` would collide in the
    // shared store's path-keyed records.
    session.container_workspace.as_ref()?;
    let resolved = resolve_shared(&host, &settings, executor)?;
    let build_cache = SessionBuildCache {
        host: host.key(),
        directory: resolved.directory,
        max_size: None,
        target_root: Some(resolved.target_root),
    };
    attach_mounts(&build_cache, mounts).then_some(build_cache)
}

/// Mount the cache, and a target root outside it, read-write at the same
/// absolute paths the host uses. An attached directory that already covers one
/// of those paths wins, and the session runs without the cache.
fn attach_mounts(
    build_cache: &SessionBuildCache,
    mounts: &mut Vec<targets::AdditionalMount>,
) -> bool {
    let mut wanted = vec![build_cache.directory.clone()];
    wanted.extend(build_cache.mounted_target_root().map(Path::to_path_buf));
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
    for directory in &wanted {
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
pub fn host_for_locator(target: &targets::TargetLocator) -> Option<CacheHost> {
    match target {
        targets::TargetLocator::LocalPodman { .. } | targets::TargetLocator::LocalDocker { .. } => {
            Some(CacheHost::Local)
        }
        targets::TargetLocator::SshPodman { ssh, .. }
        | targets::TargetLocator::SshDocker { ssh, .. }
        | targets::TargetLocator::SshAppleContainer { ssh, .. } => {
            Some(CacheHost::Ssh(ssh.clone()))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::targets::{ContainerTemplate, SshTarget, TargetTemplate};
    use mj_core::config::ImagePullPolicy;
    use std::sync::{Condvar, Mutex};

    /// The resolution cache is process-wide, so tests that exercise it run one
    /// at a time and start from an empty cache.
    static ISOLATED: Mutex<()> = Mutex::new(());

    fn isolated() -> std::sync::MutexGuard<'static, ()> {
        let guard = ISOLATED.lock().unwrap_or_else(|error| error.into_inner());
        RESOLUTIONS.lock().expect("mbx resolutions").clear();
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

        /// The canned answer for one question, or a 127 that names it.
        fn answer(&self, needle: &str) -> (i32, String) {
            self.answers
                .iter()
                .find(|(candidate, _, _)| *candidate == needle)
                .map_or_else(
                    || (127, String::new()),
                    |(_, status, stdout)| (*status, stdout.clone()),
                )
        }

        /// What the batched SSH discovery script prints, given the canned
        /// answers to the separate questions it replaces and the order in
        /// which the script asks them.
        fn discovery(&self, script: &str) -> Vec<u8> {
            let mut probes: Vec<(&str, i32, String, &str)> = Vec::new();
            let mut ask = |name: &'static str, needle: &str, suffix: &str| {
                let (status, stdout) = self.answer(needle);
                let stdout = if status == 0 {
                    format!("{stdout}{suffix}")
                } else {
                    stdout
                };
                probes.push((name, status, stdout, ""));
                (status, probes.last().unwrap().2.clone())
            };
            if script.contains("uname -sm") {
                let (_, platform) = ask("platform", "uname -sm", "");
                if !platform.starts_with("Linux") {
                    if platform.starts_with("Darwin") {
                        ask("native", "$resolved\" --version", "");
                    }
                    return self.frame(probes);
                }
            }
            let (native, _) = ask("native", "$resolved\" --version", "");
            if native == 0 && script.contains("cache_dir") {
                ask("cache_dir", "mbx cache dir --json", "");
            }
            if ask("config_dir", "XDG_CONFIG_HOME", "").0 == 0 {
                ask("config", "[ -f \"$1\" ]", "x");
            }
            self.frame(probes)
        }

        fn frame(&self, probes: Vec<(&str, i32, String, &str)>) -> Vec<u8> {
            let probes: Vec<(&str, i32, &str, &str)> = probes
                .iter()
                .map(|(name, status, stdout, stderr)| (*name, *status, stdout.as_str(), *stderr))
                .collect();
            targets::ssh_podman_probe_fixture(&probes)
        }
    }

    impl CommandExecutor for ProbeExecutor {
        fn pause_deadline(&self) -> mj_core::targets::DeadlinePause {
            mj_core::targets::DeadlinePause::none()
        }

        fn cancellation_requested_without_deadline(&self) -> bool {
            self.cancellation_requested()
        }

        fn execute_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            self.execute(command)
        }

        fn execute_with_stdin_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
            input: &mut (dyn std::io::Read + Send),
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            self.execute_with_stdin(command, input)
        }

        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            let line = format!("{} {}", command.program, command.args.join(" "));
            self.seen.lock().unwrap().push(line.clone());
            // Undo the quoting added by join_remote_command for fixture matching.
            let searchable = if command.program == "ssh" {
                line.replace("'\\''", "'").replace("' '", " ")
            } else {
                line.clone()
            };
            if searchable.contains("hel-mbx-discover") {
                return Ok(CommandOutput {
                    status: 0,
                    stdout: self.discovery(&searchable),
                    stderr: Vec::new(),
                });
            }
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
            ("hel-mbx-home", 0, "/home/dev"),
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

    // Hard-won: #1330: a Mac's working native mbx was reported as missing.
    #[test]
    #[cfg_attr(
        windows,
        ignore = "#1293: mbx cache host paths and Linux-host checks assume a Unix controller"
    )]
    fn darwin_hosts_report_native_mbx_without_sharing_it_with_containers() {
        let _isolated = isolated();
        for remote in [false, true] {
            for (enabled, native) in [None, Some(true)]
                .into_iter()
                .flat_map(|enabled| [(enabled, Some("1.16.0")), (enabled, None)])
            {
                // Each case asks its own host afresh.
                RESOLUTIONS.lock().expect("mbx resolutions").clear();
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
                let probe = native.map_or_else(String::new, |version| {
                    format!("/Users/builder/.local/bin/mbx\nmbx {version}")
                });
                let executor = ProbeExecutor::new(&[
                    ("uname -sm", 0, "Darwin arm64\n"),
                    (
                        "$resolved\" --version",
                        if native.is_some() { 0 } else { 1 },
                        &probe,
                    ),
                ]);
                let preview = preview_build_cache(&machine, &executor).unwrap().unwrap();
                // The Mac's own mbx is reported, even one too old to share.
                assert_eq!(preview.native_mbx.as_deref(), native);
                let Some(BuildCacheOff::UnsupportedPlatform(reason)) = &preview.off_reason else {
                    panic!("{:?}", preview.off_reason);
                };
                match native {
                    Some(version) => assert!(
                        reason.contains(&format!(
                            "mbx {version} runs natively on this Mac at /Users/builder/.local/bin/mbx"
                        )) && reason.contains("when it is on their login PATH"),
                        "{reason}"
                    ),
                    None => assert!(reason.contains("not installed on this Mac"), "{reason}"),
                }
                assert!(reason.contains("Linux in a virtual machine"), "{reason}");
                // The pinned Linux release cannot install or upgrade it.
                assert_eq!(install::install_kind(&preview), None);
                assert!(preview.directory.is_none());
                // An installed native mbx must not enter a container.
                assert!(resolve(&target, &executor).is_none());
                // Only the platform and the native mbx are asked about: no
                // GNU-only probes and no writes.
                let commands = executor.ran();
                assert!(
                    commands.iter().any(|command| command.contains("-sm"))
                        && commands.iter().all(|command| {
                            (command.contains("uname") && command.contains("-sm"))
                                || command.contains("--version")
                        }),
                    "{commands:?}"
                );
                assert!(
                    commands.iter().all(|command| {
                        !remote || command.starts_with("ssh ") && command.contains("mac.test")
                    }),
                    "{commands:?}"
                );
            }
        }
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "#1293: mbx cache host paths and Linux-host checks assume a Unix controller"
    )]
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
        assert!(
            executor.ran().iter().all(
                |command| command.starts_with("ssh ") && command.contains("builder@linux.test")
            )
        );
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "#1293: mbx cache host paths and Linux-host checks assume a Unix controller"
    )]
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
                .ran()
                .iter()
                .any(|command| command.contains("/home/builder/.config/mbx/config.toml"))
        );
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
    #[cfg_attr(
        windows,
        ignore = "#1293: mbx cache host paths and Linux-host checks assume a Unix controller"
    )]
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
        // Recorded even though it lies in the cache directory, so the session
        // keeps it if the host's configuration changes.
        assert_eq!(
            resolved.target_root,
            PathBuf::from("/mnt/fast/mbx-cache/targets")
        );
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
    #[cfg_attr(
        windows,
        ignore = "#1293: mbx cache host paths and Linux-host checks assume a Unix controller"
    )]
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
    #[cfg_attr(
        windows,
        ignore = "#1293: mbx cache host paths and Linux-host checks assume a Unix controller"
    )]
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
        assert_eq!(resolved.target_root, PathBuf::from("/mnt/slow/mbx-targets"));
        assert!(
            executor.ran().iter().any(|line| line.contains("src=$1")),
            "the store and the target root are probed as a pair: {:?}",
            executor.ran()
        );
    }

    #[test]
    fn the_effective_target_root_is_normalized_so_dot_dot_cannot_hide_an_outside_root() {
        let effective = |config: &str| effective_target_root(config, Path::new("/cache"));
        assert_eq!(effective(""), PathBuf::from("/cache/targets"));
        assert_eq!(
            effective("[target]\nroot = \"views\"\n"),
            PathBuf::from("/cache/views")
        );
        assert_eq!(
            effective("[target]\nroot = \"/cache/./targets\"\n"),
            PathBuf::from("/cache/targets")
        );
        let escaping = effective("[target]\nroot = \"../outside\"\n");
        assert_eq!(escaping, PathBuf::from("/outside"));
        assert!(!mj_core::target_path::is_within(
            &escaping,
            Path::new("/cache")
        ));
    }

    #[test]
    fn a_target_root_inside_the_cache_is_recorded_without_a_second_mount() {
        let _isolated = isolated();
        let executor = ProbeExecutor::new(&native_host());
        let mut mounts = Vec::new();
        // A Windows controller can share a Linux SSH host's cache, but not
        // its own local host cache with a Linux container.
        let target = TargetTemplate::SshPodman {
            ssh: SshTarget {
                destination: "builder@linux.test".into(),
                ssh_args: vec![],
            },
            container: container(None),
        };
        let build_cache = prepare(
            &target,
            &session(Some("/workspace/session-1")),
            &mut mounts,
            &executor,
        )
        .unwrap();
        assert_eq!(
            build_cache.target_root,
            Some(PathBuf::from("/mnt/fast/mbx-cache/targets"))
        );
        assert_eq!(build_cache.mounted_target_root(), None);
        assert_eq!(
            mounts
                .iter()
                .map(|mount| mount.destination.clone())
                .collect::<Vec<_>>(),
            [PathBuf::from("/mnt/fast/mbx-cache")]
        );
    }

    // Hard-won: #1597: broken configuration silently measured default filesystems.
    #[test]
    fn local_storage_config_reports_broken_files() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        assert!(read_local_storage_config(&path).unwrap().is_none());
        for text in ["[", "cache_dir = 3", "[target]\nroot = 3", "target = 3"] {
            std::fs::write(&path, text).unwrap();
            let text = read_local_storage_config(&path).unwrap().unwrap();
            assert!(
                resolve_local_storage_paths(Some("/override".into()), None, Some(&text), None)
                    .is_err()
            );
        }
        for text in [
            "# config\ncache_dir = \"https://user:super-secret-token@example.com\" invalid",
            "# config\n\"https://user:super-secret-token@example.com\" = 1\n\"https://user:super-secret-token@example.com\" = 2",
        ] {
            std::fs::write(&path, text).unwrap();
            let error = resolve_local_storage_paths(None, None, Some(text), None)
                .with_context(|| format!("resolve mbx config {}", path.display()))
                .unwrap_err();
            let error = format!("{error:#}");
            assert!(error.contains(&path.display().to_string()));
            assert!(error.contains("at line "), "{error}");
            assert!(!error.contains("super-secret-token"), "{error}");
        }
        std::fs::remove_file(&path).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(directory.path().join("missing"), &path).unwrap();
            let error = format!("{:#}", read_local_storage_config(&path).unwrap_err());
            assert!(error.contains(&path.display().to_string()));
            assert!(error.contains("dangling symlink"));
        }
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
            )
            .unwrap(),
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
            )
            .unwrap(),
            vec![
                PathBuf::from("/mnt/optane/mbx-cache"),
                PathBuf::from("/mnt/optane/mbx-cache/views"),
            ]
        );
        assert_eq!(
            resolve_local_storage_paths(None, None, None, Some(default_cache.clone())).unwrap(),
            vec![default_cache.clone(), default_cache.join("targets")]
        );
        assert_eq!(
            resolve_local_storage_paths(
                Some(PathBuf::from("/env/cache")),
                Some(PathBuf::from("views")),
                Some("cache_dir = \"/file/cache\"\n[target]\nroot = \"file-views\"\n"),
                Some(default_cache),
            )
            .unwrap(),
            vec![
                PathBuf::from("/env/cache"),
                PathBuf::from("/env/cache/views")
            ]
        );
    }

    // Hard-won: ecab42fb: non-login SSH PATH hid cargo-installed mbx and selected the wrong cache store
    #[test]
    #[cfg_attr(
        windows,
        ignore = "#1293: mbx cache host paths and Linux-host checks assume a Unix controller"
    )]
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
        // Container staging copies the executable the host probe found.
        let backend = targets::TargetLocator::LocalPodman {
            container_id: "saved-container".into(),
            workspace_storage: Default::default(),
            borrowed_from: None,
        };
        let NativeMbxLookup::Ready(native) = native_mbx_for_container(&backend, &executor).unwrap()
        else {
            panic!("the host mbx is compatible");
        };
        assert_eq!(native.program, PathBuf::from("/home/dev/.cargo/bin/mbx"));
        assert_eq!(native.version, MBX_VERSION);
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "#1293: mbx cache host paths and Linux-host checks assume a Unix controller"
    )]
    fn an_unrecognized_native_mbx_must_not_share_the_store() {
        let _isolated = isolated();
        let executor = ProbeExecutor::new(&[
            (
                "$resolved\" --version",
                0,
                "/home/jonathan/.local/bin/mbx\nmbx unknown",
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
                if reason.contains("unrecognized mbx version")
                    && reason.contains("Settings › Setup › Machines")
        ));
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "#1293: mbx cache host paths and Linux-host checks assume a Unix controller"
    )]
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
    #[cfg_attr(
        windows,
        ignore = "#1293: mbx cache host paths and Linux-host checks assume a Unix controller"
    )]
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
                    ..Default::default()
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
    #[cfg_attr(
        windows,
        ignore = "#1293: mbx cache host paths and Linux-host checks assume a Unix controller"
    )]
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
    #[cfg_attr(
        windows,
        ignore = "#1293: mbx cache host paths and Linux-host checks assume a Unix controller"
    )]
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
    fn session(container_workspace: Option<&str>) -> mj_core::state::SessionRecord {
        let mut record = crate::database::test_fixtures::checkpoint_test_session("session-1");
        record.container_workspace = container_workspace.map(PathBuf::from);
        record
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "#1293: mbx cache host paths and Linux-host checks assume a Unix controller"
    )]
    fn a_container_session_mounts_the_native_cache_and_writes_nothing_into_it() {
        let _isolated = isolated();
        let executor = ProbeExecutor::new(&native_host());
        let mut mounts = Vec::new();
        let build_cache = prepare(
            &podman(None),
            &session(Some("/workspace/session-1")),
            &mut mounts,
            &executor,
        )
        .expect("a container session uses the build cache");
        assert_eq!(build_cache.directory, PathBuf::from("/mnt/fast/mbx-cache"));
        // The executable and policy are staged per session at launch; nothing
        // is published into the shared cache directory.
        assert!(
            executor
                .ran()
                .iter()
                .all(|command| !command.contains(".mjolnir") && !command.contains("cp -- ")),
            "{:?}",
            executor.ran()
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
        let executor = ProbeExecutor::new(&native_host());
        let mut mounts = Vec::new();
        assert_eq!(
            prepare(&podman(None), &session(None), &mut mounts, &executor,),
            None
        );
        assert!(executor.ran().is_empty());
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "#1293: mbx cache host paths and Linux-host checks assume a Unix controller"
    )]
    fn a_resumed_session_resolves_the_host_cache_instead_of_its_saved_budget() {
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
        let build_cache = prepare(&podman(None), &record, &mut mounts, &executor).unwrap();
        assert_eq!(build_cache.max_size, None);
        assert_eq!(build_cache.directory, PathBuf::from("/mnt/fast/mbx-cache"));
        assert_eq!(mounts.len(), 1);
        assert!(
            executor
                .ran()
                .iter()
                .any(|line| line.contains("mj-reflink"))
        );
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "#1293: mbx cache host paths and Linux-host checks assume a Unix controller"
    )]
    fn a_session_moved_to_another_host_resolves_its_build_cache_again() {
        let _isolated = isolated();
        let executor = ProbeExecutor::new(&native_host());
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

        let build_cache = prepare(&podman(None), &record, &mut mounts, &executor)
            .expect("the destination host qualifies on its own");

        assert_eq!(build_cache.host, "local");
        assert_eq!(build_cache.directory, PathBuf::from("/mnt/fast/mbx-cache"));
        assert_eq!(
            build_cache.target_root,
            Some(PathBuf::from("/mnt/fast/mbx-cache/targets"))
        );
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
            classify_native_mbx(Some(native("1.22.0"))),
            NativeMbxStatus::Compatible(_)
        ));
        assert!(matches!(
            classify_native_mbx(Some(native(MBX_VERSION))),
            NativeMbxStatus::Compatible(_)
        ));
        assert!(matches!(
            classify_native_mbx(Some(native("1.15.0"))),
            NativeMbxStatus::Compatible(_)
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

    // Runs on macOS too, where the probe must work without `readlink -f`.
    #[cfg(unix)]
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
        // Relative links, one of them reached through a symlinked directory.
        symlink(&real_bin, root.path().join("alias")).unwrap();
        symlink("mbx-real", real_bin.join("mbx-hop")).unwrap();
        symlink("../alias/mbx-hop", path_bin.join("mbx")).unwrap();
        install(&local_bin.join("mbx"), "1.31.0");
        install(&cargo_bin.join("mbx"), "1.32.0");
        let readlink = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|directory| directory.join("readlink"))
            .find(|candidate| candidate.is_file())
            .expect("readlink utility is available");
        // Hard-won: #1330: the probe needed `readlink -f`, which macOS before 12.3 lacks.
        std::fs::write(
            path_bin.join("readlink"),
            format!(
                "#!/bin/sh\ncase $1 in -*) echo \"readlink: illegal option $1\" >&2; exit 1 ;; esac\nexec '{}' \"$@\"\n",
                readlink.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(
            path_bin.join("readlink"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();

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
                program: std::fs::canonicalize(&real_mbx).unwrap(),
                version: "1.30.0".into(),
            }
        );

        std::fs::remove_file(path_bin.join("mbx")).unwrap();
        let output = executor.execute(&command).unwrap();
        assert_eq!(output.status, 0);
        assert_eq!(
            parse_native_probe_output(&output.stdout).unwrap().program,
            std::fs::canonicalize(local_bin.join("mbx")).unwrap()
        );

        std::fs::remove_file(local_bin.join("mbx")).unwrap();
        let output = executor.execute(&command).unwrap();
        assert_eq!(output.status, 0);
        assert_eq!(
            parse_native_probe_output(&output.stdout).unwrap().program,
            std::fs::canonicalize(cargo_bin.join("mbx")).unwrap()
        );
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "#1293: mbx cache host paths and Linux-host checks assume a Unix controller"
    )]
    fn the_preview_reports_native_cache_values_without_creating_directories() {
        let _isolated = isolated();
        let executor = ProbeExecutor::new(&native_host());
        let preview = preview_build_cache(&configured_local_machine(), &executor)
            .unwrap()
            .unwrap();
        assert_eq!(preview.native_mbx.as_deref(), Some("1.22.0"));
        assert_eq!(preview.mbx_profile_file, None);
        assert_eq!(install::install_kind(&preview), None);
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
    // -- one SSH round trip per host -------------------------------------

    fn ssh_host() -> CacheHost {
        CacheHost::Ssh(SshTarget {
            destination: "builder@discover.test".into(),
            ssh_args: vec![],
        })
    }

    /// An SSH host that is a directory on this machine: its `HOME`, its `mbx`
    /// and its `uname` are fakes, and every `ssh` command runs its remote line
    /// in a real shell, exactly as the remote side would.
    #[cfg(unix)]
    struct FakeSshHost {
        _root: tempfile::TempDir,
        home: PathBuf,
        path: String,
        purposes: Mutex<Vec<String>>,
    }

    #[cfg(unix)]
    impl FakeSshHost {
        fn new(config: Option<&str>) -> Self {
            use std::os::unix::fs::PermissionsExt;

            let root = tempfile::tempdir().unwrap();
            let home = root.path().join("home");
            let bin = home.join(".local/bin");
            std::fs::create_dir_all(&bin).unwrap();
            let fake = |directory: &Path, name: &str, body: &str| {
                let path = directory.join(name);
                std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            };
            fake(
                &bin,
                "mbx",
                &format!(
                    "case \"$1\" in --version) echo 'mbx {MBX_VERSION}' ;; cache) echo '{{\"version\":1,\"store\":\"{}/actions\"}}' ;; esac",
                    root.path().join("cache").display()
                ),
            );
            let tools = root.path().join("tools");
            std::fs::create_dir_all(&tools).unwrap();
            fake(&tools, "uname", "echo 'Linux x86_64'");
            if let Some(config) = config {
                std::fs::create_dir_all(home.join(".config/mbx")).unwrap();
                std::fs::write(home.join(".config/mbx/config.toml"), config).unwrap();
            }
            Self {
                // Only system directories, so this machine's own mbx stays out.
                path: format!("{}:/usr/bin:/bin", tools.display()),
                home,
                _root: root,
                purposes: Mutex::new(Vec::new()),
            }
        }

        fn purposes(&self) -> Vec<String> {
            self.purposes.lock().unwrap().clone()
        }
    }

    #[cfg(unix)]
    impl CommandExecutor for FakeSshHost {
        fn pause_deadline(&self) -> mj_core::targets::DeadlinePause {
            mj_core::targets::DeadlinePause::none()
        }

        fn cancellation_requested_without_deadline(&self) -> bool {
            false
        }

        fn execute_without_deadline(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.execute(command)
        }

        fn execute_with_stdin_without_deadline(
            &self,
            command: &CommandSpec,
            _input: &mut (dyn std::io::Read + Send),
        ) -> Result<CommandOutput> {
            self.execute(command)
        }

        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.purposes.lock().unwrap().push(command.purpose.clone());
            assert_eq!(command.program, "ssh", "{command:?}");
            // The last argument is the whole remote command line.
            let remote = command.args.last().expect("ssh remote command");
            let mut shell = CommandSpec::new("/bin/sh", ["-c", remote.as_str()]);
            shell.clear_env = true;
            shell.env.insert("PATH".into(), self.path.clone());
            shell
                .env
                .insert("HOME".into(), self.home.display().to_string());
            targets::ProcessExecutor.execute(&shell)
        }
    }

    #[cfg(unix)]
    #[test]
    fn an_ssh_host_answers_every_discovery_question_in_one_command() {
        let _isolated = isolated();
        // Final newlines are the file's own and the container must get them.
        // Larger than a pipe buffer, so the framed output crosses one.
        let config = format!(
            "{}[gc]\nmax_total_size = \"90GB\"\n\n",
            "# padding\n".repeat(20_000)
        );
        let executor = FakeSshHost::new(Some(&config));
        let host = ssh_host();

        let facts =
            discovery::HostFacts::gather(&host, discovery::Scope::Inspection, &executor).unwrap();
        assert_eq!(facts.platform().unwrap(), HostPlatform::Linux);
        let native = facts.native().unwrap().expect("the host's mbx is found");
        assert_eq!(
            native,
            NativeMbx {
                program: std::fs::canonicalize(executor.home.join(".local/bin/mbx")).unwrap(),
                version: MBX_VERSION.into(),
            }
        );
        let directory = facts.cache_directory(&native).unwrap();
        assert_eq!(directory.file_name().unwrap(), "cache");
        assert_eq!(facts.config().unwrap().as_deref(), Some(config.as_str()));
        assert_eq!(
            executor.purposes(),
            ["discover the container host's build cache"]
        );

        // A host with no configuration file reports none rather than failing.
        let bare = FakeSshHost::new(None);
        let facts =
            discovery::HostFacts::gather(&host, discovery::Scope::Inspection, &bare).unwrap();
        assert_eq!(facts.config().unwrap(), None);
        assert_eq!(bare.purposes().len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn a_launching_session_reads_the_hosts_mbx_and_configuration_in_one_command() {
        let executor = FakeSshHost::new(Some(
            "cache_dir = \"/elsewhere\"\n[gc]\nmax_size = \"1GB\"\n",
        ));
        let target = targets::TargetLocator::SshPodman {
            ssh: SshTarget {
                destination: "builder@discover.test".into(),
                ssh_args: vec![],
            },
            container_id: "session-container".into(),
            workspace_storage: Default::default(),
            borrowed_from: None,
        };
        let cache = SessionBuildCache {
            host: "ssh:builder@discover.test".into(),
            directory: "/mnt/fast/mbx-cache".into(),
            max_size: None,
            target_root: Some("/mnt/fast/mbx-cache/targets".into()),
        };
        let (configuration, native) =
            native_mbx_and_config_for_container(&target, &cache, &executor).unwrap();
        // The host's `cache_dir` is pinned to the mounted one; other settings
        // pass through.
        let document: toml::Table = toml::from_str(&configuration).unwrap();
        assert_eq!(document["cache_dir"].as_str(), Some("/mnt/fast/mbx-cache"));
        assert_eq!(document["gc"]["max_size"].as_str(), Some("1GB"));
        let NativeMbxLookup::Ready(native) = native else {
            panic!("{native:?}");
        };
        assert_eq!(native.version, MBX_VERSION);
        assert_eq!(
            executor.purposes(),
            ["read the container host's mbx and its configuration"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_host_without_mbx_still_hands_over_its_configuration_for_a_legacy_container() {
        let _isolated = isolated();
        let executor = FakeSshHost::new(Some("[gc]\nmax_size = \"1GB\"\n"));
        std::fs::remove_file(executor.home.join(".local/bin/mbx")).unwrap();
        let target = targets::TargetLocator::SshDocker {
            ssh: SshTarget {
                destination: "builder@discover.test".into(),
                ssh_args: vec![],
            },
            container_id: "session-container".into(),
            borrowed_from: None,
        };
        let cache = SessionBuildCache {
            host: "ssh:builder@discover.test".into(),
            directory: "/mnt/fast/mbx-cache".into(),
            max_size: None,
            target_root: None,
        };
        let (configuration, native) =
            native_mbx_and_config_for_container(&target, &cache, &executor).unwrap();
        assert!(configuration.contains("1GB"), "{configuration}");
        let NativeMbxLookup::Unavailable(reason) = native else {
            panic!("{native:?}");
        };
        assert!(
            reason.contains("mbx is not installed on the container host"),
            "{reason}"
        );
        assert_eq!(executor.purposes().len(), 1);
    }

    /// Lets a test hold an inspection inside its first host command.
    struct Gate {
        started: std::sync::atomic::AtomicUsize,
        open: Mutex<bool>,
        opened: Condvar,
    }

    impl Gate {
        fn new() -> Self {
            Self {
                started: 0.into(),
                open: Mutex::new(false),
                opened: Condvar::new(),
            }
        }

        fn started(&self) -> usize {
            self.started.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn open(&self) {
            *self.open.lock().unwrap() = true;
            self.opened.notify_all();
        }

        fn wait_until_started(&self, count: usize) {
            let deadline = Instant::now() + Duration::from_secs(30);
            while self.started() < count {
                assert!(Instant::now() < deadline, "no inspection started");
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    /// Answers like [`ProbeExecutor`], except that the discovery command
    /// waits at the gate, and may panic once it is released.
    struct GatedExecutor<'a> {
        gate: &'a Gate,
        inner: &'a ProbeExecutor,
        cancelled: bool,
        panic_after_gate: bool,
        /// Cancelled once the gate opens: the caller is cancelled while its
        /// inspection is running, and the discovery command then fails.
        cancel_after_gate: bool,
    }

    impl GatedExecutor<'_> {
        fn is_cancelled(&self) -> bool {
            self.cancelled || (self.cancel_after_gate && *self.gate.open.lock().unwrap())
        }
    }

    impl CommandExecutor for GatedExecutor<'_> {
        fn pause_deadline(&self) -> mj_core::targets::DeadlinePause {
            mj_core::targets::DeadlinePause::none()
        }

        fn cancellation_requested_without_deadline(&self) -> bool {
            self.is_cancelled()
        }

        fn cancellation_requested(&self) -> bool {
            self.is_cancelled()
        }

        fn execute_without_deadline(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.execute(command)
        }

        fn execute_with_stdin_without_deadline(
            &self,
            command: &CommandSpec,
            input: &mut (dyn std::io::Read + Send),
        ) -> Result<CommandOutput> {
            self.inner.execute_with_stdin(command, input)
        }

        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            if command.purpose.starts_with("discover the container host") {
                self.gate
                    .started
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut open = self.gate.open.lock().unwrap();
                while !*open {
                    open = self.gate.opened.wait(open).unwrap();
                }
                drop(open);
                assert!(!self.panic_after_gate, "the host inspection panicked");
                if self.cancel_after_gate {
                    anyhow::bail!("cancelled while inspecting the host");
                }
            }
            self.inner.execute(command)
        }
    }

    fn inspected_directory(executor: &impl CommandExecutor) -> Result<Option<PathBuf>> {
        inspect(
            &ssh_host(),
            &TargetBuildCache::default(),
            Freshness::Memoized,
            executor,
        )
        .map(|inspection| inspection.cache.map(|cache| cache.directory))
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "#1293: mbx cache host paths and Linux-host checks assume a Unix controller"
    )]
    fn concurrent_sessions_share_one_host_inspection_and_a_cancelled_waiter_leaves() {
        let _isolated = isolated();
        let probe = ProbeExecutor::new(&native_host());
        let gate = Gate::new();
        let owner = GatedExecutor {
            gate: &gate,
            inner: &probe,
            cancelled: false,
            panic_after_gate: false,
            cancel_after_gate: false,
        };
        let waiter = owner_ref(&gate, &probe);
        let cancelled = GatedExecutor {
            cancelled: true,
            ..owner_ref(&gate, &probe)
        };
        std::thread::scope(|scope| {
            let first = scope.spawn(|| inspected_directory(&owner));
            gate.wait_until_started(1);
            let second = scope.spawn(|| inspected_directory(&waiter));
            // The waiter that was cancelled leaves at once, without holding up
            // or corrupting the inspection it was waiting for.
            let started = Instant::now();
            let error = inspected_directory(&cancelled).unwrap_err();
            assert!(format!("{error:#}").contains("cancelled"), "{error:#}");
            assert!(started.elapsed() < Duration::from_secs(10));
            // Give the second caller every chance to start a second probe.
            std::thread::sleep(Duration::from_millis(400));
            assert!(
                !second.is_finished(),
                "the second caller waits for the first"
            );
            assert_eq!(gate.started(), 1, "the host is asked once");
            gate.open();
            let expected = Some(PathBuf::from("/mnt/fast/mbx-cache"));
            assert_eq!(first.join().unwrap().unwrap(), expected);
            assert_eq!(second.join().unwrap().unwrap(), expected);
        });
        assert_eq!(gate.started(), 1, "both callers shared one inspection");
        // Memoized: a later caller is answered without asking the host.
        let ran = probe.ran().len();
        assert!(inspected_directory(&owner).unwrap().is_some());
        assert_eq!(probe.ran().len(), ran);
    }

    fn owner_ref<'a>(gate: &'a Gate, inner: &'a ProbeExecutor) -> GatedExecutor<'a> {
        GatedExecutor {
            gate,
            inner,
            cancelled: false,
            panic_after_gate: false,
            cancel_after_gate: false,
        }
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "#1293: mbx cache host paths and Linux-host checks assume a Unix controller"
    )]
    fn a_panicking_inspection_frees_its_slot_for_waiters_and_later_callers() {
        let _isolated = isolated();
        let probe = ProbeExecutor::new(&native_host());
        let gate = Gate::new();
        let doomed = GatedExecutor {
            panic_after_gate: true,
            ..owner_ref(&gate, &probe)
        };
        let healthy = owner_ref(&gate, &probe);
        std::thread::scope(|scope| {
            let first = scope.spawn(|| inspected_directory(&doomed));
            gate.wait_until_started(1);
            let second = scope.spawn(|| inspected_directory(&healthy));
            std::thread::sleep(Duration::from_millis(300));
            gate.open();
            assert!(first.join().is_err(), "the first inspection panicked");
            // The waiter was not left pending: it asked the host itself.
            assert_eq!(
                second.join().unwrap().unwrap(),
                Some(PathBuf::from("/mnt/fast/mbx-cache"))
            );
        });
        assert_eq!(gate.started(), 2);
        assert!(
            lock_resolutions()
                .values()
                .all(|resolution| matches!(resolution, Resolution::Ready(..)))
        );
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "#1293: mbx cache host paths and Linux-host checks assume a Unix controller"
    )]
    fn a_fresh_inspection_replaces_the_answer_a_slow_older_one_would_publish() {
        let _isolated = isolated();
        let stale = ProbeExecutor::new(&native_host());
        let repaired: Vec<_> = native_host()
            .into_iter()
            .map(|(needle, status, stdout)| {
                if needle == "mbx cache dir --json" {
                    (
                        needle,
                        status,
                        r#"{"version":1,"store":"/mnt/repaired/actions"}"#,
                    )
                } else if needle == "while [ ! -d" {
                    (needle, status, "/mnt/repaired")
                } else {
                    (needle, status, stdout)
                }
            })
            .collect();
        let fresh = ProbeExecutor::new(&repaired);
        let gate = Gate::new();
        let open_gate = Gate::new();
        open_gate.open();
        let slow = owner_ref(&gate, &stale);
        let joined = owner_ref(&gate, &stale);
        std::thread::scope(|scope| {
            let memoized = scope.spawn(|| inspected_directory(&slow));
            gate.wait_until_started(1);
            // A session that joined the slow inspection while it ran.
            let waiter = scope.spawn(|| inspected_directory(&joined));
            std::thread::sleep(Duration::from_millis(300));
            // The settings screen reads the host while the session's
            // inspection is still running, and its answer wins.
            let screen = inspect(
                &ssh_host(),
                &TargetBuildCache::default(),
                Freshness::Fresh,
                &owner_ref(&open_gate, &fresh),
            )
            .unwrap();
            assert_eq!(
                screen.cache.unwrap().directory,
                PathBuf::from("/mnt/repaired")
            );
            gate.open();
            // The session still gets the answer its own inspection found.
            assert_eq!(
                memoized.join().unwrap().unwrap(),
                Some(PathBuf::from("/mnt/fast/mbx-cache"))
            );
            // The one that joined it is not handed that older answer: it
            // looks again and finds the fresher one.
            assert_eq!(
                waiter.join().unwrap().unwrap(),
                Some(PathBuf::from("/mnt/repaired"))
            );
        });
        assert_eq!(gate.started(), 1, "the waiter did not ask the host itself");
        assert_eq!(
            inspected_directory(&fresh).unwrap(),
            Some(PathBuf::from("/mnt/repaired")),
            "the slow inspection did not overwrite the fresher answer"
        );
    }
    #[test]
    #[cfg_attr(
        windows,
        ignore = "#1293: mbx cache host paths and Linux-host checks assume a Unix controller"
    )]
    fn a_cancelled_inspection_is_never_remembered_and_its_waiters_ask_the_host_themselves() {
        let _isolated = isolated();
        let probe = ProbeExecutor::new(&native_host());
        let gate = Gate::new();
        let healthy = owner_ref(&gate, &probe);
        let cancelled_owner = GatedExecutor {
            cancel_after_gate: true,
            ..owner_ref(&gate, &probe)
        };
        std::thread::scope(|scope| {
            let owner = scope.spawn(|| inspected_directory(&cancelled_owner));
            gate.wait_until_started(1);
            let waiter = scope.spawn(|| inspected_directory(&healthy));
            std::thread::sleep(Duration::from_millis(300));
            assert!(!waiter.is_finished(), "the waiter waits for the owner");
            gate.open();
            let error = owner.join().unwrap().unwrap_err();
            assert!(format!("{error:#}").contains("cancelled"), "{error:#}");
            // The waiter was not handed the owner's cancellation: it asked
            // the healthy host itself.
            assert_eq!(
                waiter.join().unwrap().unwrap(),
                Some(PathBuf::from("/mnt/fast/mbx-cache"))
            );
        });
        assert_eq!(gate.started(), 2);

        // With nobody waiting, the cancellation is not cached for later
        // callers either, on the memoized path or the fresh one.
        for freshness in [Freshness::Memoized, Freshness::Fresh] {
            lock_resolutions().clear();
            assert!(
                inspect(
                    &ssh_host(),
                    &TargetBuildCache::default(),
                    freshness,
                    &cancelled_owner
                )
                .is_err()
            );
            assert!(
                lock_resolutions().is_empty(),
                "{freshness:?} remembered a cancelled caller's failure"
            );
            assert_eq!(
                inspected_directory(&healthy).unwrap(),
                Some(PathBuf::from("/mnt/fast/mbx-cache"))
            );
        }
    }
}
