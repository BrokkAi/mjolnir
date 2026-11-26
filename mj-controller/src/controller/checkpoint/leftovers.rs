//! The files a checkpoint writes in its worker root, and the sweep that
//! removes them once no checkpoint can own them.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::Result;
use chrono::{DateTime, Utc};
use mj_checkpoint::checkpoint::CHECKPOINT_CAPTURE_TEMPORARY_PREFIX;

use crate::database::{
    HostSnapshotCacheEntry, delete_host_snapshot_cache_entry,
    list_overdue_host_snapshot_cache_entries,
};
use crate::session_manager::new_command_id;
use crate::targets::CommandExecutor;

use super::host_cache::{
    HostCachePathFailure, HostCacheTarget, SNAPSHOT_CACHE_TEMP_PREFIX, local_snapshot_cache_dir,
    parse_host_cache_key, remove_host_cache_file,
};

/// How long a host-side snapshot archive copy is retained before expiry sweep.
pub const HOST_SNAPSHOT_CACHE_TTL: Duration = Duration::from_secs(48 * 3600);

const OPERATION_PREFIX: &str = "checkpoint";

/// The names one checkpoint operation gives its files in its worker root.
///
/// The stage and the archive are named by the operation ID, so two
/// checkpoints of one session never share one. [`is_checkpoint_leftover`]
/// recognises the same names.
pub(super) struct TargetCheckpointOperation {
    pub(super) id: String,
    pub(super) stage: String,
    pub(super) archive: String,
}

impl TargetCheckpointOperation {
    pub(super) fn new(worker_root: &str) -> Result<Self> {
        let id = new_command_id(OPERATION_PREFIX)?;
        Ok(Self {
            stage: format!("{worker_root}/{id}-stage"),
            archive: format!("{worker_root}/{id}.hel.zip"),
            id,
        })
    }
}

/// Whether a worker root entry is a file a checkpoint operation writes: its
/// stage, its archive, or the temporary directory the worker seals a stage
/// from.
fn is_checkpoint_leftover(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    if name.starts_with(CHECKPOINT_CAPTURE_TEMPORARY_PREFIX) {
        return true;
    }
    let Some(nonce) = name
        .strip_prefix(OPERATION_PREFIX)
        .and_then(|rest| rest.strip_prefix('-'))
        .and_then(|rest| {
            rest.strip_suffix("-stage")
                .or_else(|| rest.strip_suffix(".hel.zip"))
        })
    else {
        return false;
    };
    nonce.len() == 32
        && nonce
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// What the startup sweep removed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CheckpointLeftovers {
    pub count: usize,
    pub bytes: u64,
}

/// Remove the checkpoint files no checkpoint can still own from every local
/// worker root, and log what that freed.
///
/// A checkpoint removes its stage and its archive on every exit but one: a
/// cancelled checkpoint leaves them, and so does a daemon that stops in the
/// middle of one. A capture that is killed also leaves the temporary
/// directory it was filling. Nothing else removes those, so daemon start
/// does.
///
/// A file is left alone while a checkpoint may still own it: when it changed
/// after `started`, as the files of every checkpoint this daemon starts do,
/// or when a checkpoint command still runs from its worker root, as one can
/// after a daemon that stopped without stopping its children. Nothing is
/// removed when the running processes cannot be listed. `stop` is asked
/// between entries, so the sweep does not hold up shutdown; the next start
/// finishes it.
///
/// Only the local worker roots in this instance's data directory are swept.
/// A remote target keeps what a cancelled checkpoint left there.
pub fn sweep_local_checkpoint_leftovers(
    executor: &impl CommandExecutor,
    started: SystemTime,
    stop: &dyn Fn() -> bool,
) -> CheckpointLeftovers {
    let Some(running) = crate::controller::local_profile_homes::running_process_arguments(executor)
    else {
        tracing::warn!("left checkpoint files in local worker roots in place");
        return CheckpointLeftovers::default();
    };
    // A Move owns its exported archive across daemon and worker restarts.
    // This inventory protects captures older than the sweep's cutoff; any
    // new capture after startup is already protected by its modification time.
    let retained_move_snapshots = match crate::database::load_move_operations() {
        Ok(operations) => operations
            .into_iter()
            .filter(|operation| {
                operation.checkpoint_retained()
                    && matches!(
                        operation.source_target,
                        Some(mj_core::state::TargetLocator::LocalBare { .. })
                    )
            })
            .filter_map(|operation| {
                operation
                    .source_snapshot
                    .map(|snapshot| snapshot.archive_path)
            })
            .collect(),
        Err(error) => {
            tracing::warn!(%error, "could not check Move snapshot ownership; leaving checkpoint files in place");
            return CheckpointLeftovers::default();
        }
    };
    let swept = sweep_checkpoint_leftovers_in(
        &mj_core::config::DataLayout::current().workers_dir(),
        started,
        &running,
        &retained_move_snapshots,
        stop,
    );
    if swept.count > 0 {
        tracing::info!(
            count = swept.count,
            bytes = swept.bytes,
            "removed checkpoint files that earlier checkpoints left in local worker roots"
        );
    }
    swept
}

