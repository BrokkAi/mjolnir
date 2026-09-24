//! The build stamp compiled into every worker binary.
//!
//! The controller copies a worker file from its own host into each target, so
//! it has to know which build that file is before uploading it. It cannot run
//! a Linux worker on macOS to ask, so the worker carries a fixed marker in its
//! bytes and the controller reads it straight from the file:
//!
//! ```text
//! MJ-WORKER-BUILD:<package version>[+<git sha>]:END
//! ```
//!
//! [`worker_build_marker!`](crate::worker_build_marker) expands to that marker
//! for the crate that invokes it, which must set `MJ_BUILD_GIT_SHA` from its
//! build script (empty when the source has no git checkout).

use std::fmt;
use std::io::Read as _;
use std::path::Path;

use anyhow::{Context as _, Result, bail};

pub const MARKER_PREFIX: &str = "MJ-WORKER-BUILD:";
pub const MARKER_SUFFIX: &str = ":END";

/// Longest stamp accepted between the prefix and the suffix. A version plus a
/// full SHA-256 object name is well under this.
const MAX_STAMP_LEN: usize = 128;

/// The marker for the invoking crate's build, as a `&'static str` literal.
#[macro_export]
macro_rules! worker_build_marker {
    () => {
        concat!(
            "MJ-WORKER-BUILD:",
            env!("CARGO_PKG_VERSION"),
            "+",
            env!("MJ_BUILD_GIT_SHA"),
            ":END"
        )
    };
}

/// The build a worker binary or a controller was compiled from.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WorkerBuild {
    pub version: String,
    /// `None` when the build had no git checkout, such as `cargo install`
    /// from a published crate.
    pub git_sha: Option<String>,
}

impl WorkerBuild {
    /// Parse the text between [`MARKER_PREFIX`] and [`MARKER_SUFFIX`].
    pub fn parse_stamp(stamp: &str) -> Option<Self> {
        let (version, git_sha) = match stamp.split_once('+') {
            Some((version, sha)) => (version, (!sha.is_empty()).then(|| sha.to_owned())),
            None => (stamp, None),
        };
        let version_ok = !version.is_empty()
            && version
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'));
        let sha_ok = git_sha
            .as_deref()
            .is_none_or(|sha| sha.bytes().all(|byte| byte.is_ascii_hexdigit()));
        (version_ok && sha_ok).then(|| Self {
            version: version.to_owned(),
            git_sha,
        })
    }

    /// Parse a whole marker such as the one [`worker_build_marker!`] expands to.
    ///
    /// [`worker_build_marker!`]: crate::worker_build_marker
    pub fn parse_marker(marker: &str) -> Option<Self> {
        Self::parse_stamp(
            marker
                .strip_prefix(MARKER_PREFIX)?
                .strip_suffix(MARKER_SUFFIX)?,
        )
    }

    /// Whether a worker of this build may serve a controller of `controller`.
    ///
    /// The versions must be equal. The git revisions must be equal too when
    /// both builds know theirs; a build from a published crate has none, and
    /// its version alone names the release.
    #[must_use]
    pub fn serves(&self, controller: &Self) -> bool {
        self.version == controller.version
            && match (&self.git_sha, &controller.git_sha) {
                (Some(worker), Some(controller)) => worker == controller,
                _ => true,
            }
    }
}

impl fmt::Display for WorkerBuild {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.git_sha {
            Some(sha) => write!(formatter, "{}+{}", self.version, short_sha(sha)),
            None => formatter.write_str(&self.version),
        }
    }
}

