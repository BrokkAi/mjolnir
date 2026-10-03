//! Recognising a full disk on a target, and measuring a target's free space.
//!
//! Mjolnir writes to targets through `scp`, `ssh`, container engines and its
//! own worker. Each says "the disk is full" in its own words, so the one
//! classifier lives here and every executor reports through it.
//!
//! Free space is kept per filesystem: a host can have a full `~/Projects`
//! beside a healthy `/`, and a write is judged by the filesystem it lands on.
//! Paths are target text (POSIX on every target Mjolnir writes to), so they
//! are interpreted here, at that boundary, and nowhere else.

use std::sync::OnceLock;

use super::{CommandOutput, CommandSpec};

/// Whether `text` reports that a write failed for lack of space.
///
/// Covers `No space left on device` (scp, ssh, coreutils, podman, docker and
/// Rust's `os error 28`) and `Disk quota exceeded` (EDQUOT): a quota stops
/// writes the same way a full filesystem does.
pub fn reports_no_space(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    text.contains("no space left on device")
        || text.contains("enospc")
        || text.contains("os error 28)")
        || text.contains("disk quota exceeded")
        || text.contains("edquot")
}

/// The target path a no-space failure names, when it names one: scp's
/// `write remote "PATH"`, coreutils' `cp: error writing 'PATH'`, or a
/// `PATH: No space left on device` prefix.
pub fn no_space_path(text: &str) -> Option<String> {
    let line = text.lines().find(|line| reports_no_space(line))?;
    let quoted = |open: &str, close: char| {
        let start = line.find(open)? + open.len();
        let end = line[start..].find(close)?;
        Some(line[start..start + end].to_owned())
    };
    if let Some(path) = quoted("write remote \"", '"') {
        return Some(path);
    }
    if let Some(path) = quoted("error writing '", '\'') {
        return Some(path);
    }
    if let Some(path) = quoted("error writing \"", '"') {
        return Some(path);
    }
    // `cat: /path: No space left on device`, `write /path: no space left`.
    line.split([' ', ':'])
        .find(|word| word.starts_with('/') && word.len() > 1)
        .map(str::to_owned)
}

/// The host a target command ran on, as the storage owner names it: the SSH
/// destination without its user, or [`LOCAL_STORAGE_HOST`].
pub fn storage_host_of_destination(destination: Option<&str>) -> String {
    match destination {
        Some(destination) => ssh_host_name(destination).to_owned(),
        None => LOCAL_STORAGE_HOST.to_owned(),
    }
}

/// The host part of an OpenSSH destination such as `build@host`.
pub fn ssh_host_name(destination: &str) -> &str {
    destination
        .rsplit_once('@')
        .map_or(destination, |(_, host)| host)
}

/// The name the storage owner gives the machine the daemon runs on.
pub const LOCAL_STORAGE_HOST: &str = "local";

/// Where worker roots live under an SSH or EC2 user's home.
pub const REMOTE_WORKERS_DIRECTORY: &str = ".local/share/hel/workers";
/// Where staged profile homes live under an SSH or EC2 user's home.
pub const REMOTE_PROFILES_DIRECTORY: &str = ".local/share/hel/profiles";
/// Upload staging and binary caches under an SSH user's home.
pub const REMOTE_CACHE_DIRECTORY: &str = ".cache/mjolnir";
/// mbx's default cache under a user's home.
pub const DEFAULT_BUILD_CACHE_DIRECTORY: &str = ".cache/mbx";
/// Temporary files every target writes.
pub const TEMPORARY_DIRECTORY: &str = "/tmp";

/// Container engines whose storage Mjolnir measures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerStorage {
    Podman,
    Docker,
}

impl ContainerStorage {
    /// Where the engine keeps images and container layers, which hold every
    /// path inside a container: rootless Podman under the user's home, Docker
    /// under its root directory.
    pub fn path(self) -> &'static str {
        match self {
            Self::Podman => ".local/share/containers",
            Self::Docker => "/var/lib/docker",
        }
    }
}

/// The filesystems one session writes to: its worker root, workspace (the
/// project directory or managed clone), staged profile home and `/tmp`; a
/// container session writes them all inside its engine's storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionStoragePaths {
    pub host: String,
    pub worker_root: String,
    pub others: Vec<String>,
}

