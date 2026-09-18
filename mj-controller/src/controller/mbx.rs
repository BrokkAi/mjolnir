//! The shared mbx build cache for Rust container sessions.
//!
//! mbx wraps Cargo: a binary named `cargo` that is really `mbx` intercepts the
//! build, looks every compiler action up in a content-addressed store, and
//! restores cached outputs instead of recompiling. Its store is an ordinary
//! directory on the container host, which every mj container on that host
//! mounts read-write at the same absolute path. Nothing is synchronized
//! between hosts and mj never runs mbx garbage collection.
//!
//! Every failure here means the session runs without the cache. Nothing in
//! this module ever fails provisioning.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};

use super::cache_host::CacheHost;
use crate::targets::{self, CommandExecutor, CommandOutput, CommandSpec};
use mj_core::config::{BuildCacheConfig, TargetBuildCache};
use mj_core::state::{BuildCacheLimit, BuildCachePreview, SessionBuildCache};

/// The mbx release containers run. A native mbx older than this must not share
/// the same store, so a host that has one runs its sessions without the cache.
pub(super) const MBX_VERSION: &str = "1.12.0";

const MBX_X86_64_SHA256: &str = "b0d90013e5e4e55419b75db897a6b9eed0f0e3b7bc49edf355e0e6490fda6de2";
const MBX_AARCH64_SHA256: &str = "738b97bf260137aed70cd3cf849925d1f1bec5f1ba446f5e61ed851ae649bc7b";

/// Overrides the download with a local mbx binary for the current machine's
/// architecture. Used for development against an unreleased mbx.
const MBX_BINARY_ENV: &str = "MJ_MBX_BINARY";

const DEFAULT_CACHE_RELATIVE: &str = ".cache/mbx";
const HOST_CONFIG_RELATIVE: &str = ".config/mbx/config.toml";
/// The cap on the computed default budget: 100 GB, in SI bytes.
const DEFAULT_MAX_BYTES: u64 = 100_000_000_000;
const RESOLUTION_LIFETIME: Duration = Duration::from_secs(600);
const LABEL: &str = "hel-mbx";

/// What a container target's host offers as a build cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ResolvedBuildCache {
    /// Cache directory on the host, mounted at the same path in the container.
    pub directory: PathBuf,
    /// `MBX_GC_MAX_SIZE` for the container, or `None` when the host's own mbx
    /// configuration file already carries the budget.
    pub max_size: Option<String>,
    /// A `[target] root` the host configuration relocates outside the cache
    /// directory, which the container needs mounted at the same path too.
    pub target_root: Option<PathBuf>,
    /// The host's `~/.config/mbx/config.toml`, copied into the container so
    /// its mbx uses the host's own limits.
    pub config_file: Option<String>,
}

/// Cached host answers, keyed by host and per-target settings. Resolution runs
/// several commands on the host, and a burst of new sessions must not repeat
/// them for each one.
type Resolutions = std::collections::BTreeMap<String, (Instant, Option<ResolvedBuildCache>)>;

static RESOLUTIONS: std::sync::LazyLock<std::sync::Mutex<Resolutions>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(Resolutions::new()));

