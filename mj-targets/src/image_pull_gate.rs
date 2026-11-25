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
//! holds it exclusively; whoever needs the image waits on it. A launch that
//! only runs an image already on the host holds it shared (see the invariant
//! below).
//!
//! The lock also holds a file lock in the instance's data directory, because
//! `mj doctor --smoke` runs its smoke test in its own
//! process while the daemon may be downloading the same image. Two engines
//! pulling one image at once can fail to extract a layer.

use mj_core::file_lock::FileLock;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use mj_core::config::ImagePullPolicy;
use mj_core::targets::{
    CommandExecutor, CommandOutput, CommandSpec, ImageHost, ProvisionStage, ProvisionStageGuard,
    TargetTemplate,
};

/// How long a waiter sleeps between attempts on the lock.
///
/// A download runs for minutes, so polling this slowly costs nothing. In-process
/// waiters also wake at once when a holder releases, through the condition
/// variable; the interval bounds how long a waiter takes to notice a
/// cancellation or the release of the file lock by another process. Both the
/// refresher and a launch run on blocking threads through the synchronous
/// `CommandExecutor`, and either can be asked to stop while it waits.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

// # What the lock protects
//
// The lock is a reader/writer lock per (host, image). A download (`pull`) and
// the image prune that follows it hold it exclusively. A container `run` that
// needs only the image already on the host holds it shared, so launches of
// the same image proceed together.
//
// The safety invariant: `podman image prune -f` is host-wide and removes only
// untagged images. A tag cannot move, and an image cannot lose its tag, while
// a container `run` holds the shared lock, because a `pull` that retags an
// image and the `prune` that then removes the old, untagged one both hold the
// exclusive lock. A `run` therefore never starts a container from an image
// that a concurrent refresh is replacing under it.
//
// Writers are preferred. A shared request waits while any exclusive request is
// queued, so a continuous stream of launches cannot keep a download from ever
// starting.

/// The lock covering downloads of one image on one host.
///
/// Keyed by host label and image reference, which is what identifies a copy of
/// an image: two targets naming the same image on the same host share one
/// download, and the same image on two hosts does not.
pub fn image_pull_mutex(host: &ImageHost, image: &str) -> Arc<ImagePullLock> {
    static LOCKS: std::sync::OnceLock<Mutex<BTreeMap<String, std::sync::Weak<ImagePullLock>>>> =
        std::sync::OnceLock::new();
    let key = format!("{}|{image}", host.label());
    let mut locks = LOCKS
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    locks.retain(|_, lock| lock.strong_count() > 0);
    // Fixture processes with an isolated store still use the cross-process
    // lock. Other in-process tests must keep the live store untouched.
    let unisolated_fixture = mj_core::test_hooks::test_fixtures_enabled()
        && mj_core::config::ensure_may_control_store(
            &mj_core::config::data_dir(),
            "lock an image download",
        )
        .is_err();
    let file = (!cfg!(test) && !unisolated_fixture).then(|| lock_file_path(&key));
    let slot = locks.entry(key).or_default();
    if let Some(lock) = slot.upgrade() {
        return lock;
    }
    let lock = Arc::new(ImagePullLock::new(file));
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

/// Who holds one in-process [`ImagePullLock`].
#[derive(Debug, Default)]
struct LockState {
    readers: usize,
    writer_active: bool,
    /// Exclusive requests that have not been granted yet. A shared request
    /// does not enter while this is nonzero.
    waiting_writers: usize,
}

/// The right to use or download one image on one host: a reader/writer lock
/// for the threads of this process and, when `file` is set, a file lock for
/// other processes.
#[derive(Debug)]
pub struct ImagePullLock {
    state: Mutex<LockState>,
    changed: Condvar,
    file: Option<PathBuf>,
}

impl ImagePullLock {
    fn new(file: Option<PathBuf>) -> Self {
        Self {
            state: Mutex::new(LockState::default()),
            changed: Condvar::new(),
            file,
        }
    }

    /// A lock that only this process sees.
    #[cfg(test)]
    fn in_process() -> Self {
        Self::new(None)
    }

    /// The state holds counters only, so a holder that panicked left nothing
    /// half-done: its guard's `Drop` already ran during the unwind.
    fn state(&self) -> MutexGuard<'_, LockState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Wait for a change in the state, or the poll interval.
    fn wait<'a>(&self, state: MutexGuard<'a, LockState>) -> MutexGuard<'a, LockState> {
        self.changed
            .wait_timeout(state, POLL_INTERVAL)
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Shared,
    Exclusive,
}

