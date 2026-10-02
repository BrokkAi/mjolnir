//! The one owner of "is this filesystem on a target full".
//!
//! The daemon's capacity service measures, on its usual cadence, each
//! filesystem Mjolnir writes to on each host: worker roots, staged profile
//! homes, project directories and managed clones, caches, container storage
//! and `/tmp`, one record per filesystem. A target command that fails with
//! "No space left on device" marks the filesystem of the path it names, or,
//! when it names none, holds every filesystem on the host until the new
//! measurement it asks for says which one is full. Everything else asks this
//! board: a write checks the filesystem it lands on, recovery waits on the
//! filesystem it needs, and the runtime feed, API and web viewer publish
//! what it says.
//!
//! Hosts are named the way [`mj_core::targets::storage`] names them: `local`
//! for the daemon's machine, otherwise the SSH host without its user.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use anyhow::Result;
use mj_core::refusal::Refusal;
use mj_core::targets::TargetLocator;
use mj_core::targets::storage::{
    ContainerStorage, FilesystemSpace, HostStorageSample, LOCAL_STORAGE_HOST, TargetStorageView,
    local_home_path, storage_host_of_destination,
};

/// A failed write keeps its filesystem full until a measurement taken this
/// long after it says otherwise. A probe already in flight when the write
/// failed may describe the disk from before the failure.
const NO_SPACE_HOLD_SECONDS: u64 = 15;

#[derive(Default)]
struct HostEntry {
    home: Option<String>,
    filesystems: Vec<FilesystemSpace>,
    sampled_at: Option<u64>,
    /// Failures attributed to one filesystem, by mount point.
    no_space: BTreeMap<String, (u64, String)>,
    /// A failure that named no path this board could place.
    unattributed: Option<(u64, String)>,
}

impl HostEntry {
    fn view(&self, host: &str) -> TargetStorageView {
        TargetStorageView::evaluate(
            host,
            self.home.clone(),
            &self.filesystems,
            self.sampled_at,
            |space| {
                self.no_space
                    .get(&space.mount)
                    .map(|(_, detail)| detail.clone())
            },
            self.unattributed.as_ref().map(|(_, detail)| detail.clone()),
        )
    }

    fn superseded(&self, at: u64) -> bool {
        self.sampled_at
            .is_some_and(|sampled| sampled >= at.saturating_add(NO_SPACE_HOLD_SECONDS))
    }
}

struct Board {
    hosts: Mutex<BTreeMap<String, HostEntry>>,
    refresh: Mutex<Option<tokio::sync::mpsc::Sender<()>>>,
    /// Advances whenever a filesystem's verdict or free space changes.
    generation: tokio::sync::watch::Sender<u64>,
}

impl Default for Board {
    fn default() -> Self {
        Self {
            hosts: Mutex::default(),
            refresh: Mutex::default(),
            generation: tokio::sync::watch::channel(0).0,
        }
    }
}