/// Resolve the build cache for one container target, or `None` when this
/// target runs without one. The answer is cached for ten minutes.
pub(super) fn resolve(
    target: &targets::TargetTemplate,
    global: &BuildCacheConfig,
    executor: &impl CommandExecutor,
) -> Option<ResolvedBuildCache> {
    if !global.enabled {
        return None;
    }
    let (host, settings) = supported_host(target)?;
    let key = format!("{}|{settings:?}", host.key());
    if let Some((recorded, resolution)) = RESOLUTIONS.lock().expect("mbx resolutions").get(&key)
        && recorded.elapsed() < RESOLUTION_LIFETIME
    {
        return resolution.clone();
    }
    let resolution = match resolve_host(&host, &settings, executor) {
        Ok(resolution) => resolution,
        Err(error) => {
            tracing::warn!(host = key, "build cache unavailable: {error:#}");
            None
        }
    };
    RESOLUTIONS
        .lock()
        .expect("mbx resolutions")
        .insert(key, (Instant::now(), resolution.clone()));
    resolution
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

fn resolve_host(
    host: &CacheHost,
    settings: &TargetBuildCache,
    executor: &impl CommandExecutor,
) -> Result<Option<ResolvedBuildCache>> {
    let inspection = inspect_host(host, settings, executor)?;
    match inspection.cache {
        Some(cache) => {
            create_directory(host, &cache.directory, executor)?;
            Ok(Some(cache))
        }
        None => {
            if let Some(reason) = &inspection.preview.off_reason {
                tracing::warn!(
                    directory = inspection
                        .preview
                        .directory
                        .as_ref()
                        .map(|d| d.display().to_string()),
                    "sessions on this target run without the build cache: {reason}"
                );
            }
            Ok(None)
        }
    }
}

/// What the settings screen shows for one machine's blank build cache fields:
/// the same host inspection a session runs, without creating the directory.
/// `None` when the machine has no standing host to share a cache on.
pub fn preview_build_cache(
    machine: &mj_core::config::Machine,
    global: &BuildCacheConfig,
    executor: &impl CommandExecutor,
) -> Result<Option<BuildCachePreview>> {
    let Some(host) = CacheHost::for_machine(machine) else {
        return Ok(None);
    };
    let settings = machine.build_cache().cloned().unwrap_or_default();
    if !global.enabled {
        return Ok(Some(BuildCachePreview {
            native_mbx: None,
            directory: None,
            max_size: None,
            off_reason: Some("the build cache is turned off for every machine".into()),
        }));
    }
    inspect_host(&host, &settings, executor).map(|inspection| Some(inspection.preview))
}

/// Everything the host says about a target's build cache, read without
/// changing the host.
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
    let native = native_version(host, executor);
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
            off_reason: Some(format!(
                "the host's mbx {version} is older than the {MBX_VERSION} Mjolnir installs, \
                 so they cannot share a store"
            )),
        }));
    }
    let directory = match &settings.directory {
        Some(directory) => directory.clone(),
        None => match &native {
            Some(native) => native_cache_directory(host, native, executor)?,
            None => host.home(executor)?.join(DEFAULT_CACHE_RELATIVE),
        },
    };
    ensure!(
        directory.is_absolute(),
        "build cache directory {} is not absolute",
        directory.display()
    );

    let config_file = host_config_file(host, executor)?;
    let target_root = config_file
        .as_deref()
        .and_then(|text| relocated_target_root(text, &directory));

    let max_size = match (&settings.max_size, &config_file) {
        (Some(max_size), _) => Some(max_size.clone()),
        // The host's own file carries its budgets; a second one would fight it.
        (None, Some(_)) => None,
        (None, None) => Some(default_max_size(host, &directory, executor)?),
    };
    let limit = match (&max_size, &config_file) {
        (Some(max_size), _) => BuildCacheLimit::Size(max_size.clone()),
        (None, Some(text)) => BuildCacheLimit::HostConfiguration(configured_max_size(text)),
        (None, None) => unreachable!("a missing budget is derived above"),
    };
    let preview = |off_reason: Option<String>| BuildCachePreview {
        native_mbx: native_version.clone(),
        directory: Some(directory.clone()),
        max_size: Some(limit.clone()),
        off_reason,
    };

    // The directory may not exist yet; its filesystem is its nearest
    // existing ancestor's.
    let volume = nearest_existing_ancestor(host, &directory, executor)?;
    let enabled = match settings.enabled {
        Some(enabled) => enabled,
        None => reflinks_supported(host, &volume, executor)?,
    };
    if !enabled {
        let reason = if settings.enabled == Some(false) {
            "turned off for this target".to_owned()
        } else {
            format!(
                "the filesystem under {} does not support reflinks, so restoring cached \
                 outputs would copy every byte",
                directory.display()
            )
        };
        return Ok(off(preview(Some(reason))));
    }
    if let Some(reason) = unusable_filesystem(host, &volume, executor)? {
        return Ok(off(preview(Some(format!(
            "{} is on a {reason}, where mbx's file locks are unreliable",
            directory.display()
        )))));
    }

    Ok(Inspection {
        preview: preview(None),
        cache: Some(ResolvedBuildCache {
            directory,
            max_size,
            target_root,
            config_file,
        }),
    })
}

