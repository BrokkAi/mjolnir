//! The files a checkpoint writes in its worker root, and the sweep that
//! removes them once no checkpoint can own them.

use std::ffi::OsStr;
use std::path::Path;
use std::time::SystemTime;

use anyhow::Result;
use mj_checkpoint::checkpoint::CHECKPOINT_CAPTURE_TEMPORARY_PREFIX;

use crate::session_manager::new_command_id;
use crate::targets::CommandExecutor;

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
    let swept = sweep_checkpoint_leftovers_in(
        &mj_core::config::data_dir().join("workers"),
        started,
        &running,
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

        for path in [&stage, &capture, &archive, &busy_stage] {
            set_modified(path, before);
        }
        for path in &kept {
            set_modified(path, before);
        }
        set_modified(&current, after);

        let swept = sweep_checkpoint_leftovers_in(&workers, started, &running, &|| false);

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
            .chain([current, busy_stage, pinned, outside.join("keep")])
            .collect();
        for path in &expected_kept {
            assert!(path.exists(), "{} was removed", path.display());
        }
        assert!(std::fs::symlink_metadata(&link).is_ok());
    }
}