impl Board {
    fn hosts(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, HostEntry>> {
        self.hosts.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn changed(&self) {
        self.generation.send_modify(|generation| *generation += 1);
    }

    fn request_refresh(&self) {
        if let Some(refresh) = self
            .refresh
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            // A full channel already holds a refresh request.
            let _ = refresh.try_send(());
        }
    }
}

/// One board per data directory, so isolated tests in one process do not
/// share hosts. A daemon has exactly one.
fn board() -> Arc<Board> {
    type Boards = Mutex<BTreeMap<PathBuf, Arc<Board>>>;
    static BOARDS: OnceLock<Boards> = OnceLock::new();
    BOARDS
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .entry(mj_core::config::data_dir())
        .or_default()
        .clone()
}

fn now_epoch_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Make this process the owner that executors report full disks to.
pub(crate) fn install_no_space_observer() {
    mj_core::targets::storage::set_no_space_observer(|host, detail| {
        observe_no_space(host, detail);
    });
}

/// Let a no-space observation wake the capacity service for a new reading.
pub(crate) fn attach_refresh(refresh: tokio::sync::mpsc::Sender<()>) {
    *board()
        .refresh
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some(refresh);
}

/// Install a measurement. It replaces each sampled host's filesystems, and
/// supersedes the full-disk failures it was taken after.
pub(crate) fn record_samples(samples: &[HostStorageSample], sampled_at: u64) {
    if samples.is_empty() {
        return;
    }
    let board = board();
    let mut changed = false;
    {
        let mut hosts = board.hosts();
        for sample in samples {
            let entry = hosts.entry(sample.host.clone()).or_default();
            let before = entry.view(&sample.host);
            entry.home = sample.home.clone().or(entry.home.take());
            entry.filesystems = sample.filesystems.clone();
            entry.sampled_at = Some(sampled_at);
            let hold = |at: &u64| sampled_at < at.saturating_add(NO_SPACE_HOLD_SECONDS);
            entry.no_space.retain(|_, (at, _)| hold(at));
            if entry.unattributed.as_ref().is_some_and(|(at, _)| !hold(at)) {
                entry.unattributed = None;
            }
            let after = entry.view(&sample.host);
            changed |= before.filesystems != after.filesystems
                || before.unattributed_no_space != after.unattributed_no_space;
        }
    }
    if changed {
        board.changed();
    }
}

/// A write on `host` failed because a disk is full. The filesystem of the
/// path the failure names is marked; a failure that names no path holds the
/// whole host until the measurement it asks for.
pub(crate) fn observe_no_space(host: &str, detail: &str) {
    let path = mj_core::targets::storage::no_space_path(detail);
    observe_no_space_at(host, path.as_deref(), detail, now_epoch_seconds());
}

/// A write to `path` on `host` failed for lack of space at `at` (epoch
/// seconds). A measurement taken after that already says what the disk holds
/// now, so an old failure, such as a dead worker's exit record, does not
/// override it.
pub(crate) fn observe_no_space_at(host: &str, path: Option<&str>, detail: &str, at: u64) {
    let board = board();
    let changed = {
        let mut hosts = board.hosts();
        let entry = hosts.entry(host.to_owned()).or_default();
        if entry.superseded(at) {
            tracing::debug!(%host, %detail, "a later measurement supersedes this full-disk failure");
            return;
        }
        let before = entry.view(host);
        let mount = path.and_then(|path| {
            before
                .filesystem_for(path)
                .map(|filesystem| filesystem.space.mount.clone())
        });
        match mount {
            Some(mount) => {
                entry.no_space.insert(mount, (at, detail.to_owned()));
            }
            None => entry.unattributed = Some((at, detail.to_owned())),
        }
        before != entry.view(host)
    };
    tracing::warn!(%host, ?path, %detail, "a write failed because the target's disk is full");
    if changed {
        board.changed();
    }
    board.request_refresh();
}

/// What the board says about `host`, if it has heard anything.
pub fn view(host: &str) -> Option<TargetStorageView> {
    board().hosts().get(host).map(|entry| entry.view(host))
}

/// Every host the board knows, for publication.
pub fn views() -> Vec<TargetStorageView> {
    board()
        .hosts()
        .iter()
        .map(|(host, entry)| entry.view(host))
        .collect()
}

/// Wakes whenever a filesystem's verdict or free space changes.
pub fn subscribe() -> tokio::sync::watch::Receiver<u64> {
    board().generation.subscribe()
}

/// The host a target writes to.
pub fn storage_host(locator: &TargetLocator) -> String {
    match locator {
        TargetLocator::LocalBare { .. }
        | TargetLocator::LocalPodman { .. }
        | TargetLocator::LocalDocker { .. }
        | TargetLocator::AppleContainer { .. } => LOCAL_STORAGE_HOST.to_owned(),
        TargetLocator::AwsEc2 { ssh, .. }
        | TargetLocator::SshBare { ssh, .. }
        | TargetLocator::SshPodman { ssh, .. }
        | TargetLocator::SshDocker { ssh, .. } => {
            storage_host_of_destination(Some(&ssh.destination))
        }
    }
}

/// Where on its host a write to `path` on `locator` lands. A path inside a
/// container lands in the engine's storage; Apple's runtime keeps its
/// storage in a VM image this board does not measure.
fn host_path(locator: &TargetLocator, path: &str) -> Option<String> {
    match locator {
        TargetLocator::LocalBare { .. }
        | TargetLocator::AwsEc2 { .. }
        | TargetLocator::SshBare { .. } => Some(path.to_owned()),
        TargetLocator::LocalPodman { .. } => Some(local_home_path(ContainerStorage::Podman.path())),
        TargetLocator::SshPodman { .. } => Some(ContainerStorage::Podman.path().to_owned()),
        TargetLocator::LocalDocker { .. } | TargetLocator::SshDocker { .. } => {
            Some(ContainerStorage::Docker.path().to_owned())
        }
        TargetLocator::AppleContainer { .. } => None,
    }
}

/// Refuse a write of `bytes` (zero when the size is unknown) to `path` on
/// `locator` that would leave its filesystem with less than the reserve. A
/// filesystem the board has not measured is not refused: a write that then
/// fails is still classified.
pub fn ensure_room(locator: &TargetLocator, path: &str, bytes: u64, what: &str) -> Result<()> {
    ensure_room_for(locator, [path], || Ok(bytes), what)
}

/// [`ensure_room`] for a write that spans several directories, such as a
/// harness install that fills its cache and the staged profile home. The
/// size is measured only when a filesystem the write lands on is known, so
/// an unmeasured target costs nothing.
pub fn ensure_room_for<'a>(
    locator: &TargetLocator,
    paths: impl IntoIterator<Item = &'a str>,
    bytes: impl FnOnce() -> Result<u64>,
    what: &str,
) -> Result<()> {
    let host = storage_host(locator);
    let Some(view) = view(&host) else {
        return Ok(());
    };
    let paths = paths
        .into_iter()
        .filter_map(|path| host_path(locator, path))
        .filter(|path| view.filesystem_for(path).is_some() || view.unattributed_no_space.is_some())
        .collect::<Vec<_>>();
    if paths.is_empty() {
        return Ok(());
    }
    let bytes = bytes()?;
    for path in &paths {
        if let Some(refusal) = view.refuse_write(path, bytes, what) {
            tracing::warn!(%host, %path, bytes, what, "refused a write to a full filesystem: {refusal}");
            return Err(Refusal::precondition(refusal)
                .with_code(STORAGE_FULL_CODE)
                .into());
        }
    }
    Ok(())
}

