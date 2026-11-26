//! Host-side snapshot archive cache (`host_snapshot_cache`).
//!
//! After a successful suspend, the verified archive is published atomically into
//! host-owned storage (`<cache_dir>/<sha256>.hel.zip`) that outlives the
//! session's environment. Resume on the same host verifies the cached file's
//! SHA-256 and stages it locally instead of uploading from the daemon. Destroy
//! removes all copies belonging to the session (including superseded copies),
//! and a 48-hour expiry sweep removes overdue copies.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use anyhow::{Context, Result, bail, ensure};
use chrono::Utc;
use mj_checkpoint::archive::verify_archive_streaming;
use mj_checkpoint::checkpoint::checkpoint_sha256;
use mj_core::targets::{CommandExecutor, CommandSpec, SshTarget, SshTrafficClass, TargetLocator};

use crate::database::{
    delete_host_snapshot_cache_entry, list_host_snapshot_cache_entries_for_session,
    lookup_host_snapshot_cache_entries, record_host_snapshot_cache_entry,
};
use crate::session_manager::new_command_id;

use super::staging::upload_checkpoint_spec;

/// Prefix for temporary files written before atomic rename into the host
/// snapshot cache directory.
pub(super) const SNAPSHOT_CACHE_TEMP_PREFIX: &str = ".tmp-";

/// Prefix of the placeholder `sha256` that records a retained suspend staging
/// file on an SSH host. It is not 64 hex digits, so no lookup by a real digest
/// returns it, while Destroy and the expiry sweep still find and remove it.
const HOST_STAGING_SHA256_PREFIX: &str = "staging:";

/// Portable remote SHA-256: reads the file named by `$p` and leaves its
/// lowercase digest in `$actual`, exiting 45 when neither `sha256sum` nor
/// `shasum` exists. Shared by cache verification and suspend promotion so the
/// two never disagree on how a host hashes a file.
const PORTABLE_SHA256_SNIPPET: &str = "\
if command -v sha256sum >/dev/null 2>&1; then \
    actual=$(sha256sum -- \"$p\" | awk '{print $1}'); \
elif command -v shasum >/dev/null 2>&1; then \
    actual=$(shasum -a 256 -- \"$p\" | awk '{print $1}'); \
else \
    exit 45; \
fi; \
actual=$(printf '%s' \"$actual\" | tr 'A-F' 'a-f'); ";

/// Normalized host identity for `host_snapshot_cache.host`.
///
/// Local targets share `"local"`; SSH targets serialize their `SshTarget`
/// identity so two targets on the same SSH host share the same cache key and
/// cleanup/expiry can reach the host without needing the target template to
/// still exist in config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HostCacheTarget {
    Local,
    Ssh(SshTarget),
}

/// Process-wide wakeup notifier so the daemon's 48-hour expiry timer re-checks
/// SQLite whenever a new `host_snapshot_cache` row is published.
static CACHE_WAKEUP: OnceLock<Arc<tokio::sync::Notify>> = OnceLock::new();

pub fn host_snapshot_cache_wakeup() -> Arc<tokio::sync::Notify> {
    CACHE_WAKEUP
        .get_or_init(|| Arc::new(tokio::sync::Notify::new()))
        .clone()
}

pub(crate) fn notify_host_snapshot_cache_updated() {
    if let Some(notify) = CACHE_WAKEUP.get() {
        notify.notify_one();
    }
}

/// Compute the normalized `host_snapshot_cache.host` key for `locator`.
///
/// Matches `targets_share_host` in `move_session/transport.rs`: all local
/// targets map to `"local"` and SSH targets map by their `SshTarget`.
pub fn host_cache_key(locator: &TargetLocator) -> String {
    match locator {
        TargetLocator::LocalBare { .. }
        | TargetLocator::LocalPodman { .. }
        | TargetLocator::LocalDocker { .. }
        | TargetLocator::AppleContainer { .. } => "local".to_owned(),
        TargetLocator::AwsEc2 { ssh, .. }
        | TargetLocator::SshBare { ssh, .. }
        | TargetLocator::SshPodman { ssh, .. }
        | TargetLocator::SshDocker { ssh, .. }
        | TargetLocator::SshAppleContainer { ssh, .. }
        | TargetLocator::SshNitro { ssh, .. } => {
            format!(
                "ssh:{}",
                serde_json::to_string(ssh).expect("serialize SshTarget for host_snapshot_cache")
            )
        }
    }
}

pub(crate) fn parse_host_cache_key(host: &str) -> Result<HostCacheTarget> {
    if host == "local" {
        return Ok(HostCacheTarget::Local);
    }
    if let Some(json) = host.strip_prefix("ssh:") {
        let ssh: SshTarget = serde_json::from_str(json)
            .with_context(|| format!("parse SSH host_snapshot_cache key {host:?}"))?;
        return Ok(HostCacheTarget::Ssh(ssh));
    }
    bail!("unknown host_snapshot_cache host key {host:?}");
}

/// Local host-owned directory for cached snapshot archives.
///
/// Lives under the data directory so isolated `--instance` daemons
/// and unit tests never share or clobber each other's snapshot cache.
pub fn local_snapshot_cache_dir() -> PathBuf {
    mj_core::config::DataLayout::current().snapshot_cache_dir()
}

fn cache_filename(sha256: &str) -> String {
    format!("{sha256}.hel.zip")
}

#[cfg(unix)]
fn create_private_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::create_dir_all(path)
        .with_context(|| format!("create private snapshot cache directory {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("set 0700 permissions on {}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn create_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)
        .with_context(|| format!("create private snapshot cache directory {}", path.display()))?;
    Ok(())
}

#[cfg(unix)]
fn create_private_file(path: &Path) -> Result<File> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("create private temp snapshot cache file {}", path.display()))?;
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .with_context(|| format!("set 0600 permissions on {}", path.display()))?;
    Ok(file)
}

#[cfg(not(unix))]
fn create_private_file(path: &Path) -> Result<File> {
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .with_context(|| format!("create private temp snapshot cache file {}", path.display()))
}

/// Best-effort atomic publish of a verified suspend archive from the daemon
/// into the local host's snapshot cache. An SSH host already holds the archive
/// and uses [`promote_host_snapshot_cache`] instead.
///
/// Records the `host_snapshot_cache` row BEFORE writing the file, writes to a
/// temporary file in the same directory, and renames to `<sha256>.hel.zip`.
/// The daemon archive is the durable copy. Resume verifies the cache by hash
/// and can upload the daemon archive on a miss, so suspend does not wait for
/// an additional filesystem sync on the cache host. Errors are logged at WARN.
pub fn publish_host_snapshot_cache_best_effort(
    session_id: &str,
    locator: &TargetLocator,
    verified_archive: &Path,
    sha256: &str,
) {
    if let Err(error) = publish_host_snapshot_cache(session_id, locator, verified_archive, sha256) {
        tracing::warn!(
            session_id,
            sha256,
            host = %host_cache_key(locator),
            error = %format!("{error:#}"),
            "could not populate host snapshot cache after suspend"
        );
    }
}

/// Atomically publish `verified_archive` into the local host snapshot cache.
///
/// Each row gets its own exclusive `<copy_id>/<sha256>.hel.zip` path so
/// expiring an older row or destroying one session can never delete a file
/// belonging to a newer suspend or another session.
pub fn publish_host_snapshot_cache(
    session_id: &str,
    locator: &TargetLocator,
    verified_archive: &Path,
    sha256: &str,
) -> Result<PathBuf> {
    let host_key = host_cache_key(locator);
    let written_at = Utc::now().to_rfc3339();
    let copy_id = new_command_id("copy")?;
    match parse_host_cache_key(&host_key)? {
        HostCacheTarget::Local => {
            let root = local_snapshot_cache_dir();
            let copy_dir = root.join(&copy_id);
            let final_path = copy_dir.join(cache_filename(sha256));
            let final_path_str = final_path.to_string_lossy().into_owned();
            // Record the DB row BEFORE writing the file.
            let row_id = record_host_snapshot_cache_entry(
                session_id,
                &host_key,
                sha256,
                &final_path_str,
                &written_at,
            )?;
            let publish = (|| -> Result<()> {
                create_private_dir(&root)?;
                create_private_dir(&copy_dir)?;
                let nonce = new_command_id("snap")?;
                let temp_path =
                    copy_dir.join(format!("{SNAPSHOT_CACHE_TEMP_PREFIX}{sha256}-{nonce}"));
                let write_result = (|| -> Result<()> {
                    let mut source = File::open(verified_archive).with_context(|| {
                        format!(
                            "open verified archive {} for host cache",
                            verified_archive.display()
                        )
                    })?;
                    let mut dest = create_private_file(&temp_path)?;
                    std::io::copy(&mut source, &mut dest).with_context(|| {
                        format!("copy archive into temp file {}", temp_path.display())
                    })?;
                    drop(dest);
                    fs::rename(&temp_path, &final_path).with_context(|| {
                        format!(
                            "atomically publish snapshot cache file {}",
                            final_path.display()
                        )
                    })?;
                    Ok(())
                })();
                if write_result.is_err() {
                    let _ = fs::remove_file(&temp_path);
                    let _ = fs::remove_dir(&copy_dir);
                }
                write_result
            })();
            if let Err(error) = publish {
                if !final_path.exists() {
                    let _ = delete_host_snapshot_cache_entry(row_id);
                }
                return Err(error);
            }
            notify_host_snapshot_cache_updated();
            Ok(final_path)
        }
        HostCacheTarget::Ssh(_) => bail!(
            "an SSH host promotes the archive it already holds with \
             promote_host_snapshot_cache; uploading it again is not supported"
        ),
    }
}