impl SessionStoragePaths {
    pub fn all(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.worker_root.as_str()).chain(self.others.iter().map(String::as_str))
    }
}

/// [`SessionStoragePaths`] for a recorded target. `project_directory` is the
/// session's checkout on a local bare target.
pub fn session_storage_paths(
    locator: &crate::state::TargetLocator,
    session_id: &str,
    project_directory: Option<&std::path::Path>,
) -> SessionStoragePaths {
    use crate::state::TargetLocator as Recorded;
    let text = |path: &std::path::Path| path.to_string_lossy().into_owned();
    let remote_profile = format!("{REMOTE_PROFILES_DIRECTORY}/{session_id}");
    let (host, worker_root, others) = match locator {
        Recorded::LocalBare { worker_root } => (
            LOCAL_STORAGE_HOST.to_owned(),
            text(worker_root),
            project_directory
                .map(text)
                .into_iter()
                .chain([TEMPORARY_DIRECTORY.to_owned()])
                .collect(),
        ),
        Recorded::LocalPodman { .. } => (
            LOCAL_STORAGE_HOST.to_owned(),
            local_home_path(ContainerStorage::Podman.path()),
            Vec::new(),
        ),
        Recorded::LocalDocker { .. } => (
            LOCAL_STORAGE_HOST.to_owned(),
            ContainerStorage::Docker.path().to_owned(),
            Vec::new(),
        ),
        // Apple's container runtime keeps its storage in a VM disk image.
        Recorded::AppleContainer { .. } => {
            (LOCAL_STORAGE_HOST.to_owned(), String::new(), Vec::new())
        }
        Recorded::AwsEc2 {
            address,
            instance_id,
        } => (
            address.clone().unwrap_or_else(|| instance_id.clone()),
            format!("{REMOTE_WORKERS_DIRECTORY}/{session_id}"),
            vec![
                format!(".local/share/hel/workspaces/{session_id}"),
                remote_profile,
                TEMPORARY_DIRECTORY.to_owned(),
            ],
        ),
        Recorded::SshBare {
            host,
            workspace,
            worker_id,
        } => (
            ssh_host_name(host).to_owned(),
            format!(
                "{REMOTE_WORKERS_DIRECTORY}/{}",
                worker_id.as_deref().unwrap_or(session_id)
            ),
            vec![
                text(workspace),
                remote_profile,
                TEMPORARY_DIRECTORY.to_owned(),
            ],
        ),
        Recorded::SshPodman { host, .. } => (
            ssh_host_name(host).to_owned(),
            ContainerStorage::Podman.path().to_owned(),
            Vec::new(),
        ),
        Recorded::SshDocker { host, .. } => (
            ssh_host_name(host).to_owned(),
            ContainerStorage::Docker.path().to_owned(),
            Vec::new(),
        ),
    };
    SessionStoragePaths {
        host,
        worker_root,
        others,
    }
}

/// A home-relative path made absolute against this machine's home.
pub fn local_home_path(relative: &str) -> String {
    match dirs::home_dir() {
        Some(home) => home.join(relative).to_string_lossy().into_owned(),
        None => relative.to_owned(),
    }
}

/// A target path as the board compares it: absolute against `home`, without
/// a trailing slash. `~/x` and `x` are home-relative, as for ssh and scp.
pub fn normalize_target_path(path: &str, home: Option<&str>) -> String {
    let path = path.trim();
    let joined = if path.starts_with('/') {
        path.to_owned()
    } else {
        let relative = path.strip_prefix("~/").unwrap_or(path);
        let relative = relative.strip_prefix("./").unwrap_or(relative);
        match home {
            Some(home) if relative.is_empty() || relative == "~" || relative == "." => {
                home.to_owned()
            }
            Some(home) => format!("{}/{relative}", home.trim_end_matches('/')),
            None => relative.to_owned(),
        }
    };
    if joined.len() > 1 {
        joined.trim_end_matches('/').to_owned()
    } else {
        joined
    }
}

