//! Coordination between the daemon's background image download and a session
//! launch that needs the same image.
//!
//! The daemon downloads every configured container image shortly after it
//! starts. A person who creates a session during that download must not start
//! a second download of the same image: the two would compete for the same
//! bandwidth and the same layer store. Instead the launch waits for the one
//! already running, and then finds the image present.
//!
//! There is one lock per (host, image) pair. Whoever is allowed to download
//! holds it; whoever needs the image waits on it.
//!
//! The lock also holds a file lock in the instance's data directory, because
//! `mj setup` and `mj doctor --smoke` run their smoke test in their own
//! process while the daemon may be downloading the same image. Two engines
//! pulling one image at once can fail to extract a layer.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use mj_core::targets::{
    CommandExecutor, ImageHost, ProvisionStage, ProvisionStageGuard, TargetTemplate,
};

/// How long a waiter sleeps between attempts on the lock.
///
/// A download runs for minutes, so polling this slowly costs nothing and keeps
/// the wait cancellable without a condition variable: both the refresher and a
/// launch run on blocking threads through the synchronous `CommandExecutor`,
/// and either can be asked to stop while it waits.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// The lock covering downloads of one image on one host.
///
/// Keyed by host label and image reference, which is what identifies a copy of
/// an image: two targets naming the same image on the same host share one
/// download, and the same image on two hosts does not.
pub(crate) fn image_pull_mutex(host: &ImageHost, image: &str) -> Arc<ImagePullLock> {
    static LOCKS: std::sync::OnceLock<Mutex<BTreeMap<String, std::sync::Weak<ImagePullLock>>>> =
        std::sync::OnceLock::new();
    let key = format!("{}|{image}", host.label());
    let mut locks = LOCKS
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    locks.retain(|_, lock| lock.strong_count() > 0);
    // Unit tests share one process and must not write to the developer's
    // data directory; the file lock has a test of its own.
    let file = (!cfg!(test)).then(|| lock_file_path(&key));
    let slot = locks.entry(key).or_default();
    if let Some(lock) = slot.upgrade() {
        return lock;
    }
    let lock = Arc::new(ImagePullLock {
        local: Mutex::new(()),
        file,
    });
    *slot = Arc::downgrade(&lock);
    lock
}

/// The file other Mjolnir processes of this instance lock for the same
/// (host, image) pair. The name is a hash, because an image reference and a
/// host label can hold characters a file name cannot.
fn lock_file_path(key: &str) -> PathBuf {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(key.as_bytes());
    let name: String = digest
        .iter()
        .take(12)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    mj_core::config::data_dir()
        .join("image-pulls")
        .join(format!("{name}.lock"))
}

/// The right to download one image on one host: a lock for the threads of
/// this process and, when `file` is set, a file lock for other processes.
pub(crate) struct ImagePullLock {
    local: Mutex<()>,
    file: Option<PathBuf>,
}

impl ImagePullLock {
    /// A lock that only this process sees.
    #[cfg(test)]
    fn in_process() -> Self {
        Self {
            local: Mutex::new(()),
            file: None,
        }
    }
}

/// A held [`ImagePullLock`]. Dropping it releases both locks.
#[derive(Debug)]
pub(crate) struct ImagePullGuard<'a> {
    _local: MutexGuard<'a, ()>,
    _file: Option<File>,
}

/// Try the file lock once. `None` means another process holds it.
fn try_lock_file(path: &std::path::Path) -> Result<Option<File>> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!("create image download lock directory {}", parent.display())
        })?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .with_context(|| format!("open image download lock {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(error)) => {
            Err(error).with_context(|| format!("lock image download lock {}", path.display()))
        }
    }
}