/// A held [`ImagePullLock`]. Dropping it releases the file lock, then the
/// in-process lock.
#[derive(Debug)]
struct Held<'a> {
    lock: &'a ImagePullLock,
    mode: Mode,
    file: Option<FileLock>,
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        // Release the other processes first: a local waiter admitted by the
        // release below would otherwise find the file still locked.
        self.file.take();
        let mut state = self.lock.state();
        match self.mode {
            Mode::Shared => state.readers -= 1,
            Mode::Exclusive => state.writer_active = false,
        }
        drop(state);
        self.lock.changed.notify_all();
    }
}

/// The exclusive right to download one image. Dropping it releases the lock.
#[derive(Debug)]
pub struct ImagePullGuard<'a>(#[allow(dead_code)] Held<'a>);

/// The shared right to run containers from one image. Dropping it releases
/// the lock.
#[derive(Debug)]
pub struct ImageSharedGuard<'a>(#[allow(dead_code)] Held<'a>);

/// Counts one exclusive request as waiting until it is granted or abandoned.
/// The decrement runs on every exit, including a cancelled wait and a panic,
/// so an abandoned request can never keep shared requests out.
struct WaitingWriter<'a>(&'a ImagePullLock);

impl<'a> WaitingWriter<'a> {
    fn register(lock: &'a ImagePullLock) -> Self {
        lock.state().waiting_writers += 1;
        Self(lock)
    }
}

impl Drop for WaitingWriter<'_> {
    fn drop(&mut self) {
        self.0.state().waiting_writers -= 1;
        self.0.changed.notify_all();
    }
}

/// How a file lock attempt ended.
enum FileAttempt {
    Locked(Option<FileLock>),
    Busy,
}

/// Try the file lock once. `Busy` means another process holds a conflicting lock.
fn try_lock_file(path: &std::path::Path, mode: Mode) -> Result<FileAttempt> {
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
    let locked = match mode {
        Mode::Shared => file.try_lock_shared(),
        Mode::Exclusive => file.try_lock(),
    };
    match locked {
        Ok(()) => Ok(FileAttempt::Locked(Some(FileLock::new(file)))),
        Err(std::fs::TryLockError::WouldBlock) => Ok(FileAttempt::Busy),
        Err(std::fs::TryLockError::Error(error)) => {
            Err(error).with_context(|| format!("lock image download lock {}", path.display()))
        }
    }
}