/// Whether `path` is `base` or lies under it, by whole components.
fn path_within(path: &str, base: &str) -> bool {
    path == base
        || base == "/"
        || path
            .strip_prefix(base)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// A write failed on `host` because its disk is full.
pub type NoSpaceObserver = fn(host: &str, detail: &str);

static OBSERVER: OnceLock<NoSpaceObserver> = OnceLock::new();

/// Install the process's owner of target storage health. Only the daemon
/// installs one; other processes classify nothing.
pub fn set_no_space_observer(observer: NoSpaceObserver) {
    let _ = OBSERVER.set(observer);
}

/// Report `detail` to the installed owner when it says the disk on `host` is
/// full. Returns whether it did.
pub fn report_if_no_space(host: &str, detail: &str) -> bool {
    if !reports_no_space(detail) {
        return false;
    }
    if let Some(observer) = OBSERVER.get() {
        observer(host, detail.trim());
    }
    true
}

/// Called by every process executor once a command has finished.
pub(super) fn observe_command_output(command: &CommandSpec, output: &CommandOutput) {
    if output.status == 0 {
        return;
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let host = storage_host_of_destination(command.ssh_destination.as_deref());
    if report_if_no_space(&host, &stderr) {
        tracing::warn!(
            %host,
            purpose = command.purpose.as_str(),
            "target command failed because the disk is full: {}",
            stderr.trim()
        );
    }
}

/// One filesystem Mjolnir writes to on a target, as `df -Pk` reports it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FilesystemSpace {
    /// Where the filesystem is mounted on the target.
    pub mount: String,
    /// Bytes an unprivileged writer can still use (`f_bavail`).
    pub available_bytes: u64,
    pub total_bytes: u64,
    /// Bytes the filesystem keeps back for root (ext4's reserve): free, but
    /// not to Mjolnir's non-root writers.
    #[serde(default)]
    pub reserved_bytes: u64,
    /// The measured paths that live on this filesystem, normalized. A write
    /// belongs to the filesystem of the longest of these that contains it.
    #[serde(default)]
    pub paths: Vec<String>,
}

/// What one probe found on one host: every filesystem Mjolnir writes to there.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HostStorageSample {
    pub host: String,
    /// The probing user's home, against which relative paths are read.
    #[serde(default)]
    pub home: Option<String>,
    pub filesystems: Vec<FilesystemSpace>,
}

/// A write must leave at least this much free for the sessions already
/// running on the filesystem: their journals, logs and harness writes are
/// what fail first on a full disk. A flat amount rather than a percentage,
/// because 2% of a 4 TB disk would refuse writes with 80 GB free.
pub const WRITE_RESERVE_BYTES: u64 = 1 << 30;

/// Below this a filesystem is shown as low on space; nothing is refused.
pub const LOW_SPACE_BYTES: u64 = 5 << 30;

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum StorageCondition {
    Ok,
    Low,
    /// Below [`WRITE_RESERVE_BYTES`], or a write to it failed for lack of
    /// space since the last measurement. Writes are refused and recovery waits.
    Full,
}

/// One filesystem with the storage owner's verdict.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FilesystemView {
    #[serde(flatten)]
    pub space: FilesystemSpace,
    pub condition: StorageCondition,
    /// The failed write that marked this filesystem full, until a later
    /// measurement replaces it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_space_detail: Option<String>,
}

/// The storage owner's published answer for one host.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TargetStorageView {
    pub host: String,
    #[serde(default)]
    pub home: Option<String>,
    #[serde(default)]
    pub filesystems: Vec<FilesystemView>,
    #[serde(default)]
    pub sampled_at_epoch_seconds: Option<u64>,
    /// A write failed for lack of space without naming where. Every
    /// filesystem on the host counts as full until the measurement it
    /// triggered says which one is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unattributed_no_space: Option<String>,
}

impl TargetStorageView {
    /// Judge each filesystem of one host from its latest measurement and any
    /// failure since.
    pub fn evaluate(
        host: &str,
        home: Option<String>,
        filesystems: &[FilesystemSpace],
        sampled_at_epoch_seconds: Option<u64>,
        no_space: impl Fn(&FilesystemSpace) -> Option<String>,
        unattributed_no_space: Option<String>,
    ) -> Self {
        let filesystems = filesystems
            .iter()
            .map(|space| {
                let no_space_detail = no_space(space);
                let condition = if no_space_detail.is_some()
                    || unattributed_no_space.is_some()
                    || space.available_bytes < WRITE_RESERVE_BYTES
                {
                    StorageCondition::Full
                } else if space.available_bytes < LOW_SPACE_BYTES {
                    StorageCondition::Low
                } else {
                    StorageCondition::Ok
                };
                FilesystemView {
                    space: space.clone(),
                    condition,
                    no_space_detail,
                }
            })
            .collect();
        Self {
            host: host.to_owned(),
            home,
            filesystems,
            sampled_at_epoch_seconds,
            unattributed_no_space,
        }
    }

