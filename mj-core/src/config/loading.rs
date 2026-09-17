//! Instance names, configuration directories and atomic file writes.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

use super::{CONFIG_VERSION, PRODUCT_DIR};

pub(super) fn reject_non_bare_permissions(contents: &str) -> Result<()> {
    let value: toml::Value = contents.parse().context("parse Mjolnir config TOML")?;
    let Some(targets) = value.get("targets").and_then(toml::Value::as_table) else {
        return Ok(());
    };
    for (id, target) in targets {
        let Some(target) = target.as_table() else {
            continue;
        };
        if target.contains_key("permissions")
            && target.get("kind").and_then(toml::Value::as_str) != Some("ssh-bare")
        {
            bail!("target {id:?} sets `permissions`, which is only valid for ssh-bare targets");
        }
    }
    Ok(())
}

/// The config version in `document` when it is above this build's.
pub(super) fn newer_version(document: &toml::Value) -> Option<u32> {
    let version = document.get("version")?.as_integer()?;
    (version > i64::from(CONFIG_VERSION)).then(|| u32::try_from(version).unwrap_or(u32::MAX))
}

/// The config version at `path` when it is above this build's. Read
/// tolerantly: a missing or unreadable file never blocks a save.
pub fn newer_version_on_disk(path: &Path) -> Option<u32> {
    let contents = fs::read_to_string(path).ok()?;
    newer_version(&contents.parse::<toml::Value>().ok()?)
}

pub(super) fn reject_removed_profile_overrides(contents: &str) -> Result<()> {
    let value: toml::Value = contents.parse().context("parse Mjolnir config TOML")?;
    let Some(profiles) = value.get("profiles").and_then(toml::Value::as_table) else {
        return Ok(());
    };
    for (id, profile) in profiles {
        let Some(profile) = profile.as_table() else {
            continue;
        };
        for key in ["model", "reasoning_effort"] {
            if profile.contains_key(key) {
                bail!(
                    "profile {id:?}: `{key}` is no longer supported; configure it in the harness home or change it per session with `/config`"
                );
            }
        }
    }
    Ok(())
}

/// Read a configuration override under its `MJ_` name. Mjolnir shares no
/// state or environment with hel installs; there is no legacy fallback.
pub fn env_override_os(name: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(format!("MJ_{name}"))
}

/// String form of [`env_override_os`] for overrides parsed as UTF-8.
pub fn env_override(name: &str) -> Option<String> {
    std::env::var(format!("MJ_{name}")).ok()
}

/// Environment variable selecting an isolated Mjolnir instance. An instance
/// keeps its own configuration, database, daemon, and logs, so `MJ_INSTANCE=dev`
/// never shares state with the default setup.
pub const INSTANCE_ENV: &str = "MJ_INSTANCE";

/// Directory under the default configuration and data roots holding one
/// isolated instance's files (`<root>/mjolnir/instances/<name>`).
const INSTANCE_DIR: &str = "instances";