/// Record, before it is created, the retained host staging file of a suspend
/// on an SSH container host, so Destroy and the expiry sweep remove it if the
/// daemon stops before the suspend releases or promotes it. The row carries a
/// placeholder digest that no lookup by a real digest matches.
pub fn record_host_staging_entry(
    session_id: &str,
    locator: &TargetLocator,
    operation_id: &str,
    staging_path: &str,
) -> Result<i64> {
    let row_id = record_host_snapshot_cache_entry(
        session_id,
        &host_cache_key(locator),
        &format!("{HOST_STAGING_SHA256_PREFIX}{operation_id}"),
        staging_path,
        &Utc::now().to_rfc3339(),
    )?;
    // Wake the expiry task now: this suspend can be cancelled before it
    // promotes or releases the file, and the task may be sleeping on an
    // empty table.
    notify_host_snapshot_cache_updated();
    Ok(row_id)
}

/// Forget a retained staging row whose file is gone or became a cache copy.
pub fn retire_host_staging_entry(row_id: i64) {
    if let Err(error) = delete_host_snapshot_cache_entry(row_id) {
        tracing::warn!(
            row_id,
            error = %format!("{error:#}"),
            "could not delete retained host staging row"
        );
    }
}

/// Remove a retained host staging file and retire its row once the file is
/// confirmed gone. If the host cannot be reached the row stays, and the
/// expiry sweep is woken so it reclaims the file.
pub fn release_host_staging(
    locator: &TargetLocator,
    staging_path: &str,
    staging_row_id: i64,
    executor: &impl CommandExecutor,
) {
    match crate::checkpoint_transfer::cleanup_host_staging(locator, staging_path, executor) {
        Ok(()) => retire_host_staging_entry(staging_row_id),
        Err(error) => {
            tracing::warn!(
                row_id = staging_row_id,
                staging_path,
                error = %format!("{error:#}"),
                "could not remove retained host staging; keeping its row for Destroy and expiry"
            );
            notify_host_snapshot_cache_updated();
        }
    }
}

/// A verified archive that already sits on the cache host.
pub struct HostCachePromotion<'a> {
    pub session_id: &'a str,
    pub locator: &'a TargetLocator,
    /// The archive's path on the SSH host.
    pub source_host_path: &'a str,
    /// Remove the source once it is cached (a retained staging copy), rather
    /// than leave it for its owner (a bare target's checkpoint archive).
    pub remove_source: bool,
    /// The host's `$HOME` if the transfer already learned it.
    pub known_remote_home: Option<&'a str>,
    pub sha256: &'a str,
    /// The durable row that owns `source_host_path` when `remove_source`.
    pub staging_row_id: Option<i64>,
}

/// [`promote_host_snapshot_cache`] for a suspend: the archive is already
/// verified and durable on the daemon, so a failure only costs the resume a
/// daemon upload. Errors are logged at WARN.
pub fn promote_host_snapshot_cache_best_effort(
    promotion: &HostCachePromotion<'_>,
    executor: &impl CommandExecutor,
) {
    if let Err(error) = promote_host_snapshot_cache(promotion, executor) {
        tracing::warn!(
            session_id = promotion.session_id,
            sha256 = promotion.sha256,
            host = %host_cache_key(promotion.locator),
            error = %format!("{error:#}"),
            "could not promote host archive into snapshot cache after suspend"
        );
    }
}

/// Install an archive that already sits on the cache host into its snapshot
/// cache, in one SSH command and without sending any archive bytes: the host
/// re-hashes the file, stages it inside the cache copy directory (a hard link,
/// or a copy when the staging and cache directories are on different
/// filesystems), and renames it into `<copy_id>/<sha256>.hel.zip`.
///
/// A failure is reported, never answered by uploading the archive again. When
/// `remove_source` is set, the staging file is removed on every outcome.
pub fn promote_host_snapshot_cache(
    promotion: &HostCachePromotion<'_>,
    executor: &impl CommandExecutor,
) -> Result<PathBuf> {
    let result = promote_into_host_cache(promotion, executor);
    match (&result, promotion.remove_source, promotion.staging_row_id) {
        (Ok(_), _, Some(row_id)) => {
            // The script moved the file into the cache and removed the source.
            retire_host_staging_entry(row_id);
        }
        (Err(_), true, Some(row_id)) => {
            release_host_staging(
                promotion.locator,
                promotion.source_host_path,
                row_id,
                executor,
            );
        }
        (Err(_), true, None) => {
            if let Err(error) = crate::checkpoint_transfer::cleanup_host_staging(
                promotion.locator,
                promotion.source_host_path,
                executor,
            ) {
                tracing::warn!(
                    path = promotion.source_host_path,
                    error = %format!("{error:#}"),
                    "could not remove host archive after failed snapshot cache promotion"
                );
            }
        }
        _ => {}
    }
    result
}

fn promote_into_host_cache(
    promotion: &HostCachePromotion<'_>,
    executor: &impl CommandExecutor,
) -> Result<PathBuf> {
    let host_key = host_cache_key(promotion.locator);
    let HostCacheTarget::Ssh(ssh) = parse_host_cache_key(&host_key)? else {
        bail!("only an SSH host can promote an archive it already holds into its snapshot cache");
    };
    let sha256 = promotion.sha256;
    let remote_home = match promotion
        .known_remote_home
        .map(str::trim)
        .filter(|home| home.starts_with('/'))
    {
        Some(home) => home.to_owned(),
        None => resolve_ssh_home(&ssh, executor)?,
    };
    let copy_id = new_command_id("copy")?;
    let remote_root = format!("{remote_home}/.cache/mjolnir/snapshot-cache");
    let remote_copy_dir = format!("{remote_root}/{copy_id}");
    let final_path = format!("{remote_copy_dir}/{}", cache_filename(sha256));
    let nonce = new_command_id("snap")?;
    let temp_path = format!("{remote_copy_dir}/{SNAPSHOT_CACHE_TEMP_PREFIX}{sha256}-{nonce}");
    let row_id = record_host_snapshot_cache_entry(
        promotion.session_id,
        &host_key,
        sha256,
        &final_path,
        &Utc::now().to_rfc3339(),
    )?;
    // The temporary file is created inside the copy directory so the final
    // rename is atomic whatever filesystem the source is on.
    let script = format!(
        "set -eu; umask 077; root=$1; dir=$2; tmp=$3; final=$4; source=$5; remove_source=$6; \
         expected=$7; [ -f \"$source\" ] || exit 44; p=$source; {PORTABLE_SHA256_SNIPPET}\
         [ \"$actual\" = \"$expected\" ] || exit 46; \
         trap 'rm -f -- \"$tmp\"; rmdir -- \"$dir\" 2>/dev/null || true' EXIT; \
         trap 'exit 1' HUP INT TERM; \
         mkdir -p -- \"$root\" \"$dir\"; chmod 0700 \"$root\" \"$dir\"; \
         if ! ln -- \"$source\" \"$tmp\" 2>/dev/null; then cp -- \"$source\" \"$tmp\"; fi; \
         chmod 0600 \"$tmp\"; \
         mv -f -- \"$tmp\" \"$final\"; \
         trap - EXIT; \
         if [ \"$remove_source\" = 1 ]; then rm -f -- \"$source\"; fi"
    );
    let command = mj_core::targets::ssh_command_owned(
        SshTrafficClass::Control,
        &ssh,
        vec![
            "sh".to_owned(),
            "-c".to_owned(),
            script,
            "hel-snapshot-cache-promote".to_owned(),
            remote_root,
            remote_copy_dir,
            temp_path,
            final_path.clone(),
            promotion.source_host_path.to_owned(),
            if promotion.remove_source { "1" } else { "0" }.to_owned(),
            sha256.to_ascii_lowercase(),
        ],
    )
    .purpose("promote host archive into snapshot cache");
    let (failure, nothing_written) = match executor.execute(&command) {
        Ok(out) if out.status == 0 => {
            notify_host_snapshot_cache_updated();
            return Ok(PathBuf::from(final_path));
        }
        Ok(out) => {
            // The script exits 44, 45 and 46 before it creates anything.
            // Any other status may come after `mv` or `rm`, and 255 is a
            // dropped SSH session, so the file's fate is unknown.
            let reason = match out.status {
                44 => "the host archive is missing".to_owned(),
                45 => "the host has neither sha256sum nor shasum".to_owned(),
                46 => "the host archive's SHA-256 differs from the verified archive".to_owned(),
                _ => String::from_utf8_lossy(&out.stderr).trim().to_owned(),
            };
            (
                anyhow::anyhow!(
                    "host snapshot cache promotion failed with status {}: {reason}",
                    out.status
                ),
                matches!(out.status, 44..=46),
            )
        }
        Err(error) => (error, false),
    };
    if nothing_written {
        let _ = delete_host_snapshot_cache_entry(row_id);
    } else {
        // The file may exist on the host: keep the row so Destroy and expiry
        // reclaim it.
        notify_host_snapshot_cache_updated();
    }
    Err(failure)
}