pub(super) fn sweep_checkpoint_leftovers_in(
    workers: &Path,
    started: SystemTime,
    running: &[String],
    retained_move_snapshots: &BTreeSet<PathBuf>,
    stop: &dyn Fn() -> bool,
) -> CheckpointLeftovers {
    let mut swept = CheckpointLeftovers::default();
    let roots = match std::fs::read_dir(workers) {
        Ok(roots) => roots,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return swept,
        Err(error) => {
            tracing::warn!(
                path = %workers.display(),
                %error,
                "could not scan local worker roots for checkpoint files"
            );
            return swept;
        }
    };
    for root in roots.flatten().map(|entry| entry.path()) {
        if !std::fs::symlink_metadata(&root).is_ok_and(|metadata| metadata.is_dir())
            || checkpoint_command_running(&root, running)
        {
            continue;
        }
        let entries = match std::fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(error) => {
                tracing::warn!(
                    path = %root.display(),
                    %error,
                    "could not scan a local worker root for checkpoint files"
                );
                continue;
            }
        };
        for entry in entries.flatten() {
            if stop() {
                return swept;
            }
            if !is_checkpoint_leftover(&entry.file_name()) {
                continue;
            }
            let path = entry.path();
            if retained_move_snapshots.contains(&path) {
                continue;
            }
            // A checkpoint never writes a link, so one with a checkpoint's
            // name is not followed or removed.
            let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if !metadata.modified().is_ok_and(|modified| modified < started) {
                continue;
            }
            let (bytes, removed) = if metadata.is_dir() {
                (tree_bytes(&path), std::fs::remove_dir_all(&path))
            } else if metadata.is_file() {
                (metadata.len(), std::fs::remove_file(&path))
            } else {
                continue;
            };
            match removed {
                Ok(()) => {
                    swept.count += 1;
                    swept.bytes = swept.bytes.saturating_add(bytes);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => tracing::warn!(
                    path = %path.display(),
                    %error,
                    "could not remove a checkpoint file left in a local worker root"
                ),
            }
        }
    }
    swept
}

/// Whether a checkpoint command runs from this worker root's own binary. It
/// may still be writing the files the sweep would remove, and a writer
/// recreates what is deleted underneath it.
fn checkpoint_command_running(root: &Path, running: &[String]) -> bool {
    let binary = format!("{}/hel worker ", root.display());
    running.iter().any(|arguments| {
        arguments.strip_prefix(&binary).is_some_and(|subcommand| {
            ["capture-checkpoint", "pack-checkpoint", "export-checkpoint"]
                .iter()
                .any(|name| subcommand.starts_with(name))
        })
    })
}

/// How long a host that failed its sweep is left alone, by consecutive failure;
/// the last delay repeats. Hosts are retried on a doubling schedule so a VM
/// that is gone is probed at most every half hour, while a host with a brief
/// outage is back within a minute or two.
const HOST_SWEEP_BACKOFF_SECONDS: [i64; 6] = [60, 120, 240, 480, 900, 1800];

struct HostSweepBackoff {
    failures: usize,
    retry_at: DateTime<Utc>,
}

/// The sweep's memory of unreachable hosts, per data directory so isolated
/// instances never share it. It is a retry delay only: the rows themselves
/// stay in SQLite, so a restarted daemon simply tries every host once.
fn host_sweep_backoff()
-> &'static std::sync::Mutex<std::collections::HashMap<(PathBuf, String), HostSweepBackoff>> {
    static LEDGER: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<(PathBuf, String), HostSweepBackoff>>,
    > = std::sync::OnceLock::new();
    LEDGER.get_or_init(Default::default)
}

