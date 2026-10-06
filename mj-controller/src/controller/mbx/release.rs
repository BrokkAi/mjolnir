//! Telling mbx that a workspace Mjolnir removed will never be built again.
//!
//! mbx keys a managed target directory and its learned incremental state by
//! the workspace path and nothing else. It collects them on its own only when
//! it can prove the checkout is gone from the filesystem that holds its target
//! root. A container's `/workspace/<id>` never passes that test from the host
//! or from another container, and neither does a checkout on a different
//! filesystem from the target root, so their build output would wait for
//! `target.max_age` (30 days by default) or for disk pressure. The code that
//! removes a workspace therefore says so, with `mbx clean <workspace>`.
//!
//! [`BuildStateRelease`] is the one place that decides what to release and how.
//! A release runs after the workspace and every process that could build in it
//! are gone: the worker's process group, the container, then the files, and
//! only then mbx's state. A build can no longer recreate what it removes, and
//! `mbx clean` of a path that no longer exists removes exactly the state keyed
//! by that path. A release is bounded by the executor's cleanup deadline, and
//! a failure is reported, never returned: it must not fail or block the
//! teardown that asked for it.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use super::{CacheHost, host_for_locator};
use crate::targets::{self, CommandExecutor, SshTarget};
use mj_core::config::Config;
use mj_core::state::SessionRecord;

/// `$0` for the release script, so it is recognizable in a process list.
const LABEL: &str = "mj-mbx-release";

/// What `stdout` says when a bare host has no mbx at all, so there is no build
/// state to release.
const ABSENT: &str = "mj-mbx: absent";

/// Failed releases, one JSON object per line, under the data directory.
const FAILURES_FILE: &str = "mbx-release-failures.jsonl";

/// The journal starts over past this size; it reports recent failures, not
/// history.
const FAILURES_MAX_BYTES: u64 = 256 * 1024;

/// How long `mj doctor` keeps reporting a failed release. mbx's own age limit
/// reclaims the output after 30 days by default.
pub(crate) const FAILURE_REPORT_SECS: u64 = 14 * 24 * 60 * 60;

/// `$1` mode (`native` or `shared`), `$2` the shared cache directory, `$3`
/// its configuration home, `$4` the preferred in-cache mbx path, `$5` the
/// old local binary root; the remaining arguments are workspaces.
///
/// A shared cache prefers the host executable its container mounted, then
/// searches the same locations as the native host probe. A bare host uses
/// that search directly.
///
/// A bare workspace path is resolved through its nearest existing ancestor,
/// because mbx records the physical path Cargo reported from inside the
/// checkout, and the checkout itself is already gone. A container path is
/// passed as written: it was never a path on this host.
const RELEASE_SCRIPT: &str = r#"set -u
mode=$1 cache=$2 config=$3 preferred=$4 legacy_root=$5
shift 5
mbx=
consider() {
    [ -z "$mbx" ] || return 0
    [ -n "$1" ] && [ -x "$1" ] || return 0
    "$1" --version >/dev/null 2>&1 || return 0
    mbx=$1
}
physical() {
    dir=$1 rest=
    while [ ! -d "$dir" ]; do
        parent=$(dirname -- "$dir")
        [ "$parent" != "$dir" ] || break
        rest=/$(basename -- "$dir")$rest
        dir=$parent
    done
    resolved=$(CDPATH= cd -- "$dir" && pwd -P) || return 1
    printf '%s%s\n' "${resolved%/}" "$rest"
}
if [ "$mode" = shared ]; then
    consider "$preferred"
