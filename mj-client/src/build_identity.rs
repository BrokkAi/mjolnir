//! Which of two Mjolnir builds is newer.
//!
//! The release version orders releases. Development builds, and two
//! installations of one release, share a version string, a wire protocol and
//! a schema, so the version alone cannot say which build a running daemon
//! should give way to. Each build therefore also carries the Git revision it
//! was built from, that commit's time, and when its executable file was
//! written. The commit time orders different revisions; the file time orders
//! two builds of one revision, which differ only in uncommitted changes or
//! build settings.
//!
//! A daemon publishes these facts as semver build metadata on the
//! `build_version` it already reports, for example
//! `2.24.0+19485f17….c1790824791.m1790830000123456789`. `daemon.json` and the
//! status reply reject unknown fields, so a new field would break every older
//! client that reads them. Build metadata is invisible to older clients: they
//! compare versions with `cmp_precedence`, which ignores it.

use anyhow::{Context as _, Result};
use std::cmp::Ordering;

/// One build of Mjolnir, as far as it can be ordered against another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildIdentity {
    /// The release version, without build metadata.
    version: semver::Version,
    /// The full Git revision. `None` for a daemon that predates publishing it.
    revision: Option<String>,
    /// The revision's committer time in Unix seconds, when the build knew it.
    commit_time: Option<i64>,
    /// When the executable file was last written, in Unix nanoseconds.
    executable_modified: Option<u128>,
}

const COMMIT_TIME_PREFIX: &str = "c";
const EXECUTABLE_MODIFIED_PREFIX: &str = "m";

impl BuildIdentity {
    /// Read a published `build_version`. A plain version, as every daemon
    /// before this one published, has no revision or times. Unrecognized
    /// build metadata is ignored so a later build can publish more.
    pub fn parse(published: &str) -> Result<Self> {
        let mut version = semver::Version::parse(published)
            .with_context(|| format!("parse build version {published:?}"))?;
        let metadata = std::mem::replace(&mut version.build, semver::BuildMetadata::EMPTY);
        let mut identity = Self {
            version,
            revision: None,
            commit_time: None,
            executable_modified: None,
        };
        for part in metadata.as_str().split('.').filter(|part| !part.is_empty()) {
            if part.len() == 40 && part.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                identity.revision = Some(part.to_ascii_lowercase());
            } else if let Some(time) = part
                .strip_prefix(COMMIT_TIME_PREFIX)
                .and_then(|time| time.parse().ok())
            {
                identity.commit_time = Some(time);
            } else if let Some(time) = part
                .strip_prefix(EXECUTABLE_MODIFIED_PREFIX)
                .and_then(|time| time.parse().ok())
            {
                identity.executable_modified = Some(time);
            }
        }
        Ok(identity)
    }

    /// The `build_version` a daemon running this build publishes.
    pub fn published(&self) -> String {
        let mut metadata = Vec::new();
        if let Some(revision) = &self.revision {
            metadata.push(revision.clone());
        }
        if let Some(time) = self.commit_time {
            metadata.push(format!("{COMMIT_TIME_PREFIX}{time}"));
        }
        if let Some(time) = self.executable_modified {
            metadata.push(format!("{EXECUTABLE_MODIFIED_PREFIX}{time}"));
        }
        if metadata.is_empty() {
            self.version.to_string()
        } else {
            format!("{}+{}", self.version, metadata.join("."))
        }
    }

    /// The build for a message a person reads, such as
    /// `2.24.0+19485f17, committed 2026-09-30 15:31:22 UTC, built 2026-09-30 16:02:09 UTC`.
    pub fn describe(&self) -> String {
        let mut text = self.version.to_string();
        if let Some(revision) = &self.revision {
            text.push('+');
            text.push_str(&revision[..8]);
        }
        if let Some(time) = self
            .commit_time
            .and_then(|seconds| chrono::DateTime::from_timestamp(seconds, 0))
        {
            text.push_str(&format!(
                ", committed {}",
                time.format("%Y-%m-%d %H:%M:%S UTC")
            ));
        }
        if let Some(time) = self.executable_modified.and_then(|nanos| {
            chrono::DateTime::from_timestamp(
                i64::try_from(nanos / 1_000_000_000).ok()?,
                (nanos % 1_000_000_000) as u32,
            )
        }) {
            text.push_str(&format!(", built {}", time.format("%Y-%m-%d %H:%M:%S UTC")));
        }
        text
    }

    pub fn version(&self) -> &semver::Version {
        &self.version
    }
}

/// The identity of the build this process runs.
///
/// The file time is read once per process, from the running inode on Linux, so
/// replacing the file on disk later does not change what this process says
/// about itself. A daemon publishes exactly this value.
pub fn this_build() -> &'static BuildIdentity {
    static THIS_BUILD: std::sync::OnceLock<BuildIdentity> = std::sync::OnceLock::new();
    THIS_BUILD.get_or_init(|| {
        let revision = mj_core::worker_build::BUILD_ID
            .rsplit_once('+')
            .map(|(_, revision)| revision.to_ascii_lowercase());
        BuildIdentity {
            version: semver::Version::parse(env!("CARGO_PKG_VERSION"))
                .expect("the workspace version is semver"),
            revision,
            commit_time: mj_core::worker_build::BUILD_COMMIT_TIME.parse().ok(),
            executable_modified: running_executable_modified(),
        }
    })
}