    /// The filesystem `path` lands on: the one holding the longest measured
    /// path that contains it.
    pub fn filesystem_for(&self, path: &str) -> Option<&FilesystemView> {
        let path = normalize_target_path(path, self.home.as_deref());
        self.filesystems
            .iter()
            .flat_map(|filesystem| {
                filesystem
                    .space
                    .paths
                    .iter()
                    .filter(|base| path_within(&path, base))
                    .map(move |base| (base.len(), filesystem))
            })
            .max_by_key(|(length, _)| *length)
            .map(|(_, filesystem)| filesystem)
    }

    /// The worst filesystem among those `paths` land on.
    pub fn worst_for<'a>(
        &self,
        paths: impl IntoIterator<Item = &'a str>,
    ) -> Option<&FilesystemView> {
        paths
            .into_iter()
            .filter(|path| !path.is_empty())
            .filter_map(|path| self.filesystem_for(path))
            .max_by_key(|filesystem| filesystem.condition)
    }

    /// The worst filesystem on the host, for a one-line summary.
    pub fn worst(&self) -> Option<&FilesystemView> {
        self.filesystems.iter().max_by(|left, right| {
            left.condition
                .cmp(&right.condition)
                .then(right.space.available_bytes.cmp(&left.space.available_bytes))
        })
    }

    /// The full filesystem one of `paths` lands on, or every filesystem's
    /// stand-in while a failure is still unattributed.
    pub fn full_for<'a>(
        &self,
        paths: impl IntoIterator<Item = &'a str>,
    ) -> Option<&FilesystemView> {
        self.worst_for(paths)
            .filter(|filesystem| filesystem.condition == StorageCondition::Full)
    }

    /// "precision-3260 has 0 B free on /home (the filesystem reserves 24.69
    /// GB more for root)".
    pub fn explanation(&self, filesystem: &FilesystemView) -> String {
        let space = &filesystem.space;
        let mut text = format!(
            "{} has {} free on {}",
            self.host,
            crate::move_workspace::format_bytes(space.available_bytes),
            space.mount
        );
        if space.reserved_bytes >= WRITE_RESERVE_BYTES {
            text.push_str(&format!(
                " (the filesystem reserves {} more for root)",
                crate::move_workspace::format_bytes(space.reserved_bytes)
            ));
        }
        if filesystem.condition == StorageCondition::Full
            && space.available_bytes >= WRITE_RESERVE_BYTES
            && let Some(detail) = filesystem
                .no_space_detail
                .as_ref()
                .or(self.unattributed_no_space.as_ref())
        {
            text.push_str(&format!("; a write failed: {detail}"));
        }
        text
    }

    /// One line per filesystem: mount, free space, root reserve, and a flag
    /// on the full or low ones.
    pub fn filesystem_lines(&self) -> Vec<String> {
        self.filesystems
            .iter()
            .map(|filesystem| {
                let space = &filesystem.space;
                let mut line = format!(
                    "{}: {} free",
                    space.mount,
                    crate::move_workspace::format_bytes(space.available_bytes)
                );
                if space.reserved_bytes >= WRITE_RESERVE_BYTES {
                    line.push_str(&format!(
                        ", {} reserved for root",
                        crate::move_workspace::format_bytes(space.reserved_bytes)
                    ));
                }
                match filesystem.condition {
                    StorageCondition::Full => line.push_str(" (full)"),
                    StorageCondition::Low => line.push_str(" (low)"),
                    StorageCondition::Ok => {}
                }
                line
            })
            .collect()
    }

    /// A short summary of every filesystem, such as "/: 41.0 GB · /home:
    /// 0 B full".
    pub fn short_status(&self) -> String {
        if self.filesystems.is_empty() {
            return "free space unknown".to_owned();
        }
        self.filesystems
            .iter()
            .map(|filesystem| {
                let free = crate::move_workspace::format_bytes(filesystem.space.available_bytes);
                match filesystem.condition {
                    StorageCondition::Full => format!("{} {free} full", filesystem.space.mount),
                    StorageCondition::Low => format!("{} {free} low", filesystem.space.mount),
                    StorageCondition::Ok => format!("{} {free}", filesystem.space.mount),
                }
            })
            .collect::<Vec<_>>()
            .join(" · ")
    }

    /// "disk full: precision-3260 has 0 B free on /home …" when one of
    /// `paths` lands on a full filesystem.
    pub fn problem_for<'a>(&self, paths: impl IntoIterator<Item = &'a str>) -> Option<String> {
        let paths = paths.into_iter().collect::<Vec<_>>();
        match self.full_for(paths.iter().copied()) {
            Some(filesystem) => Some(format!("disk full: {}", self.explanation(filesystem))),
            // A failure no measurement has placed yet holds every path.
            None => self
                .unattributed_explanation()
                .filter(|_| paths.iter().any(|path| !path.is_empty()))
                .map(|explanation| format!("disk full: {explanation}")),
        }
    }

    /// "host ran out of disk space: …" for a failure no measurement has
    /// placed on a filesystem yet.
    fn unattributed_explanation(&self) -> Option<String> {
        self.unattributed_no_space
            .as_ref()
            .map(|detail| format!("{} ran out of disk space: {detail}", self.host))
    }

    /// The refusal for a write of `bytes` (zero when the size is unknown) to
    /// `path`, or `None` when it fits beside [`WRITE_RESERVE_BYTES`] or the
    /// path's filesystem has not been measured.
    pub fn refuse_write(&self, path: &str, bytes: u64, what: &str) -> Option<String> {
        let explanation = match self.filesystem_for(path) {
            Some(filesystem) => {
                let fits = filesystem.condition != StorageCondition::Full
                    && filesystem.space.available_bytes
                        >= bytes.saturating_add(WRITE_RESERVE_BYTES);
                if fits {
                    return None;
                }
                self.explanation(filesystem)
            }
            None => self.unattributed_explanation()?,
        };
        let size = if bytes > 0 {
            format!(" ({})", crate::move_workspace::format_bytes(bytes))
        } else {
            String::new()
        };
        Some(format!(
            "Cannot {what}{size}: {explanation}. Mjolnir keeps {} free for running sessions; free space on {} to continue.",
            crate::move_workspace::format_bytes(WRITE_RESERVE_BYTES),
            self.host
        ))
    }
}