/// Where the worker installs managed harnesses on a target: its XDG cache.
pub fn harness_cache_path(locator: &TargetLocator) -> String {
    const HARNESS_CACHE: &str = ".cache/mjolnir/harnesses";
    match locator {
        TargetLocator::LocalBare { .. } => local_home_path(HARNESS_CACHE),
        _ => HARNESS_CACHE.to_owned(),
    }
}

/// The refusal code carried by [`ensure_room`] refusals.
pub const STORAGE_FULL_CODE: &str = "target_storage_full";

/// Whether `error` is a write refused or failed because a disk is full.
pub fn is_storage_full(error: &anyhow::Error) -> bool {
    Refusal::of(error).is_some_and(|refusal| refusal.code() == Some(STORAGE_FULL_CODE))
        || mj_core::targets::storage::reports_no_space(&format!("{error:#}"))
}

/// The host and sentence for a full filesystem one of `paths` on `host`
/// lands on.
pub fn full_problem<'a>(host: &str, paths: impl IntoIterator<Item = &'a str>) -> Option<String> {
    view(host)?.problem_for(paths)
}

/// The host and worker root of a session's recorded target: where its
/// worker keeps its journal, and where a restart stages and writes.
pub fn session_worker_root(
    locator: &mj_core::state::TargetLocator,
    session_id: &str,
) -> (String, String) {
    let paths = mj_core::targets::storage::session_storage_paths(locator, session_id, None);
    (paths.host, paths.worker_root)
}