fn running_executable_modified() -> Option<u128> {
    #[cfg(target_os = "linux")]
    let path = std::path::PathBuf::from("/proc/self/exe");
    #[cfg(not(target_os = "linux"))]
    let path = std::env::current_exe().ok()?;
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(
        modified
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_nanos(),
    )
}

/// How a running daemon's build stands against a client's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonBuildOrder {
    /// The client's build is newer: the client replaces the daemon.
    Older,
    /// The same build: the client uses the daemon.
    Same,
    /// The daemon's build is newer: the client must never replace it.
    Newer,
    /// The facts do not establish an order: the client must not replace it.
    Unknown,
}

impl DaemonBuildOrder {
    fn from_daemon_ordering(ordering: Ordering) -> Self {
        match ordering {
            Ordering::Less => Self::Older,
            Ordering::Equal => Self::Same,
            Ordering::Greater => Self::Newer,
        }
    }
}

/// Order `daemon` against `client`.
///
/// Releases are ordered by version. Within one release, different revisions
/// are ordered by commit time, and two builds of one revision by when their
/// executables were written. A daemon that publishes no revision was built
/// before any build published one, so it is the older build. Anything else
/// that is missing, or a tie between different revisions, leaves the order
/// unknown.
pub fn compare_daemon_build(daemon: &BuildIdentity, client: &BuildIdentity) -> DaemonBuildOrder {
    match daemon.version.cmp_precedence(&client.version) {
        Ordering::Equal => {}
        other => return DaemonBuildOrder::from_daemon_ordering(other),
    }
    let Some(client_revision) = &client.revision else {
        return DaemonBuildOrder::Unknown;
    };
    let Some(daemon_revision) = &daemon.revision else {
        return DaemonBuildOrder::Older;
    };
    if daemon_revision == client_revision {
        return match (daemon.executable_modified, client.executable_modified) {
            (Some(daemon_time), Some(client_time)) => {
                DaemonBuildOrder::from_daemon_ordering(daemon_time.cmp(&client_time))
            }
            _ => DaemonBuildOrder::Unknown,
        };
    }
    match (daemon.commit_time, client.commit_time) {
        (Some(daemon_time), Some(client_time)) if daemon_time != client_time => {
            DaemonBuildOrder::from_daemon_ordering(daemon_time.cmp(&client_time))
        }
        _ => DaemonBuildOrder::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OLD: &str = "6a2a07136a2a07136a2a07136a2a07136a2a0713";
    const NEW: &str = "19485f1719485f1719485f1719485f1719485f17";

    fn build(published: &str) -> BuildIdentity {
        BuildIdentity::parse(published).unwrap()
    }

    // Hard-won: d7250ce22765: U-1: same-release builds displaced a newer daemon on alternate commands
    #[test]
    fn same_release_builds_are_ordered_by_commit_then_by_file_time() {
        let client = build(&format!("2.24.0+{OLD}.c100.m5000"));
        let order = |daemon: &str| compare_daemon_build(&build(daemon), &client);
        // A newer commit wins whatever the file times say.
        assert_eq!(
            order(&format!("2.24.0+{NEW}.c200.m1")),
            DaemonBuildOrder::Newer
        );
        assert_eq!(
            order(&format!("2.24.0+{NEW}.c50.m9999")),
            DaemonBuildOrder::Older
        );
        // One revision: the later file is the later build.
        assert_eq!(
            order(&format!("2.24.0+{OLD}.c100.m6000")),
            DaemonBuildOrder::Newer
        );
        assert_eq!(
            order(&format!("2.24.0+{OLD}.c100.m4000")),
            DaemonBuildOrder::Older
        );
        assert_eq!(
            order(&format!("2.24.0+{OLD}.c100.m5000")),
            DaemonBuildOrder::Same
        );
        // Releases come first.
        assert_eq!(order(&format!("2.25.0+{NEW}.c1")), DaemonBuildOrder::Newer);
        assert_eq!(order("2.23.0"), DaemonBuildOrder::Older);
    }

    #[test]
    fn a_daemon_without_a_published_build_is_older_and_missing_facts_are_unknown() {
        let client = build(&format!("2.24.0+{OLD}.c100.m5000"));
        let order = |daemon: &str| compare_daemon_build(&build(daemon), &client);
        assert_eq!(order("2.24.0"), DaemonBuildOrder::Older);
        // Different revisions without both commit times, or committed in the
        // same second, cannot be ordered.
        assert_eq!(
            order(&format!("2.24.0+{NEW}.m9999")),
            DaemonBuildOrder::Unknown
        );
        assert_eq!(
            order(&format!("2.24.0+{NEW}.c100")),
            DaemonBuildOrder::Unknown
        );
        let client_without_time = build(&format!("2.24.0+{OLD}.m5000"));
        assert_eq!(
            compare_daemon_build(&build(&format!("2.24.0+{NEW}.c1")), &client_without_time),
            DaemonBuildOrder::Unknown
        );
        // One revision without both file times.
        assert_eq!(
            order(&format!("2.24.0+{OLD}.c100")),
            DaemonBuildOrder::Unknown
        );
    }
}