/// Take the lock in `mode`, waiting for whoever holds it.
///
/// The in-process lock is taken first, then the file lock. When another
/// process holds the file lock, the in-process claim is given back while this
/// waiter sleeps, so the other waiters of this process see the same state.
fn hold(
    lock: &ImagePullLock,
    mode: Mode,
    is_cancelled: impl Fn() -> bool,
    on_wait: impl FnOnce(),
) -> Result<Held<'_>> {
    let mut on_wait = Some(on_wait);
    let queued = (mode == Mode::Exclusive).then(|| WaitingWriter::register(lock));
    let mut state = lock.state();
    loop {
        let admitted = match mode {
            Mode::Shared => !state.writer_active && state.waiting_writers == 0,
            Mode::Exclusive => !state.writer_active && state.readers == 0,
        };
        if !admitted {
            if let Some(on_wait) = on_wait.take() {
                // Run the caller's wait callback outside the lock, then
                // re-check before sleeping so a release during `on_wait` is
                // never missed.
                drop(state);
                on_wait();
                if is_cancelled() {
                    bail!("cancelled while waiting for image download");
                }
                state = lock.state();
                continue;
            }
            if is_cancelled() {
                drop(state);
                bail!("cancelled while waiting for image download");
            }
            state = lock.wait(state);
            continue;
        }
        match mode {
            Mode::Shared => state.readers += 1,
            Mode::Exclusive => state.writer_active = true,
        }
        drop(state);
        // From here the claim is released by `Held`'s `Drop`, on every exit.
        let mut held = Held {
            lock,
            mode,
            file: None,
        };
        match &lock.file {
            None => {
                drop(queued);
                return Ok(held);
            }
            Some(path) => match try_lock_file(path, mode)? {
                FileAttempt::Locked(file) => {
                    held.file = file;
                    drop(queued);
                    return Ok(held);
                }
                FileAttempt::Busy => {}
            },
        }
        // Another process holds the file lock.
        drop(held);
        if let Some(on_wait) = on_wait.take() {
            on_wait();
        }
        if is_cancelled() {
            bail!("cancelled while waiting for image download");
        }
        std::thread::sleep(POLL_INTERVAL);
        state = lock.state();
    }
}

/// Take the right to download one image, waiting for whoever holds it.
///
/// `on_wait` runs once, the first time the lock is found busy, so a caller can
/// tell the user it is waiting without saying anything in the common case
/// where nothing is downloading. `is_cancelled` is polled while waiting so a
/// quitting daemon or a cancelled Create does not sit here for minutes.
pub fn hold_image_pull<'a>(
    lock: &'a ImagePullLock,
    is_cancelled: impl Fn() -> bool,
    on_wait: impl FnOnce(),
) -> Result<ImagePullGuard<'a>> {
    hold(lock, Mode::Exclusive, is_cancelled, on_wait).map(ImagePullGuard)
}

/// Take the right to run containers from an image that is already on the
/// host. It waits for a download of the image, and for downloads queued
/// behind it; other shared holders do not delay it. The arguments are those of
/// [`hold_image_pull`].
pub fn hold_image_shared<'a>(
    lock: &'a ImagePullLock,
    is_cancelled: impl Fn() -> bool,
    on_wait: impl FnOnce(),
) -> Result<ImageSharedGuard<'a>> {
    hold(lock, Mode::Shared, is_cancelled, on_wait).map(ImageSharedGuard)
}

/// Run `work` with nothing downloading this target's image underneath it.
///
/// For a container target this waits for a background download of the same
/// image on the same host. Contention is reported as queue time; once this
/// caller owns the lock, `work` runs under the "Pull image" stage. A target
/// that runs no image just runs `work`.
pub fn with_image_ready<T>(
    target: &TargetTemplate,
    executor: &impl CommandExecutor,
    work: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let Some((host, container)) = target.image_host() else {
        return work();
    };
    let image = container.image.clone();
    let lock = image_pull_mutex(&host, &image);
    let mut waiting = None;
    let guard = hold_image_pull(
        &lock,
        || executor.cancellation_requested(),
        || {
            waiting = Some(ProvisionStageGuard::new(
                executor,
                ProvisionStage::WaitingForImagePull,
            ));
            executor.notify_notice(&format!("Waiting for image {image} to finish downloading"));
        },
    )?;
    drop(waiting);
    let pulling = ProvisionStageGuard::new(executor, ProvisionStage::PullingImage);
    let result = work();
    drop(guard);
    drop(pulling);
    result
}

