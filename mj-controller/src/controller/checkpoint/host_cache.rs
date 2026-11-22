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
/// Lives under `mj_core::config::data_dir()` so isolated `--instance` daemons
/// and unit tests never share or clobber each other's snapshot cache.
pub fn local_snapshot_cache_dir() -> PathBuf {
    mj_core::config::data_dir().join("snapshot-cache")
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

/// Best-effort atomic publish of a verified suspend archive into the target
/// host's snapshot cache.
///
/// Records the `host_snapshot_cache` row BEFORE writing the file, writes to a
/// temporary file in the same directory, syncs, and renames to
/// `<sha256>.hel.zip`. Never fails suspend: errors are logged at WARN.
pub fn publish_host_snapshot_cache_best_effort(
    session_id: &str,
    locator: &TargetLocator,
    verified_archive: &Path,
    sha256: &str,
    executor: &impl CommandExecutor,
) {
    if let Err(error) =
        publish_host_snapshot_cache(session_id, locator, verified_archive, sha256, executor)
    {
        tracing::warn!(
            session_id,
            sha256,
            host = %host_cache_key(locator),
            error = %format!("{error:#}"),
            "could not populate host snapshot cache after suspend"
        );
    }
}

/// Atomically publish `verified_archive` into `locator`'s host snapshot cache.
///
/// Each row gets its own exclusive `<copy_id>/<sha256>.hel.zip` path so
/// expiring an older row or destroying one session can never delete a file
/// belonging to a newer suspend or another session.
pub fn publish_host_snapshot_cache(
    session_id: &str,
    locator: &TargetLocator,
    verified_archive: &Path,
    sha256: &str,
    executor: &impl CommandExecutor,
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
                    dest.sync_all().with_context(|| {
                        format!("sync temp snapshot cache file {}", temp_path.display())
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
        HostCacheTarget::Ssh(ssh) => {
            let remote_home = resolve_ssh_home(&ssh, executor)?;
            let remote_root = format!("{remote_home}/.cache/mjolnir/snapshot-cache");
            let remote_copy_dir = format!("{remote_root}/{copy_id}");
            let final_path = format!("{remote_copy_dir}/{}", cache_filename(sha256));
            let row_id = record_host_snapshot_cache_entry(
                session_id,
                &host_key,
                sha256,
                &final_path,
                &written_at,
            )?;
            let nonce = new_command_id("snap")?;
            let temp_path =
                format!("{remote_copy_dir}/{SNAPSHOT_CACHE_TEMP_PREFIX}{sha256}-{nonce}");
            let mut archive_file = File::open(verified_archive).with_context(|| {
                format!(
                    "open verified archive {} for remote host cache",
                    verified_archive.display()
                )
            })?;
            let script = "set -eu; umask 077; root=$1; dir=$2; tmp=$3; final=$4; \
                          mkdir -p -- \"$root\" \"$dir\"; \
                          chmod 0700 \"$root\" \"$dir\"; \
                          trap 'rm -f -- \"$tmp\"; rmdir -- \"$dir\" 2>/dev/null || true' EXIT; \
                          cat > \"$tmp\"; \
                          chmod 0600 \"$tmp\"; \
                          sync \"$tmp\" 2>/dev/null || sync; \
                          mv -f -- \"$tmp\" \"$final\"; \
                          trap - EXIT";
            let command = mj_core::targets::ssh_command_owned(
                SshTrafficClass::Bulk,
                &ssh,
                vec![
                    "sh".to_owned(),
                    "-c".to_owned(),
                    script.to_owned(),
                    "hel-snapshot-cache-publish".to_owned(),
                    remote_root,
                    remote_copy_dir,
                    temp_path,
                    final_path.clone(),
                ],
            )
            .purpose("publish host snapshot cache archive");
            let output = executor.execute_with_stdin(&command, &mut archive_file);
            match output {
                Ok(out) if out.status == 0 => {
                    notify_host_snapshot_cache_updated();
                }
                Ok(out) => {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    if matches!(remote_file_exists(&ssh, &final_path, executor), Ok(false)) {
                        let _ = delete_host_snapshot_cache_entry(row_id);
                    } else {
                        // Row stays recorded because the file may exist on the
                        // remote host; wake the expiry sweep so it is reclaimed.
                        notify_host_snapshot_cache_updated();
                    }
                    bail!(
                        "remote snapshot cache publish failed with status {}: {}",
                        out.status,
                        stderr.trim()
                    );
                }
                Err(error) => {
                    if matches!(remote_file_exists(&ssh, &final_path, executor), Ok(false)) {
                        let _ = delete_host_snapshot_cache_entry(row_id);
                    } else {
                        notify_host_snapshot_cache_updated();
                    }
                    return Err(error);
                }
            }
            Ok(PathBuf::from(final_path))
        }
    }
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
            let script = "set -eu; p=$1; [ -f \"$p\" ] || exit 44; \
                          if command -v sha256sum >/dev/null 2>&1; then \
                              sha256sum -- \"$p\" | awk '{print $1}'; \
                          elif command -v shasum >/dev/null 2>&1; then \
                              shasum -a 256 -- \"$p\" | awk '{print $1}'; \
                          else \
                              exit 45; \
                          fi";
            let command = mj_core::targets::ssh_command_owned(
                SshTrafficClass::Control,
                ssh,
                vec![
                    "sh".to_owned(),
                    "-c".to_owned(),
                    script.to_owned(),
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
            let script = "set -eu; p=$1; dir=$(dirname -- \"$p\"); \
                          rm -f -- \"$p\"; \
                          for tmp in \"$dir\"/.tmp-*; do \
                              [ -e \"$tmp\" ] || continue; \
                              rm -f -- \"$tmp\"; \
                          done; \
                          rmdir -- \"$dir\" 2>/dev/null || true";
            let command = mj_core::targets::ssh_command_owned(
                SshTrafficClass::Control,
                ssh,
                vec![
                    "sh".to_owned(),
                    "-c".to_owned(),
                    script.to_owned(),
                    "hel-snapshot-cache-remove".to_owned(),
                    path.to_owned(),
                ],
            )
            .purpose("remove host snapshot cache archive");
            let output = executor.execute(&command)?;
            ensure!(
                output.status == 0,
                "remove remote snapshot cache file failed with status {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
            Ok(0)
        }
    }
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

fn remote_file_exists(
    ssh: &SshTarget,
    path: &str,
    executor: &impl CommandExecutor,
) -> Result<bool> {
    let command = mj_core::targets::ssh_command_owned(
        SshTrafficClass::Control,
        ssh,
        vec![
            "sh".to_owned(),
            "-c".to_owned(),
            "test -f \"$1\"".to_owned(),
            "hel-snapshot-cache-exists".to_owned(),
            path.to_owned(),
        ],
    )
    .purpose("check whether remote snapshot cache archive exists");
    let output = executor.execute(&command)?;
    Ok(output.status == 0)
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

        let cached =
            publish_host_snapshot_cache("sess-1", &locator, &archive, &sha, &ProcessExecutor)
                .unwrap();
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
        let path1 = publish_host_snapshot_cache(
            "sess-destroy",
            &locator,
            &archive1,
            &sha1,
            &ProcessExecutor,
        )
        .unwrap();
        let path2 = publish_host_snapshot_cache(
            "sess-destroy",
            &locator,
            &archive2,
            &sha2,
            &ProcessExecutor,
        )
        .unwrap();

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
        let path_a =
            publish_host_snapshot_cache("sess-a", &locator, &archive, &sha, &ProcessExecutor)
                .unwrap();
        let path_b =
            publish_host_snapshot_cache("sess-b", &locator, &archive, &sha, &ProcessExecutor)
                .unwrap();
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
}