/// Instance name from [`INSTANCE_ENV`], trimmed. Empty means no instance.
pub fn instance_name() -> Option<String> {
    let name = env_override("INSTANCE")?;
    let trimmed = name.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// Whether `name` is safe to use as a single path segment under [`INSTANCE_DIR`].
pub fn is_valid_instance_name(name: &str) -> bool {
    validate_id("instance", name).is_ok()
}

/// Fail when [`INSTANCE_ENV`] names something that cannot be an instance.
/// Every shipped binary calls this during startup so a typo fails closed
/// instead of silently using the default directories.
pub fn validate_instance_env() -> Result<()> {
    if let Some(name) = instance_name() {
        validate_id("instance", &name)?;
    }
    Ok(())
}

/// Record a `--instance` flag value for this process and the daemon and
/// workers it spawns. The explicit flag wins over [`INSTANCE_ENV`].
pub fn apply_instance_flag(value: Option<&str>) -> Result<()> {
    if let Some(raw) = value {
        let name = raw.trim();
        validate_id("instance", name)?;
        // SAFETY: every caller runs this during single-threaded process startup,
        // before the Tokio runtime or any other thread exists, so no other
        // thread can observe the environment while it is being mutated.
        unsafe {
            std::env::set_var(INSTANCE_ENV, name);
        }
    }
    validate_instance_env()
}

/// Nest `base` under [`INSTANCE_DIR`] when an instance is selected. A name that
/// fails validation falls back to `base`; startup validation rejects it first,
/// so this only guards against future callers that skip that check.
pub(super) fn with_instance_dir(base: PathBuf, instance: Option<&str>) -> PathBuf {
    match instance {
        Some(name) if is_valid_instance_name(name) => base.join(INSTANCE_DIR).join(name),
        _ => base,
    }
}

pub fn config_dir() -> PathBuf {
    if let Some(path) = env_override_os("CONFIG_DIR") {
        return PathBuf::from(path);
    }
    with_instance_dir(
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from(".config"))
            .join(PRODUCT_DIR),
        instance_name().as_deref(),
    )
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn data_dir() -> PathBuf {
    if let Some(path) = env_override_os("DATA_DIR") {
        return PathBuf::from(path);
    }
    with_instance_dir(
        dirs::data_local_dir()
            .or_else(dirs::data_dir)
            .unwrap_or_else(|| PathBuf::from(".local/share"))
            .join(PRODUCT_DIR),
        instance_name().as_deref(),
    )
}

/// Identity stamped on every worker this Mjolnir instance creates, so a
/// recovery scan from another instance can tell the worker is not its own.
/// A named `--instance` uses its name; otherwise the data directory path is
/// fingerprinted so an explicit `MJ_DATA_DIR` override also gets its own
/// identity.
pub fn instance_identity() -> String {
    instance_identity_for(instance_name().as_deref(), &data_dir())
}

/// Pure form of [`instance_identity`] for callers that already resolved the
/// instance name and data directory.
pub fn instance_identity_for(instance: Option<&str>, data_dir: &Path) -> String {
    if let Some(name) = instance
        && is_valid_instance_name(name)
    {
        return name.to_owned();
    }
    use sha2::Digest;
    let digest = sha2::Sha256::digest(data_dir.to_string_lossy().as_bytes());
    crate::hex::lower_hex(digest)[..16].to_owned()
}

pub fn sessions_dir() -> PathBuf {
    data_dir().join("sessions")
}

/// The first available configuration identifier, retaining a readable base.
pub fn unique_config_id<T>(entries: &BTreeMap<String, T>, base: &str) -> String {
    if !entries.contains_key(base) {
        return base.to_owned();
    }
    for number in 2.. {
        let suffix = format!("-{number}");
        // Configuration identifiers are ASCII and limited to 64 bytes.
        let prefix = base.chars().take(64 - suffix.len()).collect::<String>();
        let candidate = format!("{prefix}{suffix}");
        if !entries.contains_key(&candidate) {
            return candidate;
        }
    }
    unreachable!("configuration identifier space exhausted")
}

pub fn validate_id(kind: &str, id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 64
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        || matches!(id, "." | "..")
    {
        bail!("invalid {kind} id {id:?}; use 1-64 ASCII letters, digits, '.', '-' or '_'");
    }
    Ok(())
}

pub fn validate_relative_destination(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        bail!("destination must be a non-empty relative path");
    }
    for component in path.components() {
        match component {
            Component::Normal(_) => {}
            Component::CurDir => bail!("destination must not contain '.'"),
            Component::ParentDir => bail!("destination must not contain '..'"),
            Component::Prefix(_) | Component::RootDir => {
                bail!("destination must not be absolute")
            }
        }
    }
    Ok(())
}

pub(super) fn is_github_source(source: &str) -> bool {
    let source = source.trim();
    if source.is_empty() || source.starts_with('-') || source.chars().any(char::is_whitespace) {
        return false;
    }
    let repository_path = source
        .strip_prefix("https://github.com/")
        .or_else(|| source.strip_prefix("git@github.com:"))
        .or_else(|| source.strip_prefix("ssh://git@github.com/"))
        .unwrap_or(source);
    let mut parts = repository_path.trim_end_matches(".git").split('/');
    matches!((parts.next(), parts.next(), parts.next()), (Some(owner), Some(repository), None) if !owner.is_empty() && !repository.is_empty())
}

/// Replace `path` without exposing a partially-written configuration/state file.
pub fn atomic_write(path: &Path, body: &[u8]) -> Result<()> {
    atomic_write_with_parent(path, body, ParentDirectory::Create)
}

/// Replace `path` only while its directory still exists.
///
/// Worker state lives inside a directory that session teardown deletes out
/// from under the running daemon. Recreating it here would resurrect a closed
/// session's relay state, so a vanished parent must be an error instead.
pub fn atomic_write_existing(path: &Path, body: &[u8]) -> Result<()> {
    atomic_write_with_parent(path, body, ParentDirectory::Require)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ParentDirectory {
    Create,
    Require,
}

/// Make a rename or new entry in `path` durable. Directory fsync is only
/// available on Unix; Windows cannot open a directory handle this way.
pub fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .with_context(|| format!("sync directory {}", path.display()))?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn atomic_write_with_parent(
    path: &Path,
    body: &[u8],
    parent_directory: ParentDirectory,
) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    match parent_directory {
        ParentDirectory::Create => {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        ParentDirectory::Require => {
            if !parent.is_dir() {
                bail!("directory {} is missing", parent.display());
            }
        }
    }

    let mut random = [0u8; 8];
    getrandom::fill(&mut random)
        .map_err(|error| anyhow!("generate temporary filename: {error}"))?;
    let suffix = u64::from_le_bytes(random);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("hel");
    let temporary = parent.join(format!(
        ".{file_name}.{}.{suffix:016x}.tmp",
        std::process::id()
    ));

    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .with_context(|| format!("create {}", temporary.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(body)
            .with_context(|| format!("write {}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("sync {}", temporary.display()))?;
        drop(file);
        fs::rename(&temporary, path)
            .with_context(|| format!("replace {} with {}", path.display(), temporary.display()))?;
        sync_directory(parent)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}