/// Run `work` while this target's image cannot be replaced or pruned
/// underneath it. Like [`with_image_ready`], but a download of the image blocks
/// only the wait, and other `with_image_shared` callers run together. `work`
/// must not download the image: it holds no exclusive right.
pub fn with_image_shared<T>(
    host: &ImageHost,
    image: &str,
    executor: &impl CommandExecutor,
    work: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let lock = image_pull_mutex(host, image);
    let mut waiting = None;
    let guard = hold_image_shared(
        &lock,
        || executor.cancellation_requested(),
        || {
            waiting = Some(ProvisionStageGuard::new(
                executor,
                ProvisionStage::WaitingForImagePull,
            ));
            executor.notify_notice(&format!("Waiting for image {image} to finish downloading"));
        },
    )?;
    drop(waiting);
    let result = work();
    drop(guard);
    result
}

/// How a launch holds the image lock while it creates its container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchImageGate {
    /// The creation may download the image, so nothing else may use it
    /// meanwhile.
    Exclusive,
    /// The creation only runs an image that must already be on the host, so
    /// it shares the image with other launches. The engine refuses an absent
    /// image.
    Shared,
    /// As `Shared`, but an absent image is reported to the launch (see
    /// [`IMAGE_MISSING_STATUS`]) instead of being downloaded by the creation
    /// command. The launch then downloads it exclusively and tries again.
    SharedPullOnMiss,
}

/// The gate a launch of `target` takes. Only Podman targets whose pull policy
/// does not ask for a registry check can use a shared hold.
///
/// Two kinds of run arguments keep a launch on the exclusive path, where
/// `podman run` itself decides whether to download:
/// - An explicit `--pull` overrides the policy the shared path relies on.
/// - A platform selection (`--platform`, `--arch`, `--os`, `--variant`) with
///   the `Missing` policy. `podman image exists` and the image id lookup match
///   a tag without looking at its OS, architecture or variant, and the
///   download on a miss names no platform, so only `podman run`'s own
///   platform-aware pull is correct. `Never` never checks or downloads, so a
///   platform argument does not change what it does.
pub fn launch_image_gate(target: &TargetTemplate) -> LaunchImageGate {
    let Some((host, container)) = target.image_host() else {
        return LaunchImageGate::Exclusive;
    };
    if !matches!(host, ImageHost::LocalPodman | ImageHost::SshPodman(_)) {
        return LaunchImageGate::Exclusive;
    }
    let sets = |flags: &[&str]| {
        container.extra_run_args.iter().any(|arg| {
            flags.iter().any(|flag| {
                arg == flag
                    || arg
                        .strip_prefix(flag)
                        .is_some_and(|rest| rest.starts_with('='))
            })
        })
    };
    if sets(&["--pull"]) {
        return LaunchImageGate::Exclusive;
    }
    match container.pull_policy.at_launch(&container.image) {
        ImagePullPolicy::Missing if sets(&["--platform", "--arch", "--os", "--variant"]) => {
            LaunchImageGate::Exclusive
        }
        ImagePullPolicy::Missing => LaunchImageGate::SharedPullOnMiss,
        ImagePullPolicy::Never => LaunchImageGate::Shared,
        _ => LaunchImageGate::Exclusive,
    }
}

/// The exit status of a creation command that found its image absent from the
/// host and created nothing. Chosen outside the statuses `podman`, `ssh` and
/// `sh` use themselves (0, 1, 125-127, 255 and the signal range).
pub const IMAGE_MISSING_STATUS: i32 = 97;

/// What stderr of a creation command carries with [`IMAGE_MISSING_STATUS`].
pub const IMAGE_MISSING_MARKER: &str = "mj: container image is not on this host";

/// Whether `output` says the creation command found its image absent.
pub fn image_is_missing(output: &CommandOutput) -> bool {
    output.status == IMAGE_MISSING_STATUS
        && String::from_utf8_lossy(&output.stderr).contains(IMAGE_MISSING_MARKER)
}

/// The most downloads one launch makes for one image. The first covers an
/// image that was never there; a second covers one that something outside
/// Mjolnir removed between the download and the retry. A third absence is
/// reported rather than repeated.
const MAX_LAUNCH_PULLS: usize = 2;