/// The storage hosts a capacity row stands for: the host itself, or each
/// instance a fleet's last reading measured.
pub fn capacity_storage_hosts(
    target: &super::DeploymentCapacityTarget,
    usage: Option<&super::DeploymentCapacityUsage>,
) -> Vec<String> {
    match target.kind {
        super::DeploymentCapacityKind::Host => vec![target.host.clone()],
        super::DeploymentCapacityKind::AwsFleet => usage
            .map(|usage| {
                usage
                    .storage
                    .iter()
                    .map(|sample| sample.host.clone())
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// The views a row that stands for `hosts` shows.
pub fn views_for<'a>(
    views: &'a [TargetStorageView],
    hosts: &[String],
) -> Vec<&'a TargetStorageView> {
    views
        .iter()
        .filter(|view| hosts.contains(&view.host))
        .collect()
}

/// Shell that prints the probing user's home, then measures each path given
/// as an argument at its nearest existing ancestor:
/// `storage=<avail-kib>\t<total-kib>\t<used-kib>\t<mount>\t<path>`. `df -Pk`
/// is POSIX, so it reads the same on Linux and macOS; its "Available" column
/// is what a non-root user can write.
pub const STORAGE_PROBE_SCRIPT: &str = r#"
printf 'home=%s\n' "$HOME"
for hel_path in "$@"; do
    hel_probe=$hel_path
    while [ ! -e "$hel_probe" ] && [ "$hel_probe" != / ] && [ "$hel_probe" != . ]; do
        hel_probe=$(dirname -- "$hel_probe")
    done
    df -Pk -- "$hel_probe" 2>/dev/null | awk -v hel_path="$hel_path" 'NR == 2 { mount = $6; for (i = 7; i <= NF; i++) mount = mount " " $i; printf "storage=%s\t%s\t%s\t%s\t%s\n", $4, $2, $3, mount, hel_path }'
done
"#;

/// Parse [`STORAGE_PROBE_SCRIPT`] output into the probing user's home and one
/// entry per filesystem, each listing the measured paths on it.
pub fn parse_storage_lines(output: &[u8]) -> (Option<String>, Vec<FilesystemSpace>) {
    let text = String::from_utf8_lossy(output);
    let home = text
        .lines()
        .find_map(|line| line.strip_prefix("home="))
        .map(str::trim)
        .filter(|home| home.starts_with('/'))
        .map(str::to_owned);
    let mut filesystems: Vec<FilesystemSpace> = Vec::new();
    for line in text.lines() {
        let Some(row) = line.strip_prefix("storage=") else {
            continue;
        };
        let fields = row.split('\t').collect::<Vec<_>>();
        let [available, total, used, mount, path] = fields.as_slice() else {
            continue;
        };
        let (Ok(available), Ok(total), Ok(used)) = (
            available.parse::<u64>(),
            total.parse::<u64>(),
            used.parse::<u64>(),
        ) else {
            continue;
        };
        let path = normalize_target_path(path, home.as_deref());
        match filesystems.iter_mut().find(|known| known.mount == *mount) {
            Some(known) => {
                if !known.paths.contains(&path) {
                    known.paths.push(path);
                }
            }
            None => filesystems.push(FilesystemSpace {
                mount: (*mount).to_owned(),
                available_bytes: available.saturating_mul(1024),
                total_bytes: total.saturating_mul(1024),
                reserved_bytes: total
                    .saturating_sub(used)
                    .saturating_sub(available)
                    .saturating_mul(1024),
                paths: vec![path],
            }),
        }
    }
    (home, filesystems)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_a_full_disk_in_every_tool_s_words() {
        for text in [
            // scp to a full home directory, as on precision-3260.
            "scp: write remote \".local/share/hel/workers/abc/hel.prepared-upgrade-stage-1.next\": No space left on device",
            // A worker's own exit reason.
            "relay coordinator failed: append journal: No space left on device (os error 28)",
            // podman cp and docker cp.
            "Error: copying to container: write /var/lib/hel/workers/x/hel: no space left on device",
            "Error response from daemon: ENOSPC: no space left on device, write",
            "cp: error writing '/home/u/.cache/mjolnir/uploads/x': Disk quota exceeded",
            "write failed (os error 28)",
        ] {
            assert!(reports_no_space(text), "{text}");
        }
        for text in [
            "Permission denied (publickey)",
            "ssh: connect to host precision-3260 port 22: Connection timed out",
            "No such file or directory",
            "error 280 while writing",
        ] {
            assert!(!reports_no_space(text), "{text}");
        }
    }

    #[test]
    fn a_no_space_failure_names_the_path_it_could_not_write() {
        assert_eq!(
            no_space_path(
                "scp: write remote \".local/share/hel/workers/abc/hel.prepared-upgrade-stage-1.next\": No space left on device"
            )
            .as_deref(),
            Some(".local/share/hel/workers/abc/hel.prepared-upgrade-stage-1.next")
        );
        assert_eq!(
            no_space_path("cp: error writing '/home/u/Projects/x/y': No space left on device")
                .as_deref(),
            Some("/home/u/Projects/x/y")
        );
        assert_eq!(
            no_space_path("Error: write /var/lib/hel/workers/x/hel: no space left on device")
                .as_deref(),
            Some("/var/lib/hel/workers/x/hel")
        );
        assert_eq!(
            no_space_path("Error response from daemon: ENOSPC: no space left on device, write"),
            None
        );
    }

    #[test]
    fn storage_host_drops_the_user_from_an_ssh_destination() {
        assert_eq!(
            storage_host_of_destination(Some("jonathan@precision-3260")),
            "precision-3260"
        );
        assert_eq!(storage_host_of_destination(Some("builder")), "builder");
        assert_eq!(storage_host_of_destination(None), LOCAL_STORAGE_HOST);
    }

    /// precision-3260: `~/Projects` is its own filesystem beside `/`. Paths
    /// on one filesystem make one record, and a write is judged by the
    /// filesystem it lands on.
    #[test]
    fn storage_lines_keep_one_record_per_filesystem_and_place_each_path() {
        let output = b"home=/home/jonathan\n\
storage=0\t491134172\t467026656\t/\t.local/share/hel/workers\n\
storage=0\t491134172\t467026656\t/\t/tmp\n\
storage=41943040\t976762584\t900000000\t/home/jonathan/Projects\t~/Projects\n\
garbage\n";
        let (home, filesystems) = parse_storage_lines(output);
        assert_eq!(home.as_deref(), Some("/home/jonathan"));
        assert_eq!(filesystems.len(), 2);
        assert_eq!(filesystems[0].mount, "/");
        assert_eq!(
            filesystems[0].paths,
            ["/home/jonathan/.local/share/hel/workers", "/tmp"]
        );
        assert_eq!(
            filesystems[0].reserved_bytes,
            (491134172 - 467026656) * 1024
        );
        let view = TargetStorageView::evaluate(
            "precision-3260",
            home,
            &filesystems,
            Some(1),
            |_| None,
            None,
        );
        let worker = ".local/share/hel/workers/abc/hel.next";
        let clone = "/home/jonathan/Projects/app/.mj/clones/abc";
        assert_eq!(view.filesystem_for(worker).unwrap().space.mount, "/");
        assert_eq!(
            view.filesystem_for(clone).unwrap().space.mount,
            "/home/jonathan/Projects"
        );
        assert!(view.refuse_write(worker, 100 << 20, "stage").is_some());
        assert!(view.refuse_write(clone, 100 << 20, "restore").is_none());
        // A path under nothing measured is not judged.
        assert!(view.filesystem_for("/srv/elsewhere").is_none());
        assert!(view.refuse_write("/srv/elsewhere", 1, "write").is_none());
        let problem = view.problem_for([clone, worker]).unwrap();
        assert!(
            problem.starts_with(
                "disk full: precision-3260 has 0 B free on / (the filesystem reserves 24.69 GB more for root)"
            ),
            "{problem}"
        );
        assert_eq!(
            view.filesystem_lines(),
            [
                "/: 0 B free, 24.69 GB reserved for root (full)",
                "/home/jonathan/Projects: 42.95 GB free, 35.66 GB reserved for root"
            ]
        );
    }

    #[test]
    fn an_unattributed_failure_counts_every_filesystem_full_until_measured() {
        let (home, filesystems) =
            parse_storage_lines(b"home=/h\nstorage=41943040\t99999999\t1\t/\t/tmp\n");
        let view = TargetStorageView::evaluate(
            "host",
            home,
            &filesystems,
            Some(1),
            |_| None,
            Some("write failed: No space left on device".into()),
        );
        assert!(view.problem_for(["/tmp/x"]).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn storage_probe_script_measures_missing_paths_at_an_existing_ancestor() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("not/yet/created");
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(STORAGE_PROBE_SCRIPT)
            .arg("mj-storage")
            .arg(&missing)
            .arg(directory.path())
            .output()
            .unwrap();
        assert!(output.status.success());
        let (home, filesystems) = parse_storage_lines(&output.stdout);
        assert!(home.is_some());
        assert_eq!(filesystems.len(), 1, "{output:?}");
        assert!(filesystems[0].total_bytes > 0);
        assert_eq!(filesystems[0].paths.len(), 2);
    }

    #[test]
    fn session_paths_cover_worker_root_workspace_profile_and_tmp() {
        let paths = session_storage_paths(
            &crate::state::TargetLocator::SshBare {
                host: "precision-3260".into(),
                workspace: "/home/jonathan/Projects/app/.mj/clones/abc".into(),
                worker_id: None,
            },
            "abc",
            None,
        );
        assert_eq!(paths.host, "precision-3260");
        assert_eq!(paths.worker_root, ".local/share/hel/workers/abc");
        assert_eq!(
            paths.others,
            [
                "/home/jonathan/Projects/app/.mj/clones/abc",
                ".local/share/hel/profiles/abc",
                "/tmp"
            ]
        );
    }
}