/// The `gc.max_size` a host configuration sets, for display only.
fn configured_max_size(config_file: &str) -> Option<String> {
    let document: toml::Value = toml::from_str(config_file).ok()?;
    document
        .get("gc")?
        .get("max_size")?
        .as_str()
        .map(str::to_owned)
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
fn native_version(host: &CacheHost, executor: &impl CommandExecutor) -> Option<NativeMbx> {
    let command = host.shell_command(
        NATIVE_VERSION_SCRIPT,
        LABEL,
        [],
        "read the container host mbx version",
    );
    let output = executor.execute(&command).ok()?;
    if output.status != 0 {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let (program, version) = text.trim().split_once('\n')?;
    Some(NativeMbx {
        program: program.to_owned(),
        version: version.split_whitespace().next_back()?.to_owned(),
    })
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
    let path = host.home(executor)?.join(HOST_CONFIG_RELATIVE);
    let command = host.shell_command(
        READ_CONFIG_SCRIPT,
        LABEL,
        [path.to_string_lossy().into_owned()],
        "read the container host mbx configuration",
    );
    let output = executor.execute(&command)?;
    if output.status == 3 {
        return Ok(None);
    }
    let output = checked(output, &command)?;
    Ok(Some(
        String::from_utf8(output.stdout).context("decode the host mbx configuration")?,
    ))
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
/// value to record on the session. A session that already carries a decision
/// (resume, move, or a sub-agent child) reuses it without resolving again.
pub(super) fn prepare(
    target: &targets::TargetTemplate,
    global: &BuildCacheConfig,
    session: &mj_core::state::SessionRecord,
    bundle: Option<&targets::ProjectBundleSpec>,
    clone_cache: Option<&super::git_cache::PreparedCloneCache>,
    mounts: &mut Vec<targets::AdditionalMount>,
    executor: &impl CommandExecutor,
) -> Option<SessionBuildCache> {
    // A recorded cache is a directory on one particular host, so it only
    // survives a resume that stays on that host.
    let host_key = supported_host(target).map(|(host, _)| host.key());
    if let Some(recorded) = &session.build_cache {
        if host_key.as_deref() == Some(recorded.host.as_str()) {
            return attach_mounts(recorded, mounts).then(|| recorded.clone());
        }
        tracing::info!(
            session_id = session.id,
            recorded_host = recorded.host,
            host = host_key.as_deref().unwrap_or("unsupported target"),
            "the session moved to another container host, so its build cache is resolved again"
        );
    }
    // A session at the legacy shared `/workspace` would collide with every
    // other legacy session in mbx's path-keyed records.
    session.container_workspace.as_ref()?;
    let resolved = resolve(target, global, executor)?;
    let host = supported_host(target)?.0;
    let mirror = clone_cache?.mirror_for(&bundle?.primary)?;
    if !primary_repository_is_rust(&host, mirror, executor) {
        return None;
    }
    let build_cache = SessionBuildCache {
        host: host.key(),
        directory: resolved.directory,
        max_size: resolved.max_size,
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

/// The host's mbx configuration file for this target, read through the cached
/// resolution so the worker install does not repeat the host commands.
pub(super) fn host_configuration(
    target: &targets::TargetTemplate,
    global: &BuildCacheConfig,
    executor: &impl CommandExecutor,
) -> Option<String> {
    resolve(target, global, executor)?.config_file
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
            for (needle, status, stdout) in &self.answers {
                if line.contains(needle) {
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
    fn a_native_mbx_supplies_the_cache_directory_and_its_own_limits() {
        let _isolated = isolated();
        let executor = ProbeExecutor::new(&[
            ("$m\" --version", 0, "mbx\nmbx 1.12.0"),
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
        let resolved = resolve(&podman(None), &BuildCacheConfig::default(), &executor).unwrap();
        assert_eq!(resolved.directory, PathBuf::from("/mnt/fast/mbx-cache"));
        // The host's own configuration file carries the budget.
        assert_eq!(resolved.max_size, None);
        assert_eq!(resolved.target_root, None);
        assert!(resolved.config_file.unwrap().contains("500GiB"));
        assert!(
            !executor.ran().iter().any(|line| line.contains("df -B1")),
            "a host with its own configuration is not measured"
        );
    }

    #[test]
    fn a_relocated_target_root_is_reported_for_its_own_mount() {
        let _isolated = isolated();
        let executor = ProbeExecutor::new(&[
            ("$m\" --version", 0, "mbx\nmbx 1.12.0"),
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
        let resolved = resolve(&podman(None), &BuildCacheConfig::default(), &executor).unwrap();
        assert_eq!(
            resolved.target_root,
            Some(PathBuf::from("/mnt/fast/mbx-targets"))
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
        let mut answers = plain_host();
        answers.retain(|(needle, _, _)| *needle != "$m\" --version");
        answers.push(("$m\" --version", 0, "/home/dev/.cargo/bin/mbx\nmbx 1.12.0"));
        answers.push((
            "/home/dev/.cargo/bin/mbx cache dir --json",
            0,
            r#"{"version":1,"store":"/mnt/fast/mbx-cache/actions"}"#,
        ));
        let executor = ProbeExecutor::new(&answers);
        let resolved = resolve(&podman(None), &BuildCacheConfig::default(), &executor).unwrap();
        assert_eq!(resolved.directory, PathBuf::from("/mnt/fast/mbx-cache"));
    }

    #[test]
    fn an_older_native_mbx_must_not_share_the_store() {
        let _isolated = isolated();
        let executor = ProbeExecutor::new(&[("$m\" --version", 0, "mbx\nmbx 1.11.9")]);
        assert_eq!(
            resolve(&podman(None), &BuildCacheConfig::default(), &executor),
            None
        );
    }

    #[test]
    fn a_host_without_mbx_falls_back_to_the_default_cache_directory() {
        let _isolated = isolated();
        let executor = ProbeExecutor::new(&plain_host());
        let resolved = resolve(&podman(None), &BuildCacheConfig::default(), &executor).unwrap();
        assert_eq!(resolved.directory, default_cache_directory());
        // min(100 GB, 800 GB / 4) is the 100 GB cap.
        assert_eq!(resolved.max_size.as_deref(), Some("100000000000B"));
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
        let resolved = resolve(&podman(None), &BuildCacheConfig::default(), &executor).unwrap();
        assert_eq!(resolved.max_size.as_deref(), Some("10000000B"));
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
            })),
            &BuildCacheConfig::default(),
            &executor,
        )
        .unwrap();
        assert_eq!(resolved.directory, PathBuf::from("/mnt/nvme/mbx"));
        assert_eq!(resolved.max_size.as_deref(), Some("250GiB"));
        assert!(
            !executor
                .ran()
                .iter()
                .any(|line| line.contains("mj-reflink")),
            "an explicit enabled setting skips the reflink probe"
        );
    }

    #[test]
    fn a_volume_without_reflinks_runs_without_the_cache() {
        let _isolated = isolated();
        let mut answers = plain_host();
        answers.retain(|(needle, _, _)| *needle != "mj-reflink");
        answers.push(("mj-reflink", 1, ""));
        let executor = ProbeExecutor::new(&answers);
        assert_eq!(
            resolve(&podman(None), &BuildCacheConfig::default(), &executor),
            None
        );
    }

    #[test]
    fn the_preview_names_the_resolved_values_and_the_reason_the_cache_is_off() {
        let _isolated = isolated();
        let mut answers = plain_host();
        answers.retain(|(needle, _, _)| *needle != "mj-reflink");
        answers.push(("mj-reflink", 1, ""));
        let executor = ProbeExecutor::new(&answers);
        let preview = preview_build_cache(
            &configured_local_machine(),
            &BuildCacheConfig::default(),
            &executor,
        )
        .unwrap()
        .unwrap();
        assert_eq!(preview.native_mbx, None);
        assert_eq!(preview.directory, Some(default_cache_directory()));
        assert_eq!(
            preview.max_size,
            Some(BuildCacheLimit::Size("100000000000B".into()))
        );
        assert!(
            preview
                .off_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("reflinks")),
            "{:?}",
            preview.off_reason
        );
        // A preview reads the host; it never creates the directory.
        assert!(!executor.ran().iter().any(|line| line.contains("mkdir")));

        let executor = ProbeExecutor::new(&[
            ("$m\" --version", 0, "mbx\nmbx 1.12.0"),
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
        let preview = preview_build_cache(
            &configured_local_machine(),
            &BuildCacheConfig::default(),
            &executor,
        )
        .unwrap()
        .unwrap();
        assert_eq!(preview.native_mbx.as_deref(), Some("1.12.0"));
        assert_eq!(
            preview.directory,
            Some(PathBuf::from("/mnt/fast/mbx-cache"))
        );
        assert_eq!(
            preview.max_size,
            Some(BuildCacheLimit::HostConfiguration(Some("500GiB".into())))
        );
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
        assert_eq!(
            resolve(&podman(None), &BuildCacheConfig::default(), &executor),
            None
        );
    }

    #[test]
    fn the_global_switch_short_circuits_every_host_command() {
        let _isolated = isolated();
        let executor = ProbeExecutor::new(&plain_host());
        assert_eq!(
            resolve(
                &podman(None),
                &BuildCacheConfig { enabled: false },
                &executor
            ),
            None
        );
        assert!(executor.ran().is_empty());
    }

    #[test]
    fn local_podman_and_local_docker_inspect_one_machine_once() {
        let _isolated = isolated();
        let executor = ProbeExecutor::new(&plain_host());
        let settings = BuildCacheConfig::default();
        let first = resolve(&podman(None), &settings, &executor).unwrap();
        let ran = executor.ran().len();
        assert!(ran > 0, "the first resolve inspects the host");
        let second = resolve(&docker(None), &settings, &executor).unwrap();
        assert_eq!(
            first, second,
            "both engines on this machine share one cache"
        );
        assert_eq!(
            executor.ran().len(),
            ran,
            "the second runtime is answered from the machine's recorded inspection: {:?}",
            executor.ran()
        );
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
        assert_eq!(
            preview_build_cache(&fleet, &BuildCacheConfig::default(), &executor).unwrap(),
            None
        );
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
            assert_eq!(
                resolve(&target, &BuildCacheConfig::default(), &executor),
                None,
                "{target:?}"
            );
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
            &BuildCacheConfig::default(),
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
                &BuildCacheConfig::default(),
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
                &BuildCacheConfig::default(),
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
                &BuildCacheConfig::default(),
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
                &BuildCacheConfig::default(),
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
    fn a_resumed_session_reuses_its_recorded_cache_without_resolving_again() {
        let _isolated = isolated();
        let executor = ProbeExecutor::new(&[]);
        let mut record = session(Some("/workspace/session-1"));
        record.build_cache = Some(SessionBuildCache {
            host: "local".into(),
            directory: PathBuf::from("/mnt/fast/mbx-cache"),
            max_size: None,
            target_root: Some(PathBuf::from("/mnt/fast/mbx-targets")),
        });
        let mut mounts = Vec::new();
        let build_cache = prepare(
            &podman(None),
            &BuildCacheConfig::default(),
            &record,
            None,
            None,
            &mut mounts,
            &executor,
        )
        .expect("a resumed session keeps its build cache");
        assert_eq!(build_cache, record.build_cache.unwrap());
        assert_eq!(
            mounts
                .iter()
                .map(|mount| mount.destination.clone())
                .collect::<Vec<_>>(),
            vec![
                PathBuf::from("/mnt/fast/mbx-cache"),
                PathBuf::from("/mnt/fast/mbx-targets"),
            ]
        );
        assert!(executor.ran().is_empty());
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
            &BuildCacheConfig::default(),
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