/// Check `host_snapshot_cache` for `(host_cache_key(locator), expected_sha256)`.
///
/// If a cached copy exists on the destination host and its SHA-256 matches
/// `expected_sha256`, stage it into `remote_archive` without uploading from the
/// daemon and return `Ok(true)`.
///
/// If a cached entry is missing or its SHA-256 mismatches, delete the stale or
/// corrupted file on the host (and remove its DB row once confirmed gone) and
/// fall back to returning `Ok(false)` so the caller uploads from the daemon.
pub fn try_stage_host_snapshot_cache(
    executor: &impl CommandExecutor,
    locator: &TargetLocator,
    session_id: &str,
    expected_sha256: &str,
    remote_archive: &str,
) -> Result<bool> {
    let host_key = host_cache_key(locator);
    let entries = lookup_host_snapshot_cache_entries(&host_key, expected_sha256)?;
    if entries.is_empty() {
        return Ok(false);
    }
    let host_target = parse_host_cache_key(&host_key)?;
    for entry in entries {
        match verify_host_cache_entry(&host_target, &entry.path, expected_sha256, executor) {
            Ok(HostCacheVerification::Valid) => {
                if let Err(error) = stage_verified_host_cache_to_target(
                    executor,
                    locator,
                    session_id,
                    &entry.path,
                    remote_archive,
                ) {
                    tracing::warn!(
                        session_id,
                        sha256 = expected_sha256,
                        path = %entry.path,
                        error = %format!("{error:#}"),
                        "could not stage verified host snapshot cache archive; falling back to daemon upload"
                    );
                    continue;
                }
                tracing::info!(
                    session_id,
                    sha256 = expected_sha256,
                    host = %host_key,
                    "reused verified host snapshot cache archive on resume"
                );
                return Ok(true);
            }
            Ok(HostCacheVerification::Mismatch) => {
                tracing::warn!(
                    session_id,
                    sha256 = expected_sha256,
                    path = %entry.path,
                    "host snapshot cache checksum mismatch; removing corrupted entry"
                );
                if remove_host_cache_file(&host_target, &entry.path, executor).is_ok() {
                    let _ = delete_host_snapshot_cache_entry(entry.id);
                }
            }
            Ok(HostCacheVerification::Missing) => {
                // Leave missing entries for expiry/destroy cleanup rather than
                // deleting rows here: a concurrent publisher writes the row
                // before renaming the final archive into place.
                tracing::debug!(
                    session_id,
                    sha256 = expected_sha256,
                    path = %entry.path,
                    "host snapshot cache file not present at final path; skipping"
                );
            }
            Err(error) => {
                tracing::warn!(
                    session_id,
                    sha256 = expected_sha256,
                    path = %entry.path,
                    error = %format!("{error:#}"),
                    "could not verify host snapshot cache archive; falling back to daemon upload"
                );
            }
        }
    }
    Ok(false)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostCacheVerification {
    Valid,
    Mismatch,
    Missing,
}

fn verify_host_cache_entry(
    host: &HostCacheTarget,
    path: &str,
    expected_sha256: &str,
    executor: &impl CommandExecutor,
) -> Result<HostCacheVerification> {
    match host {
        HostCacheTarget::Local => {
            let file_path = Path::new(path);
            if !file_path.exists() {
                return Ok(HostCacheVerification::Missing);
            }
            match checkpoint_sha256(file_path) {
                Ok(actual) if actual.eq_ignore_ascii_case(expected_sha256) => {
                    Ok(HostCacheVerification::Valid)
                }
                Ok(_) => Ok(HostCacheVerification::Mismatch),
                Err(error) => {
                    if !file_path.exists() {
                        Ok(HostCacheVerification::Missing)
                    } else {
                        Err(error)
                    }
                }
            }
        }
        HostCacheTarget::Ssh(ssh) => {
            let script = format!(
                "set -eu; p=$1; [ -f \"$p\" ] || exit 44; {PORTABLE_SHA256_SNIPPET}printf '%s\\n' \"$actual\""
            );
            let command = mj_core::targets::ssh_command_owned(
                SshTrafficClass::Control,
                ssh,
                vec![
                    "sh".to_owned(),
                    "-c".to_owned(),
                    script,
                    "hel-snapshot-cache-verify".to_owned(),
                    path.to_owned(),
                ],
            )
            .purpose("verify host snapshot cache archive checksum");
            let output = executor.execute(&command)?;
            if output.status == 44 {
                return Ok(HostCacheVerification::Missing);
            }
            ensure!(
                output.status == 0,
                "remote checksum verification failed with status {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
            let actual = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            if actual.eq_ignore_ascii_case(expected_sha256) {
                Ok(HostCacheVerification::Valid)
            } else {
                Ok(HostCacheVerification::Mismatch)
            }
        }
    }
}

fn stage_verified_host_cache_to_target(
    executor: &impl CommandExecutor,
    locator: &TargetLocator,
    session_id: &str,
    cached_path: &str,
    remote_archive: &str,
) -> Result<()> {
    match locator {
        TargetLocator::LocalBare { .. } => {
            // Local bare targets read the archive directly from the host path.
            Ok(())
        }
        TargetLocator::LocalPodman { .. }
        | TargetLocator::LocalDocker { .. }
        | TargetLocator::AppleContainer { .. } => upload_checkpoint_spec(
            executor,
            locator,
            session_id,
            Path::new(cached_path),
            remote_archive,
        ),
        TargetLocator::SshBare { ssh, .. } | TargetLocator::AwsEc2 { ssh, .. } => {
            let script = "set -eu; src=$1; dst=$2; mkdir -p -- \"$(dirname -- \"$dst\")\"; cp -f -- \"$src\" \"$dst\"";
            let command = mj_core::targets::ssh_command_owned(
                SshTrafficClass::Control,
                ssh,
                vec![
                    "sh".to_owned(),
                    "-c".to_owned(),
                    script.to_owned(),
                    "hel-snapshot-cache-stage".to_owned(),
                    cached_path.to_owned(),
                    remote_archive.to_owned(),
                ],
            )
            .purpose("stage host snapshot cache archive on SSH bare target");
            let output = executor.execute(&command)?;
            ensure!(
                output.status == 0,
                "stage host snapshot cache on SSH bare failed with status {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
            Ok(())
        }
        TargetLocator::SshPodman {
            ssh, container_id, ..
        } => stage_ssh_container_cache(
            executor,
            ssh,
            "podman",
            container_id,
            cached_path,
            remote_archive,
        ),
        TargetLocator::SshDocker {
            ssh, container_id, ..
        } => stage_ssh_container_cache(
            executor,
            ssh,
            "docker",
            container_id,
            cached_path,
            remote_archive,
        ),
        TargetLocator::SshAppleContainer {
            ssh, container_id, ..
        } => stage_ssh_container_cache(
            executor,
            ssh,
            "container",
            container_id,
            cached_path,
            remote_archive,
        ),
        TargetLocator::SshNitro { .. } => {
            bail!("SshNitro does not support direct host-to-container cp staging");
        }
    }
}

fn stage_ssh_container_cache(
    executor: &impl CommandExecutor,
    ssh: &SshTarget,
    engine: &str,
    container: &str,
    cached_path: &str,
    remote_archive: &str,
) -> Result<()> {
    let destination = format!("{container}:{remote_archive}");
    let command = mj_core::targets::ssh_command_owned(
        SshTrafficClass::Bulk,
        ssh,
        vec![
            engine.to_owned(),
            "cp".to_owned(),
            cached_path.to_owned(),
            destination,
        ],
    )
    .purpose("copy host snapshot cache archive into remote container");
    let output = executor.execute(&command)?;
    ensure!(
        output.status == 0,
        "remote container cp from host snapshot cache failed with status {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

/// What the remote removal script prints first, before it touches any file.
const REMOTE_REMOVE_REACHED_MARKER: &str = "__mj_cache_rm_reached__";

/// OpenSSH's own exit status for a connection that failed.
const SSH_TRANSPORT_FAILURE_STATUS: i32 = 255;

/// A removal that failed on a host that answered: the path, not the host, is
/// the problem. Any other error from [`remove_host_cache_file`] on an SSH host
/// means the host was not reached.
#[derive(Debug)]
pub(crate) struct HostCachePathFailure(String);

impl std::fmt::Display for HostCachePathFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for HostCachePathFailure {}

/// Remove a cached snapshot archive file from `host`.
///
/// Succeeds if the file is removed OR was already absent. Returns `Err` if the
/// host is unreachable or deletion fails so the caller retains the DB row.
pub(crate) fn remove_host_cache_file(
    host: &HostCacheTarget,
    path: &str,
    executor: &impl CommandExecutor,
) -> Result<u64> {
    match host {
        HostCacheTarget::Local => {
            let file_path = Path::new(path);
            let bytes = match fs::symlink_metadata(file_path) {
                Ok(meta) if meta.is_file() => meta.len(),
                Ok(_) => return Ok(0),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("inspect local snapshot cache file {}", file_path.display())
                    });
                }
            };
            let remove_result = match fs::remove_file(file_path) {
                Ok(()) => Ok(bytes),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
                Err(error) => Err(error).with_context(|| {
                    format!("remove local snapshot cache file {}", file_path.display())
                }),
            };
            if remove_result.is_ok()
                && let Some(parent) = file_path.parent()
                && parent != local_snapshot_cache_dir()
            {
                if let Ok(entries) = fs::read_dir(parent) {
                    for entry in entries.flatten() {
                        if entry
                            .file_name()
                            .to_str()
                            .is_some_and(|name| name.starts_with(".tmp-"))
                        {
                            let _ = fs::remove_file(entry.path());
                        }
                    }
                }
                let _ = fs::remove_dir(parent);
            }
            remove_result
        }
        HostCacheTarget::Ssh(ssh) => {
            // Only a per-copy directory of the snapshot cache is ever swept
            // or removed. A retained suspend staging file lives in the shared
            // `.local/share/hel/transfers` directory, which stays.
            let script = format!(
                "set -eu; printf '%s\\n' {REMOTE_REMOVE_REACHED_MARKER}; \
                          p=$1; dir=$(dirname -- \"$p\"); \
                          rm -f -- \"$p\"; \
                          case \"$dir\" in \
                              */.cache/mjolnir/snapshot-cache/*|.cache/mjolnir/snapshot-cache/*) \
                                  for tmp in \"$dir\"/.tmp-*; do \
                                      [ -e \"$tmp\" ] || continue; \
                                      rm -f -- \"$tmp\"; \
                                  done; \
                                  rmdir -- \"$dir\" 2>/dev/null || true ;; \
                          esac"
            );
            let command = mj_core::targets::ssh_command_owned(
                SshTrafficClass::Control,
                ssh,
                vec![
                    "sh".to_owned(),
                    "-c".to_owned(),
                    script,
                    "hel-snapshot-cache-remove".to_owned(),
                    path.to_owned(),
                ],
            )
            .purpose("remove host snapshot cache archive");
            let output = executor.execute(&command)?;
            if output.status == 0 {
                return Ok(0);
            }
            let message = format!(
                "remove remote snapshot cache file failed with status {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
            // The script announces itself before touching anything, so its
            // marker proves the host answered and the failure is this path's.
            let reached = output.status != SSH_TRANSPORT_FAILURE_STATUS
                && String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .any(|line| line == REMOTE_REMOVE_REACHED_MARKER);
            if reached {
                return Err(HostCachePathFailure(message).into());
            }
            bail!("{message}")
        }
    }
}

/// Forget every `host_snapshot_cache` row for the SSH host reached with
/// `identity_file`, without touching the host: its VM is gone, so no file can
/// be removed and a sweep could only retry the dead address. Returns the number
/// of rows deleted. A machine with no store has none.
pub fn retire_host_snapshot_cache_for_ssh_identity(identity_file: &Path) -> Result<usize> {
    if !mj_store::database::database_path().exists() {
        return Ok(0);
    }
    let mut retired = 0;
    for entry in crate::database::list_all_host_snapshot_cache_entries()? {
        let Ok(HostCacheTarget::Ssh(ssh)) = parse_host_cache_key(&entry.host) else {
            continue;
        };
        if mj_core::ec2_records::ssh_identity_files(&ssh)
            .iter()
            .any(|identity| identity == identity_file)
            && delete_host_snapshot_cache_entry(entry.id)?
        {
            retired += 1;
        }
    }
    Ok(retired)
}

/// Remove all `host_snapshot_cache` copies belonging to `session_id` (including
/// copies superseded by earlier suspends).
///
/// Deletes each DB row ONLY after its file is confirmed removed or already
/// absent. If a host is unreachable, retains the DB row and returns a visible
/// warning for `DestroyOutcome.warnings`.
pub fn cleanup_session_host_snapshot_cache(
    session_id: &str,
    executor: &impl CommandExecutor,
) -> Result<Vec<String>> {
    let entries = list_host_snapshot_cache_entries_for_session(session_id)?;
    let mut warnings = Vec::new();
    for entry in entries {
        let host = match parse_host_cache_key(&entry.host) {
            Ok(host) => host,
            Err(error) => {
                warnings.push(format!(
                    "Could not remove host snapshot cache entry {} for session {session_id}: {error:#}",
                    entry.path
                ));
                continue;
            }
        };
        match remove_host_cache_file(&host, &entry.path, executor) {
            Ok(_) => {
                if let Err(error) = delete_host_snapshot_cache_entry(entry.id) {
                    tracing::warn!(
                        session_id,
                        row_id = entry.id,
                        error = %format!("{error:#}"),
                        "could not delete host_snapshot_cache row after removing file"
                    );
                }
            }
            Err(error) => {
                tracing::warn!(
                    session_id,
                    host = %entry.host,
                    path = %entry.path,
                    error = %format!("{error:#}"),
                    "could not remove host snapshot cache archive during Destroy; keeping row for expiry sweep"
                );
                warnings.push(format!(
                    "Cleanup of the host snapshot cache archive {} on host {} failed during Destroy for session {session_id}. Resources may remain; details are in the daemon logs.",
                    entry.path, entry.host
                ));
            }
        }
    }
    Ok(warnings)
}

/// Best-effort load of `OriginDelta` repository packs from `archive_path` into
/// the destination host's Git clone cache (`~/.cache/mjolnir/git/repos/<key>.git`),
/// if one exists for that origin.
///
/// Losing objects to Git GC is harmless; failures are logged and never fail
/// restore.
pub fn load_snapshot_pack_into_clone_cache_best_effort(
    locator: &TargetLocator,
    archive_path: &Path,
    executor: &impl CommandExecutor,
) {
    if let Err(error) = load_snapshot_pack_into_clone_cache(locator, archive_path, executor) {
        tracing::debug!(
            archive = %archive_path.display(),
            host = %host_cache_key(locator),
            error = %format!("{error:#}"),
            "skipped loading OriginDelta pack into host clone cache"
        );
    }
}

pub fn load_snapshot_pack_into_clone_cache(
    locator: &TargetLocator,
    archive_path: &Path,
    executor: &impl CommandExecutor,
) -> Result<usize> {
    let verified = verify_archive_streaming(archive_path).with_context(|| {
        format!(
            "inspect archive {} for clone cache pack load",
            archive_path.display()
        )
    })?;
    let pack_entries: Vec<(String, String)> = verified
        .manifest
        .repositories
        .iter()
        .filter_map(|repo| {
            let pack_path = repo.origin_delta_pack_path.as_ref()?;
            let origin = repo.metadata.origin.as_str();
            if origin.is_empty() {
                return None;
            }
            Some((origin.to_owned(), pack_path.clone()))
        })
        .collect();
    if pack_entries.is_empty() {
        return Ok(0);
    }

    let host_target = parse_host_cache_key(&host_cache_key(locator))?;
    let host_home = match &host_target {
        HostCacheTarget::Local => {
            std::env::var("HOME").context("resolve local HOME for clone cache")?
        }
        HostCacheTarget::Ssh(ssh) => resolve_ssh_home(ssh, executor)?,
    };

    let mut zip: Option<zip::ZipArchive<File>> = None;
    let mut loaded = 0usize;
    for (origin, pack_path) in pack_entries {
        let rel_mirror = crate::controller::git_cache::mirror_repo_relative_path(&origin);
        let cache_repo = format!("{host_home}/{rel_mirror}");
        // Check whether the target clone-cache mirror exists FIRST, before
        // opening or decompressing any pack entry from the archive.
        if !host_dir_exists(&host_target, &cache_repo, executor)? {
            continue;
        }
        if zip.is_none() {
            let archive_file = File::open(archive_path)
                .with_context(|| format!("open archive {}", archive_path.display()))?;
            zip = Some(
                zip::ZipArchive::new(archive_file)
                    .with_context(|| format!("read zip archive {}", archive_path.display()))?,
            );
        }
        let zip_ref = zip.as_mut().expect("zip initialized above");
        let mut entry = match zip_ref.by_name(&pack_path) {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        // Spool the zip pack entry to a temporary file and stream it into
        // `git index-pack --stdin` in O(1) daemon memory.
        let mut spool = tempfile::NamedTempFile::new()
            .context("create temporary spool file for clone-cache pack load")?;
        let copied = std::io::copy(&mut entry, &mut spool)
            .with_context(|| format!("spool OriginDelta pack {pack_path}"))?;
        if copied == 0 {
            continue;
        }
        spool.seek(SeekFrom::Start(0))?;
        if load_pack_reader_into_bare_repo(&host_target, &cache_repo, &mut spool, executor)? {
            loaded += 1;
        }
    }
    Ok(loaded)
}

fn host_dir_exists(
    host: &HostCacheTarget,
    dir: &str,
    executor: &impl CommandExecutor,
) -> Result<bool> {
    match host {
        HostCacheTarget::Local => Ok(Path::new(dir).is_dir()),
        HostCacheTarget::Ssh(ssh) => {
            let command = mj_core::targets::ssh_command_owned(
                SshTrafficClass::Control,
                ssh,
                vec![
                    "sh".to_owned(),
                    "-c".to_owned(),
                    "test -d \"$1\"".to_owned(),
                    "hel-clone-cache-exists".to_owned(),
                    dir.to_owned(),
                ],
            )
            .purpose("check whether remote clone-cache mirror exists");
            let output = executor.execute(&command)?;
            Ok(output.status == 0)
        }
    }
}

/// Stream raw Git packfile bytes from `pack_reader` into an existing bare
/// clone-cache mirror repository via `git -C <cache_repo> index-pack --stdin`.
/// Returns `Ok(true)` if the bare repo existed and accepted the pack,
/// `Ok(false)` if no clone cache existed.
pub(crate) fn load_pack_reader_into_bare_repo(
    host: &HostCacheTarget,
    cache_repo: &str,
    pack_reader: &mut (dyn Read + Send),
    executor: &impl CommandExecutor,
) -> Result<bool> {
    match host {
        HostCacheTarget::Local => {
            if !Path::new(cache_repo).is_dir() {
                return Ok(false);
            }
            let command = CommandSpec::new(
                "git",
                vec![
                    "-C".to_owned(),
                    cache_repo.to_owned(),
                    "index-pack".to_owned(),
                    "--stdin".to_owned(),
                ],
            )
            .purpose("load OriginDelta pack into local clone cache");
            let output = executor.execute_with_stdin(&command, pack_reader)?;
            ensure!(
                output.status == 0,
                "git index-pack --stdin into local clone cache failed with status {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
            Ok(true)
        }
        HostCacheTarget::Ssh(ssh) => {
            let script = "set -eu; repo=$1; [ -d \"$repo\" ] || exit 44; git -C \"$repo\" index-pack --stdin >/dev/null";
            let command = mj_core::targets::ssh_command_owned(
                SshTrafficClass::Bulk,
                ssh,
                vec![
                    "sh".to_owned(),
                    "-c".to_owned(),
                    script.to_owned(),
                    "hel-clone-cache-load-pack".to_owned(),
                    cache_repo.to_owned(),
                ],
            )
            .purpose("load OriginDelta pack into remote clone cache");
            let output = executor.execute_with_stdin(&command, pack_reader)?;
            if output.status == 44 {
                return Ok(false);
            }
            ensure!(
                output.status == 0,
                "git index-pack --stdin into remote clone cache failed with status {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
            Ok(true)
        }
    }
}

fn resolve_ssh_home(ssh: &SshTarget, executor: &impl CommandExecutor) -> Result<String> {
    let command = mj_core::targets::ssh_command_owned(
        SshTrafficClass::Control,
        ssh,
        vec![
            "sh".to_owned(),
            "-c".to_owned(),
            "printf %s \"$HOME\"".to_owned(),
            "hel-snapshot-cache-home".to_owned(),
        ],
    )
    .purpose("resolve remote host HOME for snapshot cache");
    let output = executor.execute(&command)?;
    ensure!(
        output.status == 0,
        "resolve remote HOME failed with status {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let home = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    ensure!(!home.is_empty(), "remote host HOME is empty");
    Ok(home)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::test_support::IsolatedTest;
    use crate::targets::{CommandOutput, ProcessExecutor};
    use std::path::PathBuf;

    struct FailingSshExecutor;

    impl CommandExecutor for FailingSshExecutor {
        fn pause_deadline(&self) -> mj_core::targets::DeadlinePause {
            mj_core::targets::DeadlinePause::none()
        }

        fn cancellation_requested_without_deadline(&self) -> bool {
            false
        }

        fn execute(&self, _spec: &CommandSpec) -> Result<CommandOutput> {
            bail!("ssh: connect to host unreachable.example port 22: Connection refused");
        }

        fn execute_without_deadline(&self, _spec: &CommandSpec) -> Result<CommandOutput> {
            bail!("ssh: connect to host unreachable.example port 22: Connection refused");
        }

        fn execute_with_stdin_without_deadline(
            &self,
            _spec: &CommandSpec,
            _stdin: &mut (dyn Read + Send),
        ) -> Result<CommandOutput> {
            bail!("ssh: connect to host unreachable.example port 22: Connection refused");
        }
    }

    /// Runs each `ssh` command as a local shell in a fake remote `$HOME`, the
    /// way the SSH server would, and records what crossed the "wire".
    #[cfg(unix)]
    struct LocalSsh {
        home: PathBuf,
        /// Purposes of commands that fail as an unreachable host would.
        unreachable: Vec<&'static str>,
        purposes: std::sync::Mutex<Vec<String>>,
        stdin_calls: std::sync::atomic::AtomicUsize,
    }

    #[cfg(unix)]
    impl LocalSsh {
        fn new(home: &Path) -> Self {
            Self {
                home: home.to_path_buf(),
                unreachable: Vec::new(),
                purposes: Default::default(),
                stdin_calls: Default::default(),
            }
        }

        fn count(&self, purpose: &str) -> usize {
            self.purposes
                .lock()
                .unwrap()
                .iter()
                .filter(|seen| seen.as_str() == purpose)
                .count()
        }

        fn shell_command(&self, command: &CommandSpec) -> CommandSpec {
            assert_eq!(command.program, "ssh");
            let line = command.args.last().expect("missing remote command");
            // A login starts in `$HOME`; the shims directory comes first on PATH.
            let mut shell = CommandSpec::new("sh", ["-c", &format!("cd \"$HOME\" && {line}")]);
            shell
                .env
                .insert("HOME".into(), self.home.display().to_string());
            shell.env.insert(
                "PATH".into(),
                format!("{}:/usr/bin:/bin", self.home.join("shims").display()),
            );
            shell
        }
    }

    #[cfg(unix)]
    impl CommandExecutor for LocalSsh {
        fn pause_deadline(&self) -> mj_core::targets::DeadlinePause {
            mj_core::targets::DeadlinePause::none()
        }

        fn cancellation_requested_without_deadline(&self) -> bool {
            false
        }

        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.purposes.lock().unwrap().push(command.purpose.clone());
            if self.unreachable.contains(&command.purpose.as_str()) {
                bail!("ssh: connect to host fixture port 22: Connection refused");
            }
            ProcessExecutor.execute(&self.shell_command(command))
        }

        fn execute_without_deadline(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.execute(command)
        }

        fn execute_with_stdin(
            &self,
            command: &CommandSpec,
            input: &mut (dyn Read + Send),
        ) -> Result<CommandOutput> {
            self.stdin_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            ProcessExecutor.execute_with_stdin(&self.shell_command(command), input)
        }

        fn execute_with_stdin_without_deadline(
            &self,
            command: &CommandSpec,
            input: &mut (dyn Read + Send),
        ) -> Result<CommandOutput> {
            self.execute_with_stdin(command, input)
        }
    }

    #[cfg(unix)]
    fn fixture_ssh() -> SshTarget {
        SshTarget {
            destination: "fixture".into(),
            ssh_args: Vec::new(),
        }
    }

    #[cfg(unix)]
    fn container_locator() -> TargetLocator {
        TargetLocator::SshPodman {
            borrowed_from: None,
            ssh: fixture_ssh(),
            container_id: "session-container".into(),
            workspace_storage: Default::default(),
        }
    }

    /// Write a >64 KiB staged archive where a suspend leaves it on the host.
    #[cfg(unix)]
    fn stage_on_host(home: &Path, session: &str, operation: &str) -> (String, Vec<u8>, String) {
        let relative = format!(".local/share/hel/transfers/{session}-{operation}.hel.zip");
        let bytes: Vec<u8> = (0..160 * 1024).map(|index| (index % 251) as u8).collect();
        let path = home.join(&relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, &bytes).unwrap();
        let sha = checkpoint_sha256(&path).unwrap();
        (relative, bytes, sha)
    }

    #[cfg(unix)]
    fn write_shim(home: &Path, name: &str, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        let directory = home.join("shims");
        fs::create_dir_all(&directory).unwrap();
        let shim = directory.join(name);
        fs::write(&shim, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&shim, fs::Permissions::from_mode(0o700)).unwrap();
    }

    /// Re-run `test` alone in a child with its own store; true in the child.
    fn in_isolated_store(test: &str, marker: &str) -> bool {
        if std::env::var_os(marker).is_some() {
            return true;
        }
        let directory = tempfile::tempdir().unwrap();
        IsolatedTest::new(format!("controller::checkpoint::host_cache::tests::{test}"))
            .env(marker, "1")
            .isolated_store(directory.path())
            .run();
        false
    }

    /// Destroying an EC2 host forgets exactly the cache rows reached with its
    /// key, whichever way the key is spelled, and never creates a store.
    #[test]
    fn retiring_a_destroyed_hosts_cache_rows_leaves_every_other_host() {
        if !in_isolated_store(
            "retiring_a_destroyed_hosts_cache_rows_leaves_every_other_host",
            "MJ_HOST_CACHE_RETIRE_CHILD",
        ) {
            return;
        }
        let destroyed = Path::new("/records/aaaa/id_ed25519");
        assert!(!mj_store::database::database_path().exists());
        assert_eq!(
            retire_host_snapshot_cache_for_ssh_identity(destroyed).unwrap(),
            0
        );
        assert!(
            !mj_store::database::database_path().exists(),
            "retiring rows on a machine with no store must not create one"
        );

        let key = |destination: &str, ssh_args: &[&str]| {
            host_cache_key(&TargetLocator::SshBare {
                ssh: SshTarget {
                    destination: destination.into(),
                    ssh_args: ssh_args.iter().map(ToString::to_string).collect(),
                },
                workspace: "/work".into(),
                worker_id: None,
            })
        };
        let at = Utc::now().to_rfc3339();
        for (session, host) in [
            (
                "sess-spaced",
                key("a@1.example", &["-i", "/records/aaaa/id_ed25519"]),
            ),
            (
                "sess-glued",
                key("a@1.example", &["-p", "22", "-i/records/aaaa/id_ed25519"]),
            ),
            (
                "sess-other-key",
                key("a@1.example", &["-i", "/records/bbbb/id_ed25519"]),
            ),
            (
                "sess-same-name",
                key("a@1.example", &["-i", "/records/aaaa/id_ed25519.pub"]),
            ),
            ("sess-no-key", key("a@1.example", &[])),
            ("sess-local", "local".to_owned()),
        ] {
            record_host_snapshot_cache_entry(session, &host, "sha", "/cache/archive.hel.zip", &at)
                .unwrap();
        }

        assert_eq!(
            retire_host_snapshot_cache_for_ssh_identity(destroyed).unwrap(),
            2
        );
        let remaining: Vec<String> = crate::database::list_all_host_snapshot_cache_entries()
            .unwrap()
            .into_iter()
            .map(|entry| entry.session_id)
            .collect();
        assert_eq!(
            remaining,
            [
                "sess-other-key",
                "sess-same-name",
                "sess-no-key",
                "sess-local"
            ]
        );
    }

    #[cfg(unix)]
    // Hard-won: ddd712e31: syncing a disposable cache copy wedged suspend.
    #[test]
    fn promoting_retained_staging_sends_no_archive_bytes_and_reuses_the_known_home() {
        use std::os::unix::fs::PermissionsExt;

        if !in_isolated_store(
            "promoting_retained_staging_sends_no_archive_bytes_and_reuses_the_known_home",
            "MJ_HOST_CACHE_PROMOTE_CHILD",
        ) {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        write_shim(home, "sync", "touch \"$HOME/sync-called\"\nexit 1");
        let (staging, bytes, sha) = stage_on_host(home, "sess-1", "op-1");
        let locator = container_locator();
        let executor = LocalSsh::new(home);
        let row_id = record_host_staging_entry("sess-1", &locator, "op-1", &staging).unwrap();
        let rows = list_host_snapshot_cache_entries_for_session("sess-1").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].sha256, "staging:op-1");

        let cached = promote_host_snapshot_cache(
            &HostCachePromotion {
                session_id: "sess-1",
                locator: &locator,
                source_host_path: &staging,
                remove_source: true,
                known_remote_home: Some(&home.display().to_string()),
                sha256: &sha,
                staging_row_id: Some(row_id),
            },
            &executor,
        )
        .unwrap();

        assert_eq!(fs::read(&cached).unwrap(), bytes);
        assert_eq!(
            cached.file_name().unwrap().to_str().unwrap(),
            format!("{sha}.hel.zip")
        );
        assert_eq!(
            fs::metadata(&cached).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!home.join(&staging).exists(), "staging must be consumed");
        assert!(
            home.join(".local/share/hel/transfers").is_dir(),
            "the shared staging directory must stay"
        );
        assert!(!home.join("sync-called").exists());
        assert_eq!(
            executor
                .stdin_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert_eq!(
            executor.count("resolve remote host HOME for snapshot cache"),
            0
        );
        assert_eq!(
            executor.count("promote host archive into snapshot cache"),
            1
        );
        let rows = list_host_snapshot_cache_entries_for_session("sess-1").unwrap();
        assert_eq!(rows.len(), 1, "only the cache row remains: {rows:?}");
        assert_eq!(rows[0].sha256, sha);
        assert_eq!(rows[0].path, cached.to_string_lossy());

        // Resume reads the promoted copy through the shared verification.
        let bare = TargetLocator::SshBare {
            ssh: fixture_ssh(),
            workspace: "/srv/app".into(),
            worker_id: Some("sess-1".into()),
        };
        let restored = home.join("restored/archive.hel.zip");
        assert!(
            try_stage_host_snapshot_cache(
                &executor,
                &bare,
                "sess-1",
                &sha,
                &restored.to_string_lossy(),
            )
            .unwrap()
        );
        assert_eq!(fs::read(&restored).unwrap(), bytes);

        // A bare target's own archive is cached without being consumed, and
        // the home is looked up once when the transfer did not learn it.
        let own = home.join("worker/checkpoint.hel.zip");
        fs::create_dir_all(own.parent().unwrap()).unwrap();
        fs::write(&own, &bytes).unwrap();
        let second = promote_host_snapshot_cache(
            &HostCachePromotion {
                session_id: "sess-2",
                locator: &bare,
                source_host_path: &own.to_string_lossy(),
                remove_source: false,
                known_remote_home: None,
                sha256: &sha,
                staging_row_id: None,
            },
            &executor,
        )
        .unwrap();
        assert_ne!(second, cached);
        assert_eq!(fs::read(&second).unwrap(), bytes);
        assert!(own.exists(), "the owner removes its own archive");
        assert_eq!(
            executor.count("resolve remote host HOME for snapshot cache"),
            1
        );
        assert_eq!(
            executor
                .stdin_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
    }

    #[cfg(unix)]
    #[test]
    fn promotion_copies_then_renames_when_staging_is_on_another_filesystem() {
        use std::os::unix::fs::PermissionsExt;

        if !in_isolated_store(
            "promotion_copies_then_renames_when_staging_is_on_another_filesystem",
            "MJ_HOST_CACHE_EXDEV_CHILD",
        ) {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        // `ln` across filesystems fails with EXDEV; the shim fails the same way.
        write_shim(
            home,
            "ln",
            "echo 'ln: Invalid cross-device link' >&2\nexit 1",
        );
        let (staging, bytes, sha) = stage_on_host(home, "sess-x", "op-x");
        let locator = container_locator();
        let executor = LocalSsh::new(home);
        let row_id = record_host_staging_entry("sess-x", &locator, "op-x", &staging).unwrap();
        fn promotion<'a>(
            locator: &'a TargetLocator,
            home: &'a Path,
            sha: &'a str,
            source: &'a str,
            row_id: i64,
        ) -> HostCachePromotion<'a> {
            HostCachePromotion {
                session_id: "sess-x",
                locator,
                source_host_path: source,
                remove_source: true,
                known_remote_home: home.to_str(),
                sha256: sha,
                staging_row_id: Some(row_id),
            }
        }

        let cached = promote_host_snapshot_cache(
            &promotion(&locator, home, &sha, &staging, row_id),
            &executor,
        )
        .unwrap();
        assert_eq!(fs::read(&cached).unwrap(), bytes);
        assert_eq!(
            fs::metadata(&cached).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let copy_dir = cached.parent().unwrap();
        assert_eq!(
            fs::read_dir(copy_dir)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect::<Vec<_>>(),
            vec![format!("{sha}.hel.zip")],
            "no temporary file may remain beside the published archive"
        );
        assert!(!home.join(&staging).exists());

        // A copy that fails midway leaves no temporary file or empty copy
        // directory, and the staging file is still removed.
        write_shim(home, "cp", "head -c 1024 \"$2\" > \"$3\"\nexit 1");
        let (staging, _, _) = stage_on_host(home, "sess-x", "op-y");
        let row_id = record_host_staging_entry("sess-x", &locator, "op-y", &staging).unwrap();
        let before = fs::read_dir(copy_dir.parent().unwrap()).unwrap().count();
        let error = promote_host_snapshot_cache(
            &promotion(&locator, home, &sha, &staging, row_id),
            &executor,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("status"), "{error:#}");
        assert_eq!(
            fs::read_dir(copy_dir.parent().unwrap()).unwrap().count(),
            before,
            "the failed copy must not leave a copy directory"
        );
        assert!(!home.join(&staging).exists());
        let rows = list_host_snapshot_cache_entries_for_session("sess-x").unwrap();
        // The failed copy's outcome was not confirmed, so its row stays for
        // expiry to reclaim; no file exists at its path.
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert!(!Path::new(&rows[1].path).exists());
    }

    #[cfg(unix)]
    #[test]
    fn promotion_of_tampered_staging_fails_visibly_without_uploading_or_caching() {
        if !in_isolated_store(
            "promotion_of_tampered_staging_fails_visibly_without_uploading_or_caching",
            "MJ_HOST_CACHE_TAMPER_CHILD",
        ) {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let (staging, mut bytes, sha) = stage_on_host(home, "sess-t", "op-t");
        let locator = container_locator();
        let executor = LocalSsh::new(home);
        let row_id = record_host_staging_entry("sess-t", &locator, "op-t", &staging).unwrap();
        bytes[100 * 1024] ^= 0xff;
        fs::write(home.join(&staging), &bytes).unwrap();

        let error = promote_host_snapshot_cache(
            &HostCachePromotion {
                session_id: "sess-t",
                locator: &locator,
                source_host_path: &staging,
                remove_source: true,
                known_remote_home: Some(home.to_str().unwrap()),
                sha256: &sha,
                staging_row_id: Some(row_id),
            },
            &executor,
        )
        .unwrap_err();

        let text = format!("{error:#}");
        assert!(
            text.contains("status 46") && text.contains("differs"),
            "{text}"
        );
        assert!(!home.join(".cache").exists(), "nothing may be cached");
        assert!(
            !home.join(&staging).exists(),
            "tampered staging must be removed"
        );
        assert!(
            list_host_snapshot_cache_entries_for_session("sess-t")
                .unwrap()
                .is_empty(),
            "neither the cache row nor the staging row may remain"
        );
        assert_eq!(
            executor
                .stdin_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
    }

    #[cfg(unix)]
    #[test]
    fn interrupted_promotion_leaves_a_staging_row_that_destroy_and_expiry_reap() {
        if !in_isolated_store(
            "interrupted_promotion_leaves_a_staging_row_that_destroy_and_expiry_reap",
            "MJ_HOST_CACHE_CRASH_CHILD",
        ) {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let (staging, _, sha) = stage_on_host(home, "sess-c", "op-c");
        let locator = container_locator();
        let mut down = LocalSsh::new(home);
        down.unreachable = vec![
            "promote host archive into snapshot cache",
            "remove remote checkpoint staging",
        ];
        let healthy = LocalSsh::new(home);
        let row_id = record_host_staging_entry("sess-c", &locator, "op-c", &staging).unwrap();

        promote_host_snapshot_cache(
            &HostCachePromotion {
                session_id: "sess-c",
                locator: &locator,
                source_host_path: &staging,
                remove_source: true,
                known_remote_home: Some(home.to_str().unwrap()),
                sha256: &sha,
                staging_row_id: Some(row_id),
            },
            &down,
        )
        .unwrap_err();

        assert!(
            home.join(&staging).exists(),
            "an unreachable host keeps the file"
        );
        let rows = list_host_snapshot_cache_entries_for_session("sess-c").unwrap();
        assert_eq!(
            rows.len(),
            2,
            "staging row and possible cache row: {rows:?}"
        );
        assert!(
            rows.iter()
                .any(|row| row.id == row_id && row.path == staging)
        );
        // Resume lookups by a real digest never see the staging row.
        let found = lookup_host_snapshot_cache_entries(&host_cache_key(&locator), &sha).unwrap();
        assert_eq!(found.len(), 1);
        assert_ne!(found[0].path, staging);
        assert!(
            !try_stage_host_snapshot_cache(&healthy, &locator, "sess-c", &sha, "/unused.hel.zip")
                .unwrap(),
            "no cache copy exists, and the staging file is not one"
        );

        // Destroy reaps the file and both rows, and keeps the shared directory.
        let warnings = cleanup_session_host_snapshot_cache("sess-c", &healthy).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(!home.join(&staging).exists());
        assert!(home.join(".local/share/hel/transfers").is_dir());
        assert!(
            list_host_snapshot_cache_entries_for_session("sess-c")
                .unwrap()
                .is_empty()
        );

        // So does the expiry sweep, when nothing destroys the session.
        let (staging, _, _) = stage_on_host(home, "sess-e", "op-e");
        record_host_staging_entry("sess-e", &locator, "op-e", &staging).unwrap();
        let swept = super::super::leftovers::sweep_expired_host_snapshot_cache(
            &healthy,
            Utc::now() + chrono::Duration::hours(49),
            &|| false,
        );
        assert_eq!(swept.count, 1);
        assert!(!home.join(&staging).exists());
        assert!(home.join(".local/share/hel/transfers").is_dir());
        assert!(
            list_host_snapshot_cache_entries_for_session("sess-e")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn atomic_publish_verified_reuse_and_corrupt_hash_fallback() {
        const CHILD: &str = "MJ_HOST_CACHE_REUSE_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let directory = tempfile::tempdir().unwrap();
            IsolatedTest::new(
                "controller::checkpoint::host_cache::tests::atomic_publish_verified_reuse_and_corrupt_hash_fallback",
            )
            .env(CHILD, "1")
            .isolated_store(directory.path())
            .run();
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("verified.hel.zip");
        fs::write(&archive, b"fake verified archive payload").unwrap();
        let sha = checkpoint_sha256(&archive).unwrap();

        let locator = TargetLocator::LocalBare {
            worker_root: temp.path().join("worker").to_string_lossy().into_owned(),
        };

        let cached = publish_host_snapshot_cache("sess-1", &locator, &archive, &sha).unwrap();
        assert!(cached.exists(), "published cache file must exist");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let file_mode = fs::metadata(&cached).unwrap().permissions().mode() & 0o777;
            assert_eq!(file_mode, 0o600, "cached archive must be mode 0600");
            let dir_mode = fs::metadata(cached.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(dir_mode, 0o700, "cached copy directory must be mode 0700");
        }
        let rows = lookup_host_snapshot_cache_entries("local", &sha).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].session_id, "sess-1");
        assert_eq!(rows[0].path, cached.to_string_lossy());

        // Valid hash reuses the cached file.
        assert!(
            try_stage_host_snapshot_cache(
                &ProcessExecutor,
                &locator,
                "sess-1",
                &sha,
                "/unused/local-bare.hel.zip",
            )
            .unwrap()
        );

        // Corrupt the cached file: resume must detect mismatch, delete the
        // corrupted file, remove its DB row, and fall back (return false).
        fs::write(&cached, b"corrupted bytes").unwrap();
        assert!(
            !try_stage_host_snapshot_cache(
                &ProcessExecutor,
                &locator,
                "sess-1",
                &sha,
                "/unused/local-bare.hel.zip",
            )
            .unwrap()
        );
        assert!(!cached.exists(), "corrupted cache file must be deleted");
        assert!(
            lookup_host_snapshot_cache_entries("local", &sha)
                .unwrap()
                .is_empty(),
            "DB row must be removed after deleting corrupted file"
        );
    }

    #[test]
    fn destroy_cleanup_removes_superseded_copies_and_warns_on_unreachable_host() {
        const CHILD: &str = "MJ_HOST_CACHE_DESTROY_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let directory = tempfile::tempdir().unwrap();
            IsolatedTest::new(
                "controller::checkpoint::host_cache::tests::destroy_cleanup_removes_superseded_copies_and_warns_on_unreachable_host",
            )
            .env(CHILD, "1")
            .isolated_store(directory.path())
            .run();
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let archive1 = temp.path().join("a1.hel.zip");
        let archive2 = temp.path().join("a2.hel.zip");
        fs::write(&archive1, b"first suspend").unwrap();
        fs::write(&archive2, b"second suspend").unwrap();
        let sha1 = checkpoint_sha256(&archive1).unwrap();
        let sha2 = checkpoint_sha256(&archive2).unwrap();

        let locator = TargetLocator::LocalBare {
            worker_root: temp.path().join("worker").to_string_lossy().into_owned(),
        };
        let path1 =
            publish_host_snapshot_cache("sess-destroy", &locator, &archive1, &sha1).unwrap();
        let path2 =
            publish_host_snapshot_cache("sess-destroy", &locator, &archive2, &sha2).unwrap();

        // Also record a remote SSH copy for the same session on an unreachable host.
        let unreachable_ssh = SshTarget {
            destination: "unreachable.example".to_owned(),
            ssh_args: Vec::new(),
        };
        let remote_host = host_cache_key(&TargetLocator::SshBare {
            ssh: unreachable_ssh,
            workspace: "/srv/app".to_owned(),
            worker_id: Some("sess-destroy".to_owned()),
        });
        let remote_row_id = record_host_snapshot_cache_entry(
            "sess-destroy",
            &remote_host,
            &sha2,
            "/home/u/.cache/mjolnir/snapshot-cache/remote.hel.zip",
            &Utc::now().to_rfc3339(),
        )
        .unwrap();

        // Run cleanup with an executor that fails SSH commands (local uses std::fs directly).
        let warnings =
            cleanup_session_host_snapshot_cache("sess-destroy", &FailingSshExecutor).unwrap();
        assert!(!path1.exists(), "superseded local copy must be removed");
        assert!(!path2.exists(), "latest local copy must be removed");
        assert_eq!(warnings.len(), 1, "unreachable host must emit a warning");
        let remaining = list_host_snapshot_cache_entries_for_session("sess-destroy").unwrap();
        assert_eq!(remaining.len(), 1, "unreachable host row must be retained");
        assert_eq!(remaining[0].id, remote_row_id);
    }

    #[test]
    fn load_pack_bytes_into_bare_clone_cache_adds_objects() {
        let temp = tempfile::tempdir().unwrap();
        let src = temp.path().join("src");
        fs::create_dir_all(&src).unwrap();

        let git = |dir: &Path, args: &[&str]| {
            let mut full_args = vec!["-C".to_owned(), dir.to_string_lossy().into_owned()];
            full_args.extend(args.iter().map(|s| (*s).to_owned()));
            let out = ProcessExecutor
                .execute(&CommandSpec::new("git", full_args))
                .unwrap();
            assert_eq!(
                out.status,
                0,
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        };

        git(&src, &["init", "-b", "main"]);
        git(&src, &["config", "user.email", "test@example.com"]);
        git(&src, &["config", "user.name", "Test"]);
        fs::write(src.join("hello.txt"), "hello clone cache\n").unwrap();
        git(&src, &["add", "hello.txt"]);
        git(&src, &["commit", "-m", "initial"]);
        let commit_oid = git(&src, &["rev-parse", "HEAD"]);

        // Create a packfile containing the commit and its tree/blob.
        let pack_cmd = CommandSpec::new(
            "git",
            vec![
                "-C".to_owned(),
                src.to_string_lossy().into_owned(),
                "pack-objects".to_owned(),
                "--stdout".to_owned(),
                "--revs".to_owned(),
            ],
        );
        let mut pack_stdin = std::io::Cursor::new(format!("{commit_oid}\n").into_bytes());
        let pack_out = ProcessExecutor
            .execute_with_stdin(&pack_cmd, &mut pack_stdin)
            .unwrap();
        assert_eq!(pack_out.status, 0);
        assert!(!pack_out.stdout.is_empty());

        // Initialize an empty bare clone-cache mirror at the real
        // `.cache/mjolnir/git/mirrors/<key>/repo.git` layout and stream the pack.
        let origin = "https://github.com/brokkai/example-repo.git";
        let rel_mirror = crate::controller::git_cache::mirror_repo_relative_path(origin);
        assert!(
            rel_mirror.starts_with(".cache/mjolnir/git/mirrors/")
                && rel_mirror.ends_with("/repo.git"),
            "unexpected mirror layout: {rel_mirror}"
        );
        let cache = temp.path().join(&rel_mirror);
        fs::create_dir_all(&cache).unwrap();
        git(&cache, &["init", "--bare"]);
        let mut pack_reader = std::io::Cursor::new(&pack_out.stdout);
        let loaded = load_pack_reader_into_bare_repo(
            &HostCacheTarget::Local,
            &cache.to_string_lossy(),
            &mut pack_reader,
            &ProcessExecutor,
        )
        .unwrap();
        assert!(
            loaded,
            "existing bare clone cache mirror must accept the pack"
        );

        // Verify the commit object is now present in the clone cache mirror.
        let obj_type = git(&cache, &["cat-file", "-t", &commit_oid]);
        assert_eq!(obj_type, "commit");

        // Non-existent clone cache returns Ok(false).
        let missing = PathBuf::from(temp.path()).join("does-not-exist.git");
        let mut missing_reader = std::io::Cursor::new(&pack_out.stdout);
        let loaded_missing = load_pack_reader_into_bare_repo(
            &HostCacheTarget::Local,
            &missing.to_string_lossy(),
            &mut missing_reader,
            &ProcessExecutor,
        )
        .unwrap();
        assert!(!loaded_missing);
    }

    #[test]
    fn duplicate_sha256_rows_use_exclusive_paths_without_deletion_race() {
        const CHILD: &str = "MJ_HOST_CACHE_DUP_SHA_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let directory = tempfile::tempdir().unwrap();
            IsolatedTest::new(
                "controller::checkpoint::host_cache::tests::duplicate_sha256_rows_use_exclusive_paths_without_deletion_race",
            )
            .env(CHILD, "1")
            .isolated_store(directory.path())
            .run();
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("same.hel.zip");
        fs::write(&archive, b"identical archive content").unwrap();
        let sha = checkpoint_sha256(&archive).unwrap();

        let locator = TargetLocator::LocalBare {
            worker_root: temp.path().join("worker").to_string_lossy().into_owned(),
        };
        let path_a = publish_host_snapshot_cache("sess-a", &locator, &archive, &sha).unwrap();
        let path_b = publish_host_snapshot_cache("sess-b", &locator, &archive, &sha).unwrap();
        assert_ne!(
            path_a, path_b,
            "each host_snapshot_cache row must own an exclusive path"
        );
        assert!(path_a.exists() && path_b.exists());

        // Destroying sess-a removes path_a without touching sess-b's path_b.
        let warnings = cleanup_session_host_snapshot_cache("sess-a", &ProcessExecutor).unwrap();
        assert!(warnings.is_empty());
        assert!(!path_a.exists(), "destroyed session's copy must be removed");
        assert!(
            path_b.exists(),
            "other session's copy with the same sha256 must remain intact"
        );
        assert!(
            try_stage_host_snapshot_cache(
                &ProcessExecutor,
                &locator,
                "sess-b",
                &sha,
                "/unused.hel.zip",
            )
            .unwrap(),
            "sess-b must still be able to reuse its verified cached copy"
        );
    }

    #[cfg(unix)]
    #[test]
    fn recording_a_staging_row_wakes_the_expiry_task_and_ambiguous_failures_keep_cache_rows() {
        if !in_isolated_store(
            "recording_a_staging_row_wakes_the_expiry_task_and_ambiguous_failures_keep_cache_rows",
            "MJ_HOST_CACHE_WAKE_CHILD",
        ) {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let locator = container_locator();

        // Recording wakes a task that is sleeping on an empty table.
        let wakeup = host_snapshot_cache_wakeup();
        let (staging, _, sha) = stage_on_host(home, "sess-w", "op-w");
        let row_id = record_host_staging_entry("sess-w", &locator, "op-w", &staging).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(5), wakeup.notified())
                .await
                .expect("recording a staging row must wake the expiry task");
        });

        // A promotion that dies with an unknown outcome (status 255, as a
        // dropped SSH session reports) keeps the cache row even though no
        // probe can confirm the file; a status the script exits with before
        // writing anything (46) removes it.
        let promote = |executor: &LocalSsh| {
            promote_host_snapshot_cache(
                &HostCachePromotion {
                    session_id: "sess-w",
                    locator: &locator,
                    source_host_path: &staging,
                    remove_source: false,
                    known_remote_home: home.to_str(),
                    sha256: &sha,
                    staging_row_id: None,
                },
                executor,
            )
        };
        write_shim(home, "mv", "exit 255");
        let executor = LocalSsh::new(home);
        let error = promote(&executor).unwrap_err();
        assert!(format!("{error:#}").contains("status 255"), "{error:#}");
        let cache_rows = |session: &str| {
            list_host_snapshot_cache_entries_for_session(session)
                .unwrap()
                .into_iter()
                .filter(|row| row.sha256 == sha)
                .count()
        };
        assert_eq!(
            cache_rows("sess-w"),
            1,
            "unknown outcome keeps the cache row"
        );
        // The script's trap removed its temporary file and empty directory.
        assert_eq!(
            fs::read_dir(home.join(".cache/mjolnir/snapshot-cache"))
                .unwrap()
                .count(),
            0
        );

        fs::write(home.join(&staging), b"tampered").unwrap();
        let before = cache_rows("sess-w");
        promote(&executor).unwrap_err();
        assert_eq!(cache_rows("sess-w"), before, "status 46 drops its own row");
        retire_host_staging_entry(row_id);
    }
}