/// The result of one attempt to create a launch's container.
pub enum CreationAttempt<T> {
    Created(T),
    /// The creation command found the image absent and created nothing.
    ImageMissing,
}

/// Run a launch's container creation under the lock that `launch_image_gate`
/// names for `target`.
///
/// `attempt` is told which gate applies. With
/// [`LaunchImageGate::SharedPullOnMiss`], it runs under the shared lock. When it reports the image missing, the lock is released, the image is
/// downloaded under the exclusive lock (unless another launch just did), and
/// `attempt` runs again under the shared lock.
pub fn with_image_for_launch<T>(
    target: &TargetTemplate,
    executor: &impl CommandExecutor,
    mut attempt: impl FnMut(LaunchImageGate) -> Result<CreationAttempt<T>>,
) -> Result<T> {
    let gate = launch_image_gate(target);
    let Some((host, container)) = target.image_host() else {
        return created(attempt(gate)?, "");
    };
    let image = container.image.as_str();
    if gate == LaunchImageGate::Exclusive {
        return with_image_ready(target, executor, || created(attempt(gate)?, image));
    }
    for pulls in 0..=MAX_LAUNCH_PULLS {
        match with_image_shared(&host, image, executor, || attempt(gate))? {
            CreationAttempt::Created(created) => return Ok(created),
            CreationAttempt::ImageMissing if gate == LaunchImageGate::Shared => {
                bail!("container image {image} is not on this host");
            }
            CreationAttempt::ImageMissing if pulls == MAX_LAUNCH_PULLS => {
                bail!(
                    "container image {image} was removed from {} again after it was downloaded",
                    host.label()
                );
            }
            CreationAttempt::ImageMissing => pull_missing_image(&host, image, executor)?,
        }
    }
    unreachable!("the last pull attempt returns")
}

fn created<T>(attempt: CreationAttempt<T>, image: &str) -> Result<T> {
    match attempt {
        CreationAttempt::Created(created) => Ok(created),
        CreationAttempt::ImageMissing => {
            bail!("the container creation reported image {image} missing under an exclusive lock")
        }
    }
}

/// Download `image` under the exclusive lock, unless it is already there.
///
/// Another launch may have downloaded it while this one waited, so the image
/// is looked for again before the download.
fn pull_missing_image(
    host: &ImageHost,
    image: &str,
    executor: &impl CommandExecutor,
) -> Result<()> {
    let lock = image_pull_mutex(host, image);
    let mut waiting = None;
    let guard = hold_image_pull(
        &lock,
        || executor.cancellation_requested(),
        || {
            waiting = Some(ProvisionStageGuard::new(
                executor,
                ProvisionStage::WaitingForImagePull,
            ));
            executor.notify_notice(&format!("Waiting for image {image} to finish downloading"));
        },
    )?;
    drop(waiting);
    let _pulling = ProvisionStageGuard::new(executor, ProvisionStage::PullingImage);
    let refresh =
        mj_core::targets::image_refresh(host.clone(), image, None, ImagePullPolicy::Missing)
            .context("an image refresh exists for the Missing policy")?;
    let present = executor
        .execute(&refresh.image_id)
        .with_context(|| format!("look for container image {image}"))?;
    if present.status != 0 {
        run_checked(&refresh.pull, executor)?;
    }
    drop(guard);
    Ok(())
}