fn host_sweep_key(host: &str) -> (PathBuf, String) {
    (
        mj_core::config::DataLayout::current().root().to_path_buf(),
        host.to_owned(),
    )
}

fn host_is_backed_off(host: &str, now: DateTime<Utc>) -> bool {
    host_sweep_backoff()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&host_sweep_key(host))
        .is_some_and(|backoff| backoff.retry_at > now)
}

fn record_host_sweep_failure(host: &str, now: DateTime<Utc>) {
    let mut ledger = host_sweep_backoff()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let backoff = ledger
        .entry(host_sweep_key(host))
        .or_insert(HostSweepBackoff {
            failures: 0,
            retry_at: now,
        });
    let delay =
        HOST_SWEEP_BACKOFF_SECONDS[backoff.failures.min(HOST_SWEEP_BACKOFF_SECONDS.len() - 1)];
    backoff.failures += 1;
    backoff.retry_at = now + chrono::Duration::seconds(delay);
}

fn clear_host_sweep_backoff(host: &str) {
    host_sweep_backoff()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(&host_sweep_key(host));
}

#[cfg(all(test, unix))]
pub(crate) fn clear_host_sweep_backoff_for_test() {
    host_sweep_backoff()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
}

/// Remove one host's overdue copies in order. An SSH host that fails once is
/// not asked about its remaining copies: they would fail the same way, after
/// the same wait. It is left alone until its backoff expires.
fn sweep_host_snapshot_group(
    host_key: &str,
    entries: Vec<HostSnapshotCacheEntry>,
    executor: &(impl CommandExecutor + Sync),
    now: DateTime<Utc>,
    stop: &(dyn Fn() -> bool + Sync),
) -> CheckpointLeftovers {
    let mut swept = CheckpointLeftovers::default();
    let host = match parse_host_cache_key(host_key) {
        Ok(host) => host,
        Err(error) => {
            tracing::warn!(
                host = %host_key,
                rows = entries.len(),
                error = %format!("{error:#}"),
                "skipping overdue host_snapshot_cache rows with invalid host key"
            );
            return swept;
        }
    };
    for entry in entries {
        if stop() {
            return swept;
        }
        match remove_host_cache_file(&host, &entry.path, executor) {
            Ok(bytes) => {
                clear_host_sweep_backoff(host_key);
                if let Err(error) = delete_host_snapshot_cache_entry(entry.id) {
                    tracing::warn!(
                        row_id = entry.id,
                        error = %format!("{error:#}"),
                        "could not delete expired host_snapshot_cache row after removing file"
                    );
                } else {
                    swept.count += 1;
                    swept.bytes = swept.bytes.saturating_add(bytes);
                }
            }
            Err(error) => {
                tracing::warn!(
                    row_id = entry.id,
                    host = %entry.host,
                    path = %entry.path,
                    error = %format!("{error:#}"),
                    "could not remove overdue host snapshot cache file; keeping row for retry"
                );
                if matches!(host, HostCacheTarget::Ssh(_)) {
                    if error.is::<HostCachePathFailure>() {
                        // The host answered; only this path failed. Its other
                        // rows are still worth trying.
                        clear_host_sweep_backoff(host_key);
                    } else {
                        record_host_sweep_failure(host_key, now);
                        return swept;
                    }
                }
            }
        }
    }
    swept
}