fn short_sha(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

/// Read the build stamp from a worker executable without running it.
///
/// `Ok(None)` means the file carries no stamp, which is every worker from
/// before stamps existed. A file with two different stamps is an error: it
/// cannot be told which build it is.
pub fn read_worker_build(path: &Path) -> Result<Option<WorkerBuild>> {
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("open worker executable {}", path.display()))?;
    // Keep enough of each chunk's tail that a marker split across two reads
    // is still seen whole in the next window.
    let overlap = MARKER_PREFIX.len() + MAX_STAMP_LEN + MARKER_SUFFIX.len();
    let mut window = Vec::with_capacity(1024 * 1024 + overlap);
    let mut buffer = vec![0_u8; 1024 * 1024];
    let mut found: Option<WorkerBuild> = None;
    let mut record = |build: WorkerBuild| -> Result<()> {
        match &found {
            Some(existing) if *existing != build => bail!(
                "worker executable {} carries two build stamps, {existing} and {build}",
                path.display()
            ),
            Some(_) => Ok(()),
            None => {
                found = Some(build);
                Ok(())
            }
        }
    };
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("read worker executable {}", path.display()))?;
        let at_end = read == 0;
        window.extend_from_slice(&buffer[..read]);
        // A marker is only complete once its suffix is in the window, so
        // everything but the tail can be settled now.
        let settled = if at_end {
            window.len()
        } else {
            window.len().saturating_sub(overlap)
        };
        for start in memchr::memmem::find_iter(&window, MARKER_PREFIX.as_bytes()) {
            if start >= settled {
                break;
            }
            if let Some(build) = stamp_at(&window[start + MARKER_PREFIX.len()..]) {
                record(build)?;
            }
        }
        if at_end {
            break;
        }
        window.drain(..settled);
    }
    Ok(found)
}

/// The stamp that starts at `bytes`, if a well-formed one does. The prefix
/// alone also appears in any binary that links this module's reader, so a
/// prefix without a valid stamp and suffix after it is not a marker.
fn stamp_at(bytes: &[u8]) -> Option<WorkerBuild> {
    let limit = bytes.len().min(MAX_STAMP_LEN + MARKER_SUFFIX.len());
    let end = memchr::memmem::find(&bytes[..limit], MARKER_SUFFIX.as_bytes())?;
    WorkerBuild::parse_stamp(std::str::from_utf8(&bytes[..end]).ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(version: &str, sha: Option<&str>) -> WorkerBuild {
        WorkerBuild {
            version: version.into(),
            git_sha: sha.map(str::to_owned),
        }
    }

    #[test]
    fn marker_round_trips_with_and_without_a_git_revision() {
        assert_eq!(
            WorkerBuild::parse_marker("MJ-WORKER-BUILD:2.20.0+0123abcd:END"),
            Some(build("2.20.0", Some("0123abcd")))
        );
        assert_eq!(
            WorkerBuild::parse_marker("MJ-WORKER-BUILD:2.20.0+:END"),
            Some(build("2.20.0", None))
        );
        assert_eq!(WorkerBuild::parse_marker("MJ-WORKER-BUILD::END"), None);
    }

    #[test]
    fn a_worker_serves_only_the_same_version_and_known_revision() {
        let controller = build("2.20.0", Some("aaaa"));
        assert!(build("2.20.0", Some("aaaa")).serves(&controller));
        assert!(!build("2.20.0", Some("bbbb")).serves(&controller));
        assert!(!build("2.6.0", Some("aaaa")).serves(&controller));
        // A published-crate controller knows only its version.
        assert!(build("2.20.0", Some("bbbb")).serves(&build("2.20.0", None)));
        assert!(build("2.20.0", None).serves(&controller));
    }

    #[test]
    fn reader_finds_a_marker_split_across_read_chunks_in_a_large_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("worker");
        // Straddle the first 1 MiB read boundary, and repeat the bare prefix
        // the way a binary that links the reader does.
        let mut bytes = vec![0_u8; 1024 * 1024 - 10];
        bytes.extend_from_slice(b"MJ-WORKER-BUILD:2.20.0+abc123:END");
        bytes.extend_from_slice(&vec![7_u8; 3 * 1024 * 1024]);
        bytes.extend_from_slice(b"MJ-WORKER-BUILD:\0\0\0");
        std::fs::write(&path, &bytes).unwrap();

        assert_eq!(
            read_worker_build(&path).unwrap(),
            Some(build("2.20.0", Some("abc123")))
        );
    }

    #[test]
    fn reader_reports_an_unstamped_file_and_rejects_two_stamps() {
        let directory = tempfile::tempdir().unwrap();
        let unstamped = directory.path().join("old");
        std::fs::write(&unstamped, vec![1_u8; 2 * 1024 * 1024]).unwrap();
        assert_eq!(read_worker_build(&unstamped).unwrap(), None);

        let ambiguous = directory.path().join("two");
        std::fs::write(
            &ambiguous,
            b"MJ-WORKER-BUILD:2.20.0+aa:END....MJ-WORKER-BUILD:2.19.0+bb:END",
        )
        .unwrap();
        assert!(read_worker_build(&ambiguous).is_err());
    }
}