fi
consider "$(command -v mbx 2>/dev/null || true)"
consider "$HOME/.local/bin/mbx"
consider "$HOME/.cargo/bin/mbx"
if [ -z "$mbx" ] && [ "$mode" = shared ]; then
    if [ -n "$legacy_root" ]; then
        for candidate in "$legacy_root"/*/*/mbx; do
            consider "$candidate"
        done
    else
        for candidate in "$HOME/.cache/mjolnir/mbx"/*/mbx; do
            consider "$candidate"
        done
    fi
fi
if [ -z "$mbx" ]; then
    if [ "$mode" = native ]; then
        echo 'mj-mbx: absent'
        exit 0
    fi
    echo 'no mbx on this host can release the shared build cache' >&2
    exit 3
fi
status=0
for workspace in "$@"; do
    if [ "$mode" = shared ]; then
        MBX_CACHE_DIR=$cache XDG_CONFIG_HOME=$config "$mbx" clean "$workspace" || status=$?
    elif path=$(physical "$workspace"); then
        "$mbx" clean "$path" || status=$?
    else
        echo "could not resolve $workspace" >&2
        status=1
    fi
done
exit "$status""#;

/// Which mbx owns the build state of the released workspaces.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Store {
    /// A bare host's own mbx, with its own configuration.
    Native,
    /// The cache directory a host's containers mount at the same path.
    Shared { directory: PathBuf },
}

/// The build state of workspaces Mjolnir has removed, and where it lives.
#[derive(Debug, Clone)]
pub(in crate::controller) struct BuildStateRelease {
    host: CacheHost,
    store: Store,
    preferred_mbx: Option<PathBuf>,
    /// The old per-version cache used by local shared-cache sessions. Remote
    /// copies remain under the SSH user's home directory.
    legacy_mbx_root: Option<PathBuf>,
    workspaces: Vec<PathBuf>,
}

impl BuildStateRelease {
    /// A managed clone or linked worktree, on this machine or an SSH host.
    pub(in crate::controller) fn managed_checkout(ssh: Option<SshTarget>, root: &Path) -> Self {
        Self {
            host: ssh.map_or(CacheHost::Local, CacheHost::Ssh),
            store: Store::Native,
            preferred_mbx: None,
            legacy_mbx_root: None,
            workspaces: vec![root.to_path_buf()],
        }
    }

    /// The repositories a session's own target held: an SSH workspace, or a
    /// container that mounted the shared build cache. `None` for a target
    /// another session owns, one that never had build state Mjolnir can
    /// name, and a container that ran without the cache.
    pub(in crate::controller) fn for_target(
        session: &SessionRecord,
        backend: &targets::TargetLocator,
        config: &Config,
    ) -> Option<Self> {
        let (host, root) = workspace_root(session, backend)?;
        let (store, preferred_mbx) = match backend {
            targets::TargetLocator::SshBare { .. } => (Store::Native, None),
            _ => {
                let cache = session.build_cache.as_ref()?;
                if host.key() != cache.host {
                    tracing::warn!(
                        session_id = %session.id,
                        recorded = %cache.host,
                        actual = %host.key(),
                        "the build cache was recorded on another host, so its state is not released"
                    );
                    return None;
                }
                (
                    Store::Shared {
                        directory: cache.directory.clone(),
                    },
                    Some(super::cache_binary_path(&cache.directory)),
                )
            }
        };
        let bundle = session.project_bundle(config)?;
        let workspaces = bundle
            .repositories
            .iter()
            .map(|repository| root.join(&repository.destination))
            .collect::<Vec<_>>();
        let legacy_mbx_root = match (&store, &host) {
            (Store::Shared { .. }, CacheHost::Local) => {
                Some(mj_core::config::data_dir().join("mbx"))
            }
            _ => None,
        };
        (!workspaces.is_empty()).then_some(Self {
            host,
            store,
            preferred_mbx,
            legacy_mbx_root,
            workspaces,
        })
    }

    /// This release without anything under the workspace root of `live`, the
    /// target the session goes on running in, when that is on the same host.
    ///
    /// mbx keys state by path alone, and a session keeps its id when it moves,
    /// so the target a Move retires can hold the same paths as the one the
    /// session continues in. Any path under the live root is kept, whether or
    /// not the live target has the build cache.
    pub(in crate::controller) fn excluding(
        mut self,
        live: Option<(&SessionRecord, &targets::TargetLocator)>,
    ) -> Option<Self> {
        if let Some((host, root)) =
            live.and_then(|(session, backend)| workspace_root(session, backend))
            && host.key() == self.host.key()
        {
            self.workspaces
                .retain(|workspace| !workspace.starts_with(&root));
        }
        (!self.workspaces.is_empty()).then_some(self)
    }

    /// Run `mbx clean` for every workspace. Never fails: a release that does
    /// not complete is logged, shown to the person, and kept for `mj doctor`.
    pub(in crate::controller) fn run(&self, executor: &impl CommandExecutor) {
        if !enabled() {
            return;
        }
        let command = self.command();
        let failure = match executor.execute_cleanup(&command) {
            Ok(output) if output.status == 0 => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                if stdout.trim() == ABSENT {
                    tracing::debug!(
                        host = %self.host.key(),
                        "no mbx on this host, so there is no build state to release"
                    );
                } else {
                    // mbx exits 0 when it keeps a target a running command
                    // holds, and says so on stderr.
                    tracing::info!(
                        host = %self.host.key(),
                        workspaces = ?self.workspaces,
                        output = %stdout.trim(),
                        warnings = %String::from_utf8_lossy(&output.stderr).trim(),
                        "released the mbx build state of removed workspaces"
                    );
                }
                return;
            }
            Ok(output) => format!(
                "exit status {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ),
            Err(error) => format!("{error:#}"),
        };
        let workspaces = self
            .workspaces
            .iter()
            .map(|workspace| workspace.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        tracing::warn!(
            host = %self.host.key(),
            workspaces,
            error = failure,
            "mbx kept the build state of removed workspaces"
        );
        executor.notify_notice(&format!(
            "The Rust build cache on {} still holds build output for {workspaces}: {failure}. \
             `mj doctor` shows how to remove it.",
            self.host.key()
        ));
        if let Err(error) = record_failure(self, &failure) {
            tracing::warn!(
                error = format!("{error:#}"),
                "could not record the failed mbx release for mj doctor"
            );
        }
    }

    fn command(&self) -> targets::CommandSpec {
        let (mode, directory, config_home) = match &self.store {
            Store::Native => ("native", String::new(), String::new()),
            Store::Shared { directory } => (
                "shared",
                directory.to_string_lossy().into_owned(),
                configuration_home(directory).to_string_lossy().into_owned(),
            ),
        };
        let arguments = [
            mode.to_owned(),
            directory,
            config_home,
            self.preferred_mbx
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default(),
            self.legacy_mbx_root
                .as_ref()
                .map(|root| root.to_string_lossy().into_owned())
                .unwrap_or_default(),
        ]
        .into_iter()
        .chain(
            self.workspaces
                .iter()
                .map(|workspace| workspace.to_string_lossy().into_owned()),
        );
        self.host.shell_command(
            RELEASE_SCRIPT,
            LABEL,
            arguments,
            "release the mbx build state of removed workspaces",
        )
    }

    /// The remediation `mj doctor` prints for this release.
    fn remediation(&self) -> String {
        let environment = match &self.store {
            Store::Native => String::new(),
            Store::Shared { directory } => format!(
                "MBX_CACHE_DIR={} XDG_CONFIG_HOME={} ",
                directory.display(),
                configuration_home(directory).display()
            ),
        };
        self.workspaces
            .iter()
            .map(|workspace| format!("{environment}mbx clean {}", workspace.display()))
            .collect::<Vec<_>>()
            .join("; ")
    }
}

/// The host and directory under which a session's own target checks out its
/// repositories, for the targets whose build state Mjolnir can name: an SSH
/// workspace of a bundle session, and a container's per-session workspace.
/// A borrowed target belongs to another session and has none of its own.
fn workspace_root(
    session: &SessionRecord,
    backend: &targets::TargetLocator,
) -> Option<(CacheHost, PathBuf)> {
    let checkout = session.checkout();
    if targets::is_borrowed(backend) {
        return None;
    }
    match backend {
        targets::TargetLocator::SshBare { ssh, workspace, .. }
            if checkout.project_directory().is_none() =>
        {
            Some((CacheHost::Ssh(ssh.clone()), PathBuf::from(workspace)))
        }
        // Only a per-session workspace: the legacy shared `/workspace` would
        // name every legacy session's checkout at once.
        targets::TargetLocator::LocalPodman { .. }
        | targets::TargetLocator::LocalDocker { .. }
        | targets::TargetLocator::SshPodman { .. }
        | targets::TargetLocator::SshDocker { .. } => Some((
            host_for_locator(backend)?,
            session.container_workspace.clone()?,
        )),
        _ => None,
    }
}

/// The `XDG_CONFIG_HOME` under which the containers' mbx finds the machine
/// policy Mjolnir publishes in the shared cache, `<home>/mbx/config.toml`.
fn configuration_home(directory: &Path) -> PathBuf {
    let configuration = mj_core::config::build_cache_configuration_directory(directory);
    configuration
        .parent()
        .map_or_else(|| configuration.clone(), Path::to_path_buf)
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Failure {
    at_secs: u64,
    host: String,
    error: String,
    remediation: String,
}

fn failures_path() -> PathBuf {
    mj_core::config::data_dir().join(FAILURES_FILE)
}

/// Append one line. A line this short is one `write` on an append-mode file,
/// so concurrent teardowns do not interleave.
fn record_failure(release: &BuildStateRelease, error: &str) -> anyhow::Result<()> {
    let path = failures_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let restart =
        std::fs::metadata(&path).is_ok_and(|metadata| metadata.len() > FAILURES_MAX_BYTES);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(!restart)
        .write(true)
        .truncate(restart)
        .open(&path)?;
    let mut line = serde_json::to_vec(&Failure {
        at_secs: now_secs(),
        host: release.host.key(),
        error: error.chars().take(512).collect(),
        remediation: release.remediation(),
    })?;
    line.push(b'\n');
    file.write_all(&line)?;
    Ok(())
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// A release that failed recently, for `mj doctor`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReleaseFailure {
    pub host: String,
    pub error: String,
    pub remediation: String,
}

/// Releases that failed within [`FAILURE_REPORT_SECS`], newest last.
pub(crate) fn recent_release_failures() -> Vec<ReleaseFailure> {
    let Ok(text) = std::fs::read_to_string(failures_path()) else {
        return Vec::new();
    };
    let since = now_secs().saturating_sub(FAILURE_REPORT_SECS);
    text.lines()
        .filter_map(|line| serde_json::from_str::<Failure>(line).ok())
        .filter(|failure| failure.at_secs >= since)
        .map(|failure| ReleaseFailure {
            host: failure.host,
            error: failure.error,
            remediation: failure.remediation,
        })
        .collect()
}

#[cfg(not(test))]
fn enabled() -> bool {
    true
}

// Most controller tests tear sessions down through the real process
// executor, and a release would then run this machine's own mbx against the
// developer's live build cache. Releases stay off in unit tests unless a test
// turns them on for its thread.
#[cfg(test)]
thread_local! {
    static ENABLED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn enabled() -> bool {
    ENABLED.with(std::cell::Cell::get)
}

/// Turn releases on for the current test thread until the guard drops.
#[cfg(test)]
pub(in crate::controller) fn enable_for_test() -> impl Drop {
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            ENABLED.with(|enabled| enabled.set(false));
        }
    }
    ENABLED.with(|enabled| enabled.set(true));
    Guard
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::controller::mbx::MBX_VERSION;
    use crate::targets::ProcessExecutor;

    /// A directory with a test-only `mbx` that logs each `clean` or rejects
    /// execution, plus a home with nothing in it.
    struct Sandbox {
        root: tempfile::TempDir,
    }

    impl Sandbox {
        fn new(with_mbx: bool) -> Self {
            let root = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(root.path().join("home")).unwrap();
            std::fs::create_dir_all(root.path().join("bin")).unwrap();
            let mbx = root.path().join("bin/mbx");
            let contents = if with_mbx {
                format!(
                    "#!/bin/sh\n[ \"$1\" = --version ] && {{ echo 'mbx {MBX_VERSION}'; exit 0; }}\n\
                         printf '%s|%s|%s\\n' \"$*\" \"${{MBX_CACHE_DIR:-}}\" \"${{XDG_CONFIG_HOME:-}}\" >> '{}'\n",
                    root.path().join("log").display()
                )
            } else {
                "#!/bin/sh\nexit 1\n".to_owned()
            };
            std::fs::write(&mbx, contents).unwrap();
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&mbx, std::fs::Permissions::from_mode(0o755)).unwrap();
            Self { root }
        }

        fn path(&self, relative: &str) -> PathBuf {
            self.root.path().join(relative)
        }

        /// Run `release` with this sandbox's `PATH` and home.
        fn run(&self, release: &BuildStateRelease) -> targets::CommandOutput {
            let mut command = release.command();
            command.env.insert(
                "PATH".into(),
                format!("{}:/usr/bin:/bin", self.path("bin").display()),
            );
            command
                .env
                .insert("HOME".into(), self.path("home").display().to_string());
            command.env.insert("MBX_CACHE_DIR".into(), String::new());
            command.env.insert("XDG_CONFIG_HOME".into(), String::new());
            ProcessExecutor.execute(&command).unwrap()
        }

        fn log(&self) -> String {
            std::fs::read_to_string(self.path("log")).unwrap_or_default()
        }

        fn install_legacy_mbx(&self, path: &Path) {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(
                path,
                format!(
                    "#!/bin/sh\n[ \"$1\" = --version ] && {{ echo 'mbx 1.19.0'; exit 0; }}\n\
                     printf '%s|%s|%s\\n' \"$*\" \"${{MBX_CACHE_DIR:-}}\" \"${{XDG_CONFIG_HOME:-}}\" >> '{}'\n",
                    self.path("log").display()
                ),
            )
            .unwrap();
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    fn shared(workspaces: &[&str]) -> BuildStateRelease {
        BuildStateRelease {
            host: CacheHost::Local,
            store: Store::Shared {
                directory: "/srv/cache".into(),
            },
            // Exercise PATH and legacy fallbacks without ever probing a
            // possibly live cache binary on the test host.
            preferred_mbx: None,
            legacy_mbx_root: None,
            workspaces: workspaces.iter().map(PathBuf::from).collect(),
        }
    }

    #[test]
    fn a_removed_bare_checkout_is_cleaned_by_its_physical_path() {
        let sandbox = Sandbox::new(true);
        std::fs::create_dir_all(sandbox.path("real/clones")).unwrap();
        std::os::unix::fs::symlink(sandbox.path("real"), sandbox.path("link")).unwrap();
        let release = BuildStateRelease::managed_checkout(None, &sandbox.path("link/clones/gone"));

        let output = sandbox.run(&release);

        assert_eq!(output.status, 0, "{output:?}");
        let physical = std::fs::canonicalize(sandbox.path("real")).unwrap();
        assert_eq!(
            sandbox.log(),
            format!("clean {}/clones/gone||\n", physical.display())
        );
    }

    // Hard-won: 741163fe: container workspace cleanup lost the cache mount policy and left stale build state
    #[test]
    fn a_container_workspace_is_cleaned_as_written_with_the_shared_cache_policy() {
        let sandbox = Sandbox::new(true);

        let output = sandbox.run(&shared(&["/workspace/abc/app", "/workspace/abc/lib"]));

        assert_eq!(output.status, 0, "{output:?}");
        assert_eq!(
            sandbox.log(),
            "clean /workspace/abc/app|/srv/cache|/srv/cache/.mjolnir/config\n\
             clean /workspace/abc/lib|/srv/cache|/srv/cache/.mjolnir/config\n"
        );
    }

    #[test]
    fn shared_cache_cleanup_prefers_the_recorded_host_mbx_path() {
        use std::os::unix::fs::PermissionsExt as _;

        let sandbox = Sandbox::new(true);
        let preferred = sandbox.path("preferred/mbx");
        std::fs::create_dir_all(preferred.parent().unwrap()).unwrap();
        std::fs::write(
            &preferred,
            format!(
                "#!/bin/sh\n[ \"$1\" = --version ] && {{ echo 'mbx {MBX_VERSION}'; exit 0; }}\n\
                 printf '%s|%s|%s\\n' \"$*\" \"${{MBX_CACHE_DIR:-}}\" \"${{XDG_CONFIG_HOME:-}}\" >> '{}'\n",
                sandbox.path("preferred.log").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&preferred, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut release = shared(&["/workspace/abc/app"]);
        release.preferred_mbx = Some(preferred.clone());

        let output = sandbox.run(&release);

        assert_eq!(output.status, 0, "{output:?}");
        assert_eq!(
            std::fs::read_to_string(sandbox.path("preferred.log")).unwrap(),
            "clean /workspace/abc/app|/srv/cache|/srv/cache/.mjolnir/config\n"
        );
        assert_eq!(sandbox.log(), "");
    }

    #[test]
    fn legacy_local_cleanup_finds_any_versioned_private_binary() {
        let sandbox = Sandbox::new(false);
        let legacy_root = sandbox.path("data/mbx");
        sandbox.install_legacy_mbx(&legacy_root.join("1.19.0/x86_64-unknown-linux-musl/mbx"));
        let mut release = shared(&["/workspace/abc/app"]);
        release.legacy_mbx_root = Some(legacy_root);

        let output = sandbox.run(&release);

        assert_eq!(output.status, 0, "{output:?}");
        assert_eq!(
            sandbox.log(),
            "clean /workspace/abc/app|/srv/cache|/srv/cache/.mjolnir/config\n"
        );
    }

    #[test]
    fn shared_remote_cleanup_finds_legacy_digest_directory_copies() {
        let sandbox = Sandbox::new(false);
        sandbox.install_legacy_mbx(&sandbox.path("home/.cache/mjolnir/mbx/sha256-old/mbx"));
        let release = shared(&["/workspace/abc/app"]);

        let output = sandbox.run(&release);

        assert_eq!(output.status, 0, "{output:?}");
        assert_eq!(
            sandbox.log(),
            "clean /workspace/abc/app|/srv/cache|/srv/cache/.mjolnir/config\n"
        );
    }

    #[test]
    fn a_bare_host_without_mbx_has_nothing_to_release() {
        let sandbox = Sandbox::new(false);
        let release = BuildStateRelease::managed_checkout(None, &sandbox.path("gone"));

        let output = sandbox.run(&release);

        assert_eq!(output.status, 0, "{output:?}");
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), ABSENT);
    }

    #[test]
    fn a_shared_cache_without_mbx_is_a_failure() {
        let sandbox = Sandbox::new(false);

        let output = sandbox.run(&shared(&["/workspace/abc/app"]));

        assert_eq!(output.status, 3, "{output:?}");
    }

    fn container_session(
        id: &str,
        backend_host: Option<&str>,
    ) -> (SessionRecord, targets::TargetLocator) {
        let mut session = crate::controller::test_support::checkpoint_test_session(id);
        session.container_workspace = Some(mj_core::targets::new_container_workspace(id).unwrap());
        session.build_cache = Some(mj_core::state::SessionBuildCache {
            host: backend_host.map_or_else(|| "local".to_owned(), |host| format!("ssh:{host}")),
            directory: "/srv/cache".into(),
            max_size: None,
            target_root: None,
        });
        let container_id = targets::resource_name(id).unwrap();
        let workspace_storage = targets::PodmanWorkspaceLocator::ContainerLayer;
        let backend = match backend_host {
            None => targets::TargetLocator::LocalPodman {
                borrowed_from: None,
                container_id,
                workspace_storage,
            },
            Some(host) => targets::TargetLocator::SshPodman {
                ssh: SshTarget {
                    destination: host.into(),
                    ssh_args: Vec::new(),
                },
                container_id,
                workspace_storage,
                borrowed_from: None,
            },
        };
        (session, backend)
    }

    #[test]
    fn a_moved_session_keeps_the_build_state_its_live_target_shares() {
        let id = "0123456789abcdef0123456789abcdef";
        let mut config = Config::default();
        config.bundles.insert(
            "project".into(),
            mj_core::config::ProjectBundle {
                primary_repo: "app".into(),
                repositories: vec![mj_core::config::ProjectRepository {
                    id: "app".into(),
                    github: Some("owner/app".into()),
                    destination: "app".into(),
                    ..Default::default()
                }],
            },
        );
        let (retired, retired_backend) = container_session(id, None);
        let release =
            || BuildStateRelease::for_target(&retired, &retired_backend, &config).unwrap();

        // A destination container on the same host checks out at the same path.
        let (live, live_backend) = container_session(id, None);
        assert!(release().excluding(Some((&live, &live_backend))).is_none());

        // One on another host does not share the retired host's cache.
        let (live, live_backend) = container_session(id, Some("build.test"));
        let kept = release().excluding(Some((&live, &live_backend))).unwrap();
        assert_eq!(
            kept.workspaces,
            [PathBuf::from(format!("/workspace/{id}/app"))]
        );
    }
}