/// Remove overdue `host_snapshot_cache` copies whose `written_at + 48h <= now`,
/// plus any orphaned temporary publish files older than `HOST_SNAPSHOT_CACHE_TTL`
/// in the local snapshot cache directory.
///
/// Each DB row is deleted ONLY after its file is confirmed removed (or already
/// absent). If a host is unreachable, its row stays in SQLite for the next
/// sweep. Distinct hosts are swept at the same time, so a slow or unreachable
/// host never delays a healthy one, and a host that failed is skipped until its
/// backoff (counted from the sweep's `now`) expires.
pub fn sweep_expired_host_snapshot_cache(
    executor: &(impl CommandExecutor + Sync),
    now: DateTime<Utc>,
    stop: &(dyn Fn() -> bool + Sync),
) -> CheckpointLeftovers {
    let mut swept = CheckpointLeftovers::default();
    let cutoff = now - chrono::Duration::seconds(HOST_SNAPSHOT_CACHE_TTL.as_secs() as i64);
    let entries = match list_overdue_host_snapshot_cache_entries(&cutoff.to_rfc3339()) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::warn!(
                error = %format!("{error:#}"),
                "could not query overdue host_snapshot_cache entries"
            );
            return swept;
        }
    };
    let mut hosts: std::collections::BTreeMap<String, Vec<HostSnapshotCacheEntry>> =
        std::collections::BTreeMap::new();
    for entry in entries {
        hosts.entry(entry.host.clone()).or_default().push(entry);
    }
    hosts.retain(|host, _| !host_is_backed_off(host, now));
    // Each host's thread starts outside the runtime this one may run in.
    let runtime = tokio::runtime::Handle::try_current().ok();
    std::thread::scope(|scope| {
        let threads: Vec<_> = hosts
            .into_iter()
            .map(|(host, entries)| {
                let runtime = runtime.clone();
                scope.spawn(move || {
                    let _entered = runtime.as_ref().map(tokio::runtime::Handle::enter);
                    sweep_host_snapshot_group(&host, entries, executor, now, stop)
                })
            })
            .collect();
        for thread in threads {
            match thread.join() {
                Ok(group) => {
                    swept.count += group.count;
                    swept.bytes = swept.bytes.saturating_add(group.bytes);
                }
                Err(panic) => tracing::error!(
                    panic = %crate::targets::command_thread_panic_message(panic.as_ref()),
                    "host snapshot cache sweep panicked"
                ),
            }
        }
    });
    // Also sweep orphaned temp files in the local snapshot cache directory
    // (both top-level and inside per-copy subdirectories).
    let local_dir = local_snapshot_cache_dir();
    if let Ok(entries) = std::fs::read_dir(&local_dir) {
        let cutoff_sys = SystemTime::now()
            .checked_sub(HOST_SNAPSHOT_CACHE_TTL)
            .unwrap_or(SystemTime::UNIX_EPOCH);
        for entry in entries.flatten() {
            if stop() {
                return swept;
            }
            let path = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.is_dir() {
                if let Ok(sub_entries) = std::fs::read_dir(&path) {
                    for sub in sub_entries.flatten() {
                        if stop() {
                            return swept;
                        }
                        let Some(name) = sub.file_name().to_str().map(ToOwned::to_owned) else {
                            continue;
                        };
                        if !name.starts_with(SNAPSHOT_CACHE_TEMP_PREFIX) {
                            continue;
                        }
                        let sub_path = sub.path();
                        let Ok(sub_meta) = std::fs::symlink_metadata(&sub_path) else {
                            continue;
                        };
                        if !sub_meta.is_file()
                            || !sub_meta.modified().is_ok_and(|m| m <= cutoff_sys)
                        {
                            continue;
                        }
                        if std::fs::remove_file(&sub_path).is_ok() {
                            swept.count += 1;
                            swept.bytes = swept.bytes.saturating_add(sub_meta.len());
                        }
                    }
                }
                if meta.modified().is_ok_and(|m| m <= cutoff_sys) {
                    let _ = std::fs::remove_dir(&path);
                }
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(ToOwned::to_owned) else {
                continue;
            };
            if !name.starts_with(SNAPSHOT_CACHE_TEMP_PREFIX) {
                continue;
            }
            if !meta.is_file() || !meta.modified().is_ok_and(|m| m <= cutoff_sys) {
                continue;
            }
            if std::fs::remove_file(&path).is_ok() {
                swept.count += 1;
                swept.bytes = swept.bytes.saturating_add(meta.len());
            }
        }
    }
    if swept.count > 0 {
        tracing::info!(
            count = swept.count,
            bytes = swept.bytes,
            "removed expired host snapshot cache archives"
        );
    }
    swept
}

/// The bytes the regular files under `path` hold, without following links.
fn tree_bytes(path: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.metadata() {
            Ok(metadata) if metadata.is_dir() => tree_bytes(&entry.path()),
            Ok(metadata) if metadata.is_file() => metadata.len(),
            _ => 0,
        })
        .fold(0, u64::saturating_add)
}