/// Take the right to download one image, waiting for whoever holds it.
///
/// `on_wait` runs once, the first time the lock is found busy, so a caller can
/// tell the user it is waiting without saying anything in the common case
/// where nothing is downloading. `is_cancelled` is polled while waiting so a
/// quitting daemon or a cancelled Create does not sit here for minutes.
pub(crate) fn hold_image_pull<'a>(
    lock: &'a ImagePullLock,
    is_cancelled: impl Fn() -> bool,
    on_wait: impl FnOnce(),
) -> Result<ImagePullGuard<'a>> {
    let mut on_wait = Some(on_wait);
    loop {
        let local = match lock.local.try_lock() {
            Ok(guard) => Some(guard),
            // A holder that panicked left no state behind: the lock guards
            // nothing but the right to run a download.
            Err(std::sync::TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => None,
        };
        if let Some(local) = local {
            let file = match &lock.file {
                Some(path) => try_lock_file(path)?.map(Some),
                None => Some(None),
            };
            if let Some(file) = file {
                return Ok(ImagePullGuard {
                    _local: local,
                    _file: file,
                });
            }
            // Another process is downloading; let this process's other
            // waiters see the same state while this one sleeps.
            drop(local);
        }
        if let Some(on_wait) = on_wait.take() {
            on_wait();
        }
        if is_cancelled() {
            bail!("cancelled while waiting for image download");
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Run `work` with nothing downloading this target's image underneath it.
///
/// For a container target this waits for a background download of the same
/// image on the same host, and reports the wait as the "Pull image" stage with
/// a notice saying what it is waiting for. Nothing extra is reported in the
/// ordinary case where no download is running, and a target that runs no image
/// just runs `work`.
pub(crate) fn with_image_ready<T>(
    target: &TargetTemplate,
    executor: &impl CommandExecutor,
    work: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let Some((host, container)) = target.image_host() else {
        return work();
    };
    let image = container.image.clone();
    let lock = image_pull_mutex(&host, &image);
    // The stage guard is created inside `on_wait`, so it exists only on the
    // waiting path, and dropped as soon as the wait is over: the stage
    // describes the wait, not the work that follows it.
    let mut waiting = None;
    let guard = hold_image_pull(
        &lock,
        || executor.cancellation_requested(),
        || {
            waiting = Some(ProvisionStageGuard::new(
                executor,
                ProvisionStage::PullingImage,
            ));
            executor.notify_notice(&format!("Waiting for image {image} to finish downloading"));
        },
    )?;
    drop(waiting);
    let result = work();
    drop(guard);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// A Create issued while the daemon is downloading the image waits for
    /// that download instead of starting a second one, and says so once.
    #[test]
    fn a_create_waits_for_the_in_flight_pull_of_its_image() {
        let lock = Arc::new(ImagePullLock::in_process());
        let released = Arc::new(AtomicBool::new(false));
        let waits = Arc::new(AtomicUsize::new(0));

        let downloader = {
            let lock = lock.clone();
            let released = released.clone();
            std::thread::spawn(move || {
                let guard = lock.local.lock().unwrap();
                // Long enough that the waiter has to block on it.
                std::thread::sleep(Duration::from_millis(400));
                released.store(true, Ordering::Release);
                drop(guard);
            })
        };

        // Make sure the downloader holds the lock before the launch tries.
        while lock.local.try_lock().is_ok() {
            std::thread::sleep(Duration::from_millis(10));
        }

        let guard = hold_image_pull(
            &lock,
            || false,
            || {
                waits.fetch_add(1, Ordering::Release);
            },
        )
        .expect("the launch takes the lock once the download finishes");
        assert!(
            released.load(Ordering::Acquire),
            "the launch proceeded while the download still held the lock"
        );
        assert_eq!(
            waits.load(Ordering::Acquire),
            1,
            "the wait should be announced exactly once"
        );
        drop(guard);
        downloader.join().expect("the download thread finishes");
    }

    /// A cancelled Create stops waiting instead of sitting behind a
    /// multi-gigabyte download.
    #[test]
    fn a_waiting_create_stops_when_cancelled() {
        let lock = ImagePullLock::in_process();
        let held = lock.local.lock().unwrap();

        let error = hold_image_pull(&lock, || true, || {})
            .expect_err("a cancelled wait must not return a lock it never took");
        assert!(
            format!("{error:#}").contains("cancelled while waiting for image download"),
            "{error:#}"
        );
        drop(held);
    }

    /// `mj setup` runs its smoke test in its own process while the daemon may
    /// be downloading the same image. The file lock makes it wait, as a
    /// thread of the daemon would: two locks with the same file stand for the
    /// two processes.
    #[test]
    fn a_smoke_test_in_another_process_waits_for_the_daemon_s_download() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pulls").join("image.lock");
        let daemon = ImagePullLock {
            local: Mutex::new(()),
            file: Some(path.clone()),
        };
        let setup = ImagePullLock {
            local: Mutex::new(()),
            file: Some(path),
        };
        let holding = AtomicBool::new(false);
        let released = AtomicBool::new(false);
        let waits = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let held = hold_image_pull(&daemon, || false, || {}).unwrap();
                holding.store(true, Ordering::Release);
                std::thread::sleep(Duration::from_millis(400));
                released.store(true, Ordering::Release);
                drop(held);
            });
            while !holding.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(10));
            }
            let guard = hold_image_pull(
                &setup,
                || false,
                || {
                    waits.fetch_add(1, Ordering::Release);
                },
            )
            .unwrap();
            assert!(released.load(Ordering::Acquire));
            drop(guard);
        });
        assert_eq!(waits.load(Ordering::Acquire), 1);
    }

    /// The same image on the same host is one download; a different host or a
    /// different image is not.
    #[test]
    fn the_pull_lock_is_shared_per_host_and_image() {
        let first = image_pull_mutex(&ImageHost::LocalPodman, "ghcr.io/example/dev:latest");
        let again = image_pull_mutex(&ImageHost::LocalPodman, "ghcr.io/example/dev:latest");
        assert!(Arc::ptr_eq(&first, &again));

        let other_image = image_pull_mutex(&ImageHost::LocalPodman, "ghcr.io/example/other:latest");
        assert!(!Arc::ptr_eq(&first, &other_image));

        let other_host = image_pull_mutex(&ImageHost::LocalDocker, "ghcr.io/example/dev:latest");
        assert!(!Arc::ptr_eq(&first, &other_host));
    }
}