fn run_checked(command: &CommandSpec, executor: &impl CommandExecutor) -> Result<()> {
    let output = executor.execute(command)?;
    if output.status != 0 {
        bail!(
            "{} failed with status {}: {}",
            command.purpose,
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    fn wait_until(condition: impl Fn() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !condition() {
            assert!(
                std::time::Instant::now() < deadline,
                "condition was not met"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// A cancelled Create stops waiting instead of sitting behind a
    /// multi-gigabyte download.
    #[test]
    fn a_waiting_create_stops_when_cancelled() {
        let lock = ImagePullLock::in_process();
        let held = hold_image_pull(&lock, || false, || {}).unwrap();

        let error = hold_image_pull(&lock, || true, || {})
            .expect_err("a cancelled wait must not return a lock it never took");
        assert!(
            format!("{error:#}").contains("cancelled while waiting for image download"),
            "{error:#}"
        );
        drop(held);
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

    /// `mj doctor --smoke` runs its smoke test in its own process while the daemon may
    /// be downloading the same image. The file lock makes it wait, as a
    /// thread of the daemon would: two locks with the same file stand for the
    /// two processes.
    // Hard-won: ae03cbf5: doctor/setup smoke raced the daemon image unpack and failed while the same image was downloading.
    #[test]
    fn a_smoke_test_in_another_process_waits_for_the_daemon_s_download() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pulls").join("image.lock");
        let daemon = ImagePullLock::new(Some(path.clone()));
        let setup = ImagePullLock::new(Some(path));
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

    /// Launches share the image, a download waits for them, and a launch that
    /// arrives after the download queued does not jump ahead of it: a steady
    /// stream of launches must not keep a download from ever starting.
    #[test]
    fn a_queued_download_is_not_overtaken_by_later_launches() {
        let lock = ImagePullLock::in_process();
        let writer_entered = AtomicBool::new(false);
        let writer_release = AtomicBool::new(false);
        let third_entered = AtomicBool::new(false);
        let third_release = AtomicBool::new(false);
        std::thread::scope(|scope| {
            // Two launches hold the image together.
            let first = hold_image_shared(&lock, || false, || {}).unwrap();
            let second = hold_image_shared(&lock, || false, || {}).unwrap();
            assert_eq!(lock.state().readers, 2);

            scope.spawn(|| {
                let guard = hold_image_pull(&lock, || false, || {}).unwrap();
                writer_entered.store(true, Ordering::Release);
                wait_until(|| writer_release.load(Ordering::Acquire));
                drop(guard);
            });
            wait_until(|| lock.state().waiting_writers == 1);
            assert!(!writer_entered.load(Ordering::Acquire));

            scope.spawn(|| {
                let guard = hold_image_shared(&lock, || false, || {}).unwrap();
                third_entered.store(true, Ordering::Release);
                wait_until(|| third_release.load(Ordering::Acquire));
                drop(guard);
            });
            std::thread::sleep(Duration::from_millis(400));
            assert!(
                !third_entered.load(Ordering::Acquire),
                "a launch overtook the queued download"
            );

            drop(first);
            std::thread::sleep(Duration::from_millis(400));
            assert!(
                !writer_entered.load(Ordering::Acquire),
                "the download started while a launch still held the image"
            );
            drop(second);
            wait_until(|| writer_entered.load(Ordering::Acquire));
            std::thread::sleep(Duration::from_millis(400));
            assert!(
                !third_entered.load(Ordering::Acquire),
                "a launch ran during the download"
            );
            writer_release.store(true, Ordering::Release);
            wait_until(|| third_entered.load(Ordering::Acquire));
            third_release.store(true, Ordering::Release);
        });
        let state = lock.state();
        assert_eq!(
            (state.readers, state.writer_active, state.waiting_writers),
            (0, false, 0)
        );
    }

    /// A download that is cancelled while queued must not keep later launches
    /// waiting for it.
    #[test]
    fn a_cancelled_queued_download_lets_waiting_launches_in() {
        let lock = ImagePullLock::in_process();
        let cancelled = AtomicBool::new(false);
        let reader_entered = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let held = hold_image_shared(&lock, || false, || {}).unwrap();
            let writer = scope.spawn(|| {
                hold_image_pull(&lock, || cancelled.load(Ordering::Acquire), || {})
                    .map(|_| ())
                    .unwrap_err()
            });
            wait_until(|| lock.state().waiting_writers == 1);
            scope.spawn(|| {
                let guard = hold_image_shared(&lock, || false, || {}).unwrap();
                reader_entered.store(true, Ordering::Release);
                drop(guard);
            });
            std::thread::sleep(Duration::from_millis(400));
            assert!(!reader_entered.load(Ordering::Acquire));

            cancelled.store(true, Ordering::Release);
            let error = writer.join().unwrap();
            assert!(format!("{error:#}").contains("cancelled while waiting"));
            wait_until(|| reader_entered.load(Ordering::Acquire));
            drop(held);
        });
        assert_eq!(lock.state().waiting_writers, 0);
    }

    /// Launches in separate processes share the image through the file lock
    /// too, and a download in another process still excludes them.
    #[test]
    fn launches_in_two_processes_share_the_image_and_wait_for_a_download() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pulls").join("image.lock");
        let first = ImagePullLock::new(Some(path.clone()));
        let second = ImagePullLock::new(Some(path));
        let one = hold_image_shared(&first, || false, || {}).unwrap();
        let two = hold_image_shared(&second, || false, || {}).unwrap();
        let waits = AtomicUsize::new(0);
        let error = hold_image_pull(
            &first,
            || true,
            || {
                waits.fetch_add(1, Ordering::Release);
            },
        )
        .map(|_| ())
        .unwrap_err();
        assert!(format!("{error:#}").contains("cancelled while waiting"));
        assert_eq!(waits.load(Ordering::Acquire), 1);
        drop((one, two));
        let pull = hold_image_pull(&first, || false, || {}).unwrap();
        assert!(hold_image_shared(&second, || true, || {}).is_err());
        drop(pull);
    }

    /// Records the engine commands of the download step and answers whether
    /// the image is present, while exposing the lock's state to the test.
    struct EngineExecutor {
        present: bool,
        host: ImageHost,
        image: &'static str,
        commands: Mutex<Vec<String>>,
        /// The lock state the download command found, as (readers, writer).
        state_during_pull: Mutex<Option<(usize, bool)>>,
    }

    impl CommandExecutor for EngineExecutor {
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
            self.commands.lock().unwrap().push(command.args.join(" "));
            let pulling = command.args.first().is_some_and(|verb| verb == "pull");
            if pulling {
                let lock = image_pull_mutex(&self.host, self.image);
                let state = lock.state();
                *self.state_during_pull.lock().unwrap() =
                    Some((state.readers, state.writer_active));
            }
            let absent = !pulling && !self.present;
            Ok(CommandOutput {
                status: i32::from(absent),
                stdout: b"sha256:image\n".to_vec(),
                stderr: Vec::new(),
            })
        }
    }

    fn podman_target(image: &str) -> TargetTemplate {
        TargetTemplate::LocalPodman(mj_core::targets::ContainerTemplate {
            build_cache: None,
            image: image.to_owned(),
            pull_policy: ImagePullPolicy::Missing,
            extra_run_args: Vec::new(),
            workspace_storage: Default::default(),
        })
    }

    /// A launch whose image is absent downloads it once, under the exclusive
    /// lock with no launch running, and runs again under the shared lock; a
    /// launch whose image another launch downloaded meanwhile downloads
    /// nothing.
    #[test]
    fn a_launch_that_finds_its_image_absent_downloads_it_exclusively_and_runs_again() {
        const IMAGE: &str = "example.test/miss-then-pull:1";
        for already_downloaded in [false, true] {
            let executor = EngineExecutor {
                present: already_downloaded,
                host: ImageHost::LocalPodman,
                image: IMAGE,
                commands: Mutex::new(Vec::new()),
                state_during_pull: Mutex::new(None),
            };
            let lock = image_pull_mutex(&ImageHost::LocalPodman, IMAGE);
            let mut attempts = 0;
            let created = with_image_for_launch(&podman_target(IMAGE), &executor, |gate| {
                assert_eq!(gate, LaunchImageGate::SharedPullOnMiss);
                let state = lock.state();
                assert_eq!((state.readers, state.writer_active), (1, false));
                drop(state);
                attempts += 1;
                Ok(if attempts == 1 {
                    CreationAttempt::ImageMissing
                } else {
                    CreationAttempt::Created("container")
                })
            })
            .unwrap();
            assert_eq!(created, "container");
            assert_eq!(attempts, 2);
            let commands = executor.commands.lock().unwrap().clone();
            let pulls = commands
                .iter()
                .filter(|line| line.starts_with("pull"))
                .count();
            assert_eq!(pulls, usize::from(!already_downloaded), "{commands:?}");
            assert_eq!(
                *executor.state_during_pull.lock().unwrap(),
                (!already_downloaded).then_some((0, true)),
                "the download ran exclusively"
            );
            let state = lock.state();
            assert_eq!((state.readers, state.writer_active), (0, false));
        }
    }

    /// An image that something outside Mjolnir keeps removing is reported
    /// after a bounded number of downloads, not retried forever.
    #[test]
    fn a_launch_stops_downloading_an_image_that_keeps_disappearing() {
        const IMAGE: &str = "example.test/keeps-disappearing:1";
        let executor = EngineExecutor {
            present: false,
            host: ImageHost::LocalPodman,
            image: IMAGE,
            commands: Mutex::new(Vec::new()),
            state_during_pull: Mutex::new(None),
        };
        let mut attempts = 0;
        let error = with_image_for_launch(&podman_target(IMAGE), &executor, |_| {
            attempts += 1;
            Ok(CreationAttempt::<()>::ImageMissing)
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("was removed"), "{error:#}");
        assert_eq!(attempts, MAX_LAUNCH_PULLS + 1);
        let commands = executor.commands.lock().unwrap().clone();
        assert_eq!(
            commands
                .iter()
                .filter(|line| line.starts_with("pull"))
                .count(),
            MAX_LAUNCH_PULLS,
            "{commands:?}"
        );
    }

    /// Arguments that change which image `podman run` wants, or how it pulls,
    /// keep the launch on the path where `podman run` decides.
    #[test]
    fn platform_and_pull_arguments_keep_a_launch_on_the_exclusive_path() {
        let gate = |policy: ImagePullPolicy, args: &[&str]| {
            launch_image_gate(&TargetTemplate::LocalPodman(
                mj_core::targets::ContainerTemplate {
                    build_cache: None,
                    image: "example.test/gate:1".to_owned(),
                    pull_policy: policy,
                    extra_run_args: args.iter().map(|arg| (*arg).to_owned()).collect(),
                    workspace_storage: Default::default(),
                },
            ))
        };
        use LaunchImageGate::{Exclusive, Shared, SharedPullOnMiss};
        assert_eq!(gate(ImagePullPolicy::Missing, &[]), SharedPullOnMiss);
        assert_eq!(gate(ImagePullPolicy::Never, &[]), Shared);
        for args in [
            &["--platform=linux/amd64"][..],
            &["--platform", "linux/amd64"],
            &["--arch=amd64"],
            &["--os", "linux"],
            &["--variant=v8"],
        ] {
            assert_eq!(gate(ImagePullPolicy::Missing, args), Exclusive, "{args:?}");
            assert_eq!(gate(ImagePullPolicy::Never, args), Shared, "{args:?}");
        }
        for args in [&["--pull=always"][..], &["--pull", "never"]] {
            for policy in [ImagePullPolicy::Missing, ImagePullPolicy::Never] {
                assert_eq!(gate(policy, args), Exclusive, "{policy:?} {args:?}");
            }
        }
        // A flag that only shares a prefix is not the flag.
        assert_eq!(
            gate(
                ImagePullPolicy::Missing,
                &["--platformish=1", "--pull-extra"]
            ),
            SharedPullOnMiss
        );
    }
}