#[cfg(all(test, unix))]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use super::*;

    const ID: &str = "0123456789abcdef0123456789abcdef";
    const OTHER_ID: &str = "fedcba9876543210fedcba9876543210";

    fn write(path: &Path, bytes: usize) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, vec![1_u8; bytes]).unwrap();
    }

    fn set_modified(path: &Path, time: SystemTime) {
        std::fs::File::open(path)
            .unwrap()
            .set_modified(time)
            .unwrap();
    }

    /// Only the files a checkpoint names by its operation ID, and the
    /// directory the worker seals a stage from, are removed, and only when no
    /// running checkpoint can own them: older than this daemon's start, and
    /// in a worker root with no checkpoint command running. Everything else a
    /// worker root holds stays.
    // Hard-won: 8de4dde8d: startup garbage collection deleted an archive retained for a Move retry.
    #[test]
    fn the_sweep_removes_only_checkpoint_files_nothing_can_still_own() {
        let directory = tempfile::tempdir().unwrap();
        let workers = directory.path().join("workers");
        let started = SystemTime::now();
        let before = started - Duration::from_secs(3600);
        let after = started + Duration::from_secs(60);

        let root = workers.join("session-a");
        let stage = root.join(format!("checkpoint-{ID}-stage"));
        write(&stage.join("native/00000000"), 1000);
        write(&stage.join("stage.json"), 2);
        let capture = root.join(".checkpoint-capture-Ab12Cd");
        write(&capture.join("native/00000000"), 500);
        let archive = root.join(format!("checkpoint-{ID}.hel.zip"));
        write(&archive, 300);
        let move_snapshot = root.join(format!("checkpoint-{OTHER_ID}.hel.zip"));
        write(&move_snapshot, 400);
        // Started by this daemon, after the sweep's cut.
        let current = root.join(format!("checkpoint-{OTHER_ID}-stage"));
        write(&current.join("stage.json"), 2);
        // Everything else in a worker root.
        let kept = [
            root.join("launch.json"),
            root.join("restore.hel.zip"),
            root.join("profile/auth.json"),
            root.join("checkpoint-notes-stage/file"),
            root.join(format!("checkpoint-{ID}-stage.json")),
        ];
        for path in &kept {
            write(path, 10);
        }
        // A link with a stage's name: the sweep never follows one.
        let outside = directory.path().join("outside");
        write(&outside.join("keep"), 10);
        let link = root.join(format!("checkpoint-{}-stage", "a".repeat(32)));
        std::os::unix::fs::symlink(&outside, &link).unwrap();

        // A worker root whose checkpoint command still runs, as a daemon
        // that stopped without stopping its children leaves it.
        let busy = workers.join("session-b");
        let busy_stage = busy.join(format!("checkpoint-{ID}-stage"));
        write(&busy_stage.join("stage.json"), 2);
        let running = vec![
            format!(
                "{}/hel worker run --root {}",
                root.display(),
                root.display()
            ),
            format!("{}/hel worker capture-checkpoint", busy.display()),
        ];
        // Pinned worker binaries live beside the worker roots.
        let pinned = workers.join("pinned/hel");
        write(&pinned, 10);

        for path in [&stage, &capture, &archive, &busy_stage, &move_snapshot] {
            set_modified(path, before);
        }
        for path in &kept {
            set_modified(path, before);
        }
        set_modified(&current, after);

        let swept = sweep_checkpoint_leftovers_in(
            &workers,
            started,
            &running,
            &BTreeSet::from([move_snapshot.clone()]),
            &|| false,
        );

        assert_eq!(
            swept,
            CheckpointLeftovers {
                count: 3,
                bytes: 1802
            }
        );
        for removed in [&stage, &capture, &archive] {
            assert!(!removed.exists(), "{} was kept", removed.display());
        }
        let expected_kept: Vec<PathBuf> = kept
            .iter()
            .cloned()
            .chain([
                current,
                busy_stage,
                pinned,
                outside.join("keep"),
                move_snapshot,
            ])
            .collect();
        for path in &expected_kept {
            assert!(path.exists(), "{} was removed", path.display());
        }
        assert!(std::fs::symlink_metadata(&link).is_ok());
    }

    #[test]
    fn expired_host_snapshot_cache_sweep_deletes_overdue_and_keeps_fresh() {
        const CHILD: &str = "MJ_HOST_CACHE_SWEEP_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let directory = tempfile::tempdir().unwrap();
            crate::controller::test_support::IsolatedTest::new(
                "controller::checkpoint::leftovers::tests::expired_host_snapshot_cache_sweep_deletes_overdue_and_keeps_fresh",
            )
            .env(CHILD, "1")
            .isolated_store(directory.path())
            .run();
            return;
        }
        let dir = local_snapshot_cache_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let overdue_path = dir.join("overdue.hel.zip");
        let fresh_path = dir.join("fresh.hel.zip");
        std::fs::write(&overdue_path, b"old archive").unwrap();
        std::fs::write(&fresh_path, b"new archive").unwrap();

        let now = Utc::now();
        let overdue_at = (now - chrono::Duration::hours(49)).to_rfc3339();
        let fresh_at = (now - chrono::Duration::hours(1)).to_rfc3339();

        crate::database::record_host_snapshot_cache_entry(
            "sess-old",
            "local",
            "sha-old",
            &overdue_path.to_string_lossy(),
            &overdue_at,
        )
        .unwrap();
        crate::database::record_host_snapshot_cache_entry(
            "sess-new",
            "local",
            "sha-new",
            &fresh_path.to_string_lossy(),
            &fresh_at,
        )
        .unwrap();

        let swept =
            sweep_expired_host_snapshot_cache(&crate::targets::ProcessExecutor, now, &|| false);
        assert_eq!(swept.count, 1);
        assert_eq!(swept.bytes, b"old archive".len() as u64);
        assert!(!overdue_path.exists(), "overdue file must be deleted");
        assert!(fresh_path.exists(), "fresh file must be kept");
        assert!(
            crate::database::list_host_snapshot_cache_entries_for_session("sess-old")
                .unwrap()
                .is_empty(),
            "overdue DB row must be deleted"
        );
        assert_eq!(
            crate::database::list_host_snapshot_cache_entries_for_session("sess-new")
                .unwrap()
                .len(),
            1,
            "fresh DB row must be kept"
        );
    }

    /// Re-run `test` alone in a child with its own store; true in the child.
    fn in_isolated_store(test: &str, marker: &str) -> bool {
        if std::env::var_os(marker).is_some() {
            return true;
        }
        let directory = tempfile::tempdir().unwrap();
        crate::controller::test_support::IsolatedTest::new(format!(
            "controller::checkpoint::leftovers::tests::{test}"
        ))
        .env(marker, "1")
        .isolated_store(directory.path())
        .run();
        false
    }

    fn ssh_locator(destination: &str) -> mj_core::targets::TargetLocator {
        mj_core::targets::TargetLocator::SshBare {
            ssh: mj_core::targets::SshTarget {
                destination: destination.into(),
                ssh_args: Vec::new(),
            },
            workspace: "/work".into(),
            worker_id: None,
        }
    }

    /// Hosts that answer the sweep's removal command, or fail it, or wait.
    #[derive(Default)]
    struct SweepHosts {
        calls: std::sync::Mutex<Vec<String>>,
        failing: std::sync::Mutex<std::collections::BTreeSet<String>>,
        /// A host that answers only once `healthy_done` is set.
        slow: Option<&'static str>,
        healthy_done: std::sync::atomic::AtomicBool,
        healthy: Option<&'static str>,
    }

    impl SweepHosts {
        fn calls_to(&self, destination: &str) -> usize {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|call| call.as_str() == destination)
                .count()
        }
    }

    impl CommandExecutor for SweepHosts {
        fn execute(
            &self,
            command: &crate::targets::CommandSpec,
        ) -> Result<crate::targets::CommandOutput> {
            let destination = command
                .args
                .iter()
                .find(|arg| arg.ends_with(".example"))
                .expect("an SSH destination")
                .clone();
            self.calls.lock().unwrap().push(destination.clone());
            if self.slow == Some(destination.as_str()) {
                let started = std::time::Instant::now();
                while !self.healthy_done.load(std::sync::atomic::Ordering::SeqCst) {
                    assert!(
                        started.elapsed() < Duration::from_secs(20),
                        "the slow host was swept before the healthy one finished"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            let status = i32::from(self.failing.lock().unwrap().contains(&destination));
            if self.healthy == Some(destination.as_str()) {
                self.healthy_done
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
            Ok(crate::targets::CommandOutput {
                status,
                stdout: Vec::new(),
                stderr: if status == 0 {
                    Vec::new()
                } else {
                    b"ssh: connect to host: timed out".to_vec()
                },
            })
        }

        fn execute_without_deadline(
            &self,
            command: &crate::targets::CommandSpec,
        ) -> Result<crate::targets::CommandOutput> {
            self.execute(command)
        }

        fn execute_with_stdin_without_deadline(
            &self,
            command: &crate::targets::CommandSpec,
            _input: &mut (dyn std::io::Read + Send),
        ) -> Result<crate::targets::CommandOutput> {
            self.execute(command)
        }

        fn cancellation_requested_without_deadline(&self) -> bool {
            false
        }

        fn pause_deadline(&self) -> crate::targets::DeadlinePause {
            crate::targets::DeadlinePause::none()
        }
    }

    fn record_overdue(session: &str, destination: &str, at: DateTime<Utc>) {
        crate::database::record_host_snapshot_cache_entry(
            session,
            &super::super::host_cache::host_cache_key(&ssh_locator(destination)),
            &format!("sha-{session}"),
            &format!(".cache/mjolnir/snapshot-cache/{session}/archive.hel.zip"),
            &(at - chrono::Duration::hours(49)).to_rfc3339(),
        )
        .unwrap();
    }

    fn rows(session: &str) -> usize {
        crate::database::list_host_snapshot_cache_entries_for_session(session)
            .unwrap()
            .len()
    }

    /// Each host is swept on its own thread: a host that has not answered yet
    /// never holds back one that has.
    #[test]
    fn a_slow_host_does_not_delay_the_sweep_of_a_healthy_one() {
        if !in_isolated_store(
            "a_slow_host_does_not_delay_the_sweep_of_a_healthy_one",
            "MJ_HOST_SWEEP_CONCURRENT_CHILD",
        ) {
            return;
        }
        clear_host_sweep_backoff_for_test();
        let now = Utc::now();
        // The slow host sorts first, so a sequential sweep would wait on it.
        record_overdue("sess-a1", "a.example", now);
        record_overdue("sess-a2", "a.example", now);
        record_overdue("sess-b", "b.example", now);
        let hosts = SweepHosts {
            slow: Some("a.example"),
            healthy: Some("b.example"),
            ..Default::default()
        };
        let swept = sweep_expired_host_snapshot_cache(&hosts, now, &|| false);
        assert_eq!(swept.count, 3);
        assert_eq!(
            (rows("sess-a1"), rows("sess-a2"), rows("sess-b")),
            (0, 0, 0)
        );
    }

    /// An SSH host that fails is asked once per sweep, not once per row, and is
    /// left alone until its backoff expires; backoff grows with each failure
    /// and a success forgets it. Time is the sweep's `now`.
    #[test]
    fn an_unreachable_host_is_skipped_until_its_backoff_expires() {
        if !in_isolated_store(
            "an_unreachable_host_is_skipped_until_its_backoff_expires",
            "MJ_HOST_SWEEP_BACKOFF_CHILD",
        ) {
            return;
        }
        clear_host_sweep_backoff_for_test();
        let start = Utc::now();
        let after = |seconds: i64| start + chrono::Duration::seconds(seconds);
        record_overdue("sess-a1", "a.example", start);
        record_overdue("sess-a2", "a.example", start);
        record_overdue("sess-b", "b.example", start);
        let hosts = SweepHosts::default();
        hosts.failing.lock().unwrap().insert("a.example".into());
        let sweep = |now| sweep_expired_host_snapshot_cache(&hosts, now, &|| false);

        // First sweep: the healthy host is swept; the failing one is asked
        // once, and its second row waits.
        assert_eq!(sweep(start).count, 1);
        assert_eq!(hosts.calls_to("a.example"), 1);
        assert_eq!(
            (rows("sess-a1"), rows("sess-a2"), rows("sess-b")),
            (1, 1, 0)
        );

        // Inside the first backoff (60 s) nothing is asked.
        assert_eq!(sweep(after(59)).count, 0);
        assert_eq!(hosts.calls_to("a.example"), 1);

        // At its expiry the host is tried again, and the next wait doubles.
        assert_eq!(sweep(after(60)).count, 0);
        assert_eq!(hosts.calls_to("a.example"), 2);
        assert_eq!(sweep(after(60 + 119)).count, 0);
        assert_eq!(hosts.calls_to("a.example"), 2);
        assert_eq!(sweep(after(60 + 120)).count, 0);
        assert_eq!(hosts.calls_to("a.example"), 3);

        // Back up: both rows go, and the failures are forgotten, so a later
        // failure starts again at 60 s.
        hosts.failing.lock().unwrap().clear();
        let recovered = after(60 + 120 + 240);
        assert_eq!(sweep(recovered).count, 2);
        assert_eq!((rows("sess-a1"), rows("sess-a2")), (0, 0));
        record_overdue("sess-a3", "a.example", recovered);
        hosts.failing.lock().unwrap().insert("a.example".into());
        let calls = hosts.calls_to("a.example");
        assert_eq!(sweep(recovered).count, 0);
        assert_eq!(hosts.calls_to("a.example"), calls + 1);
        assert_eq!(sweep(recovered + chrono::Duration::seconds(59)).count, 0);
        assert_eq!(hosts.calls_to("a.example"), calls + 1);
        assert_eq!(sweep(recovered + chrono::Duration::seconds(60)).count, 0);
        assert_eq!(hosts.calls_to("a.example"), calls + 2);
    }

    /// Runs the remote command line an SSH command carries in a local `sh`, as
    /// the host's SSH daemon would.
    #[cfg(unix)]
    struct LocalRemoteShell;

    #[cfg(unix)]
    impl CommandExecutor for LocalRemoteShell {
        fn execute(
            &self,
            command: &crate::targets::CommandSpec,
        ) -> Result<crate::targets::CommandOutput> {
            crate::targets::ProcessExecutor.execute(&crate::targets::CommandSpec::new(
                "sh",
                ["-c", command.args.last().unwrap().as_str()],
            ))
        }

        fn execute_without_deadline(
            &self,
            command: &crate::targets::CommandSpec,
        ) -> Result<crate::targets::CommandOutput> {
            self.execute(command)
        }

        fn execute_with_stdin_without_deadline(
            &self,
            command: &crate::targets::CommandSpec,
            _input: &mut (dyn std::io::Read + Send),
        ) -> Result<crate::targets::CommandOutput> {
            self.execute(command)
        }

        fn cancellation_requested_without_deadline(&self) -> bool {
            false
        }

        fn pause_deadline(&self) -> crate::targets::DeadlinePause {
            crate::targets::DeadlinePause::none()
        }
    }

    /// A reachable host whose removal fails for one path (here a non-empty
    /// directory where an archive should be) says nothing about the host's
    /// other copies: they are still removed in the same sweep, and the host is
    /// not backed off.
    #[cfg(unix)]
    #[test]
    fn a_path_specific_removal_failure_on_a_reachable_host_does_not_starve_its_other_rows() {
        if !in_isolated_store(
            "a_path_specific_removal_failure_on_a_reachable_host_does_not_starve_its_other_rows",
            "MJ_HOST_SWEEP_PATH_FAILURE_CHILD",
        ) {
            return;
        }
        clear_host_sweep_backoff_for_test();
        let directory = tempfile::tempdir().unwrap();
        let stuck = directory.path().join("stuck.hel.zip");
        std::fs::create_dir_all(stuck.join("contents")).unwrap();
        let host = super::super::host_cache::host_cache_key(&ssh_locator("a.example"));
        let now = Utc::now();
        let record = |session: &str, path: &Path, age_hours: i64| {
            crate::database::record_host_snapshot_cache_entry(
                session,
                &host,
                "sha",
                &path.to_string_lossy(),
                &(now - chrono::Duration::hours(age_hours)).to_rfc3339(),
            )
            .unwrap();
        };
        let archive = |name: &str| {
            let path = directory.path().join(name);
            std::fs::write(&path, b"archive").unwrap();
            path
        };
        let second = archive("second.hel.zip");
        record("sess-stuck", &stuck, 60);
        record("sess-second", &second, 50);

        let swept = sweep_expired_host_snapshot_cache(&LocalRemoteShell, now, &|| false);
        assert_eq!(swept.count, 1);
        assert_eq!(rows("sess-stuck"), 1, "the failed row is kept for retry");
        assert_eq!(rows("sess-second"), 0, "the next row is not starved");
        assert!(!second.exists());

        // The host answered, so a copy that becomes overdue is swept at once.
        let third = archive("third.hel.zip");
        record("sess-third", &third, 49);
        let swept = sweep_expired_host_snapshot_cache(&LocalRemoteShell, now, &|| false);
        assert_eq!(swept.count, 1);
        assert_eq!((rows("sess-stuck"), rows("sess-third")), (1, 0));
    }
}