/// The host and sentence while the filesystem a session's worker root is on
/// is full: recovery waits on exactly this.
pub fn worker_root_problem(
    locator: &mj_core::state::TargetLocator,
    session_id: &str,
) -> Option<(String, String)> {
    let (host, worker_root) = session_worker_root(locator, session_id);
    let problem = full_problem(&host, [worker_root.as_str()])?;
    Some((host, problem))
}

/// The sentence a session shows instead of "unreachable" while one of its
/// own filesystems is full: its worker root, workspace, profile home or
/// `/tmp`.
pub fn session_problem(session: &mj_core::state::SessionRecord) -> Option<String> {
    located_session_problem(
        session.target.as_ref()?,
        &session.id,
        session.project_directory.as_deref(),
    )
}

fn located_session_problem(
    locator: &mj_core::state::TargetLocator,
    session_id: &str,
    project_directory: Option<&std::path::Path>,
) -> Option<String> {
    let paths =
        mj_core::targets::storage::session_storage_paths(locator, session_id, project_directory);
    full_problem(&paths.host, paths.all())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use mj_core::targets::storage::WRITE_RESERVE_BYTES;

    /// One host with `/` holding the worker roots and `~/Projects` on a
    /// filesystem of its own, as on precision-3260.
    pub(crate) fn sample(host: &str, root_free: u64, projects_free: u64) -> HostStorageSample {
        HostStorageSample {
            host: host.to_owned(),
            home: Some("/home/jonathan".into()),
            filesystems: vec![
                FilesystemSpace {
                    mount: "/".into(),
                    available_bytes: root_free,
                    total_bytes: 500 << 30,
                    reserved_bytes: 25 << 30,
                    paths: vec![
                        "/home/jonathan/.local/share/hel/workers".into(),
                        "/home/jonathan/.local/share/hel/profiles".into(),
                        "/home/jonathan/.cache/mjolnir".into(),
                        "/tmp".into(),
                    ],
                },
                FilesystemSpace {
                    mount: "/home/jonathan/Projects".into(),
                    available_bytes: projects_free,
                    total_bytes: 1000 << 30,
                    reserved_bytes: 0,
                    paths: vec!["/home/jonathan/Projects".into()],
                },
            ],
        }
    }

    pub(crate) fn ssh_bare(host: &str) -> TargetLocator {
        TargetLocator::SshBare {
            ssh: mj_core::targets::SshTarget {
                destination: format!("jonathan@{host}"),
                ssh_args: Vec::new(),
            },
            workspace: "/home/jonathan/Projects/app/.mj/clones/abc".into(),
            worker_id: None,
        }
    }

    const WORKER: &str = ".local/share/hel/workers/abc/hel.prepared-x.next";
    const CLONE: &str = "/home/jonathan/Projects/app/.mj/clones/abc/restore.hel.zip";

    // The board is shared by every test in this process, so each test names
    // its own host.
    #[test]
    fn each_write_is_judged_by_the_filesystem_it_lands_on() {
        let host = "precision-3260";
        let target = ssh_bare(host);
        // Not measured yet: unknown is not a reason to refuse.
        ensure_room(&target, WORKER, 100 << 20, "stage the worker").unwrap();

        // A full ~/Projects refuses a restore into a clone there, but does not
        // block worker staging on a healthy /.
        record_samples(&[sample(host, 40 << 30, 0)], now_epoch_seconds());
        ensure_room(&target, WORKER, 100 << 20, "stage the worker").unwrap();
        let error = ensure_room(&target, CLONE, 100 << 20, "restore the checkpoint").unwrap_err();
        assert!(is_storage_full(&error));
        let text = format!("{error:#}");
        assert!(
            text.contains("precision-3260 has 0 B free on /home/jonathan/Projects"),
            "{text}"
        );
        assert!(
            text.contains("free space on precision-3260 to continue"),
            "{text}"
        );

        // And the other way round: a full / blocks staging, not the restore.
        record_samples(&[sample(host, 0, 40 << 30)], now_epoch_seconds());
        let error = ensure_room(&target, WORKER, 100 << 20, "stage the worker").unwrap_err();
        let text = format!("{error:#}");
        assert!(
            text.contains(
                "precision-3260 has 0 B free on / (the filesystem reserves 26.84 GB more for root)"
            ),
            "{text}"
        );
        ensure_room(&target, CLONE, 100 << 20, "restore the checkpoint").unwrap();

        // Room for the write, but not for the reserve beside it.
        record_samples(
            &[sample(host, WRITE_RESERVE_BYTES + (50 << 20), 40 << 30)],
            now_epoch_seconds(),
        );
        assert!(ensure_room(&target, WORKER, 100 << 20, "stage the worker").is_err());
        ensure_room(&target, WORKER, 10 << 20, "write a config").unwrap();
    }

    #[test]
    fn a_failed_write_marks_the_filesystem_it_names_until_a_later_measurement() {
        let host = "observed-3260";
        let target = ssh_bare(host);
        let now = now_epoch_seconds();
        record_samples(&[sample(host, 20 << 30, 20 << 30)], now);
        let mut changes = subscribe();
        changes.borrow_and_update();

        observe_no_space(
            host,
            "scp: write remote \"/home/jonathan/Projects/app/.mj/clones/abc/x\": No space left on device",
        );
        assert!(changes.has_changed().unwrap());
        assert!(ensure_room(&target, CLONE, 0, "restore").is_err());
        ensure_room(&target, WORKER, 0, "stage the worker").unwrap();

        // A probe that may have started before the failure does not clear it.
        record_samples(&[sample(host, 20 << 30, 20 << 30)], now_epoch_seconds());
        assert!(ensure_room(&target, CLONE, 0, "restore").is_err());
        record_samples(
            &[sample(host, 20 << 30, 20 << 30)],
            now_epoch_seconds() + NO_SPACE_HOLD_SECONDS,
        );
        ensure_room(&target, CLONE, 0, "restore").unwrap();
    }

    #[test]
    fn a_failure_naming_no_path_holds_the_host_until_measured() {
        let host = "unattributed-3260";
        let target = ssh_bare(host);
        record_samples(&[sample(host, 20 << 30, 20 << 30)], now_epoch_seconds());
        observe_no_space(
            host,
            "Error response from daemon: ENOSPC: no space left on device",
        );
        assert!(ensure_room(&target, WORKER, 0, "stage the worker").is_err());
        assert!(ensure_room(&target, CLONE, 0, "restore").is_err());
        // The measurement it asked for decides: here, both have room.
        record_samples(
            &[sample(host, 20 << 30, 20 << 30)],
            now_epoch_seconds() + NO_SPACE_HOLD_SECONDS,
        );
        ensure_room(&target, WORKER, 0, "stage the worker").unwrap();
    }

    #[test]
    fn a_session_is_disk_full_only_when_one_of_its_own_filesystems_is() {
        let host = "session-3260";
        let session = mj_core::state::TargetLocator::SshBare {
            host: host.into(),
            workspace: "/home/jonathan/Projects/app/.mj/clones/abc".into(),
            worker_id: None,
        };
        let elsewhere = mj_core::state::TargetLocator::SshBare {
            host: host.into(),
            workspace: "/srv/work/abc".into(),
            worker_id: None,
        };
        record_samples(&[sample(host, 40 << 30, 0)], now_epoch_seconds());
        let problem = located_session_problem(&session, "abc", None).unwrap();
        assert!(
            problem.starts_with("disk full: session-3260 has 0 B free on /home/jonathan/Projects"),
            "{problem}"
        );
        assert!(located_session_problem(&elsewhere, "abc", None).is_none());
    }
}
