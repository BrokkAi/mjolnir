//! Path completion primitives shared by every surface that edits a path.
//!
//! Completion is a text protocol: the caller passes the text a user typed and
//! receives candidates in the same shape. Nothing here decides which host owns
//! a path; the caller names the host and supplies the executor.

use std::fs;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::targets::{
    CommandExecutor, CommandSpec, SshTarget, posix_quote, push_connection_reuse_args,
};

/// The most candidates any completion returns. A longer list is not readable
/// in a popup, and the caller reports that more matches exist.
pub const MAX_CANDIDATES: usize = 50;

/// What a path field accepts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionKind {
    /// Only directories, each with a trailing separator.
    #[default]
    Directories,
    /// Directories and files. Directories keep the trailing separator.
    Any,
}

/// The machine whose filesystem a path belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompletionHost {
    /// The machine running the controller.
    Local,
    /// The machine behind a configured target template.
    Target(String),
    /// A configured machine, which owns a home directory of its own.
    Machine(Box<crate::config::Machine>),
}

/// One answer to a completion request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathCompletion {
    pub candidates: Vec<String>,
    /// The single match, or the longer shared prefix a client may insert.
    pub insert: Option<String>,
    /// Whether matches were dropped to reach `MAX_CANDIDATES`.
    pub truncated: bool,
}

/// Whether text asks to be read as a filesystem path rather than a URL or an
/// `owner/repo` shorthand. The same predicate decides it in `remote_git`.
pub fn looks_like_path(text: &str) -> bool {
    text.starts_with('/')
        || text.starts_with('~')
        || text.starts_with('.')
        || crate::remote_git::is_windows_absolute_path(text)
}

/// Complete an on-disk path without spawning a shell.
pub fn local_completions(prefix: &str, kind: CompletionKind) -> Vec<String> {
    let (directory, fragment) = match prefix.rsplit_once('/') {
        Some((directory, fragment)) => (format!("{directory}/"), fragment),
        None => (String::new(), prefix),
    };
    let lookup = if directory.is_empty() {
        "."
    } else {
        &directory
    };
    let entries = match fs::read_dir(lookup) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::debug!(path = lookup, %error, "path completion directory could not be read");
            return Vec::new();
        }
    };
    let mut matches = entries
        .filter_map(|entry| {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    tracing::debug!(path = lookup, %error, "path completion directory entry could not be read");
                    return None;
                }
            };
            let name = entry.file_name();
            let name = match name.to_str() {
                Some(name) => name,
                None => {
                    tracing::debug!(path = %entry.path().display(), "path completion skipped a non-UTF-8 directory entry");
                    return None;
                }
            };
            if !name.starts_with(fragment) {
                return None;
            }
            let is_directory = entry.path().is_dir();
            match (kind, is_directory) {
                (_, true) => Some(format!("{directory}{name}/")),
                (CompletionKind::Any, false) => Some(format!("{directory}{name}")),
                (CompletionKind::Directories, false) => None,
            }
        })
        .collect::<Vec<_>>();
    matches.sort();
    matches.dedup();
    matches
}

/// Complete a path through the configured SSH target.
///
/// The SSH connection timeout and noninteractive mode keep a completion
/// request from blocking a screen when a host is unavailable. The quoted
/// prefix remains literal while the trailing glob is expanded only by the
/// remote shell.
pub fn ssh_completions(
    ssh: &SshTarget,
    prefix: &str,
    kind: CompletionKind,
    executor: &impl CommandExecutor,
) -> Result<Vec<String>> {
    if prefix.is_empty() {
        return Ok(Vec::new());
    }
    // `-p` marks directories with a trailing separator, which is how a
    // candidate says it has children.
    let remote_command = match kind {
        CompletionKind::Directories => format!("ls -d -- {}*/ 2>/dev/null", posix_quote(prefix)),
        CompletionKind::Any => format!("ls -dp -- {}* 2>/dev/null", posix_quote(prefix)),
    };
    let mut args = ssh.ssh_args.clone();
    args.extend([
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ConnectTimeout=3".into(),
        "-o".into(),
        "ServerAliveInterval=2".into(),
        "-o".into(),
        "ServerAliveCountMax=1".into(),
    ]);
    push_connection_reuse_args(&mut args, ssh);
    args.extend([ssh.destination.clone(), remote_command]);
    let output = executor.execute(
        &CommandSpec::new("ssh", args)
            .ssh_destination(ssh.destination.clone())
            .purpose("complete remote mount directory"),
    )?;
    if output.status != 0 {
        return Ok(Vec::new());
    }
    let mut matches = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|path| extends_prefix_by_one_component(path, prefix, kind))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    matches.sort();
    matches.dedup();
    Ok(matches)
}

/// Whether a remote line is a candidate: it extends the prefix by exactly one
/// component, and the remote encoding survived the round trip.
fn extends_prefix_by_one_component(path: &str, prefix: &str, kind: CompletionKind) -> bool {
    let Some(remainder) = path.strip_prefix(prefix) else {
        return false;
    };
    if remainder.is_empty() || path.contains('\u{FFFD}') {
        return false;
    }
    let directory = path.ends_with('/');
    if kind == CompletionKind::Directories && !directory {
        return false;
    }
    let body = if directory {
        &remainder[..remainder.len() - 1]
    } else {
        remainder
    };
    !body.contains('/')
}

/// Return the single match or the extra shared path prefix that a client can
/// insert without choosing between candidates.
pub fn common_insert(prefix: &str, candidates: &[String]) -> Option<String> {
    let first = candidates.first()?;
    if candidates.len() == 1 {
        return Some(first.clone());
    }
    let common = candidates
        .iter()
        .skip(1)
        .fold(first.clone(), |common, next| {
            common
                .chars()
                .zip(next.chars())
                .take_while(|(left, right)| left == right)
                .map(|(character, _)| character)
                .collect()
        });
    (common.len() > prefix.len() && common.starts_with(prefix)).then_some(common)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn any_kind_lists_files_without_a_separator_and_directories_with_one() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("data")).unwrap();
        std::fs::write(directory.path().join("data.txt"), "a file").unwrap();
        let prefix = format!("{}/da", directory.path().display());

        assert_eq!(
            local_completions(&prefix, CompletionKind::Any),
            // A directory's trailing separator sorts it after a file whose
            // name extends the same stem.
            vec![
                format!("{}/data.txt", directory.path().display()),
                format!("{}/data/", directory.path().display()),
            ]
        );
    }

    #[test]
    fn directories_kind_omits_files() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("data")).unwrap();
        std::fs::write(directory.path().join("data.txt"), "a file").unwrap();
        let prefix = format!("{}/da", directory.path().display());

        assert_eq!(
            local_completions(&prefix, CompletionKind::Directories),
            vec![format!("{}/data/", directory.path().display())]
        );
    }

    #[test]
    fn common_insert_extends_only_when_shared() {
        assert_eq!(
            common_insert("/srv/da", &["/srv/data/".into(), "/srv/database/".into()]),
            Some("/srv/data".into())
        );
        assert_eq!(
            common_insert("/srv/da", &["/srv/data/".into()]),
            Some("/srv/data/".into())
        );
        assert_eq!(
            common_insert("/srv/da", &["/srv/data/".into(), "/srv/dbs/".into()]),
            None
        );
        assert_eq!(common_insert("/srv/da", &[]), None);
    }

    #[test]
    // macOS filesystems reject invalid UTF-8 names before completion can read them.
    #[cfg(target_os = "linux")]
    fn non_utf8_entries_are_skipped() {
        use std::os::unix::ffi::OsStrExt;

        let directory = tempfile::tempdir().unwrap();
        let invalid = std::ffi::OsStr::from_bytes(b"da\xff");
        std::fs::create_dir(directory.path().join(invalid)).unwrap();
        std::fs::create_dir(directory.path().join("data")).unwrap();
        let prefix = format!("{}/da", directory.path().display());

        assert_eq!(
            local_completions(&prefix, CompletionKind::Any),
            vec![format!("{}/data/", directory.path().display())]
        );
    }

    #[test]
    fn looks_like_path_accepts_slash_tilde_dot_and_windows_drives() {
        for path in ["/srv", "~/cache", "./here", "../there", "C:/users", "d:\\x"] {
            assert!(looks_like_path(path), "{path}");
        }
        for other in [
            "owner/repo",
            "https://example.invalid/repo.git",
            "git@example.invalid:owner/repo.git",
            "",
        ] {
            assert!(!looks_like_path(other), "{other}");
        }
    }

    #[test]
    fn remote_lines_are_kept_only_when_they_extend_the_prefix_by_one_component() {
        for (line, kind, kept) in [
            ("/srv/projects/", CompletionKind::Directories, true),
            ("/srv/projects/nested/", CompletionKind::Directories, false),
            ("/srv/prompts.txt", CompletionKind::Directories, false),
            ("/srv/prompts.txt", CompletionKind::Any, true),
            ("/srv/pr\u{FFFD}x/", CompletionKind::Any, false),
            ("/srv/pr", CompletionKind::Any, false),
            ("/other/pr", CompletionKind::Any, false),
        ] {
            assert_eq!(
                extends_prefix_by_one_component(line, "/srv/pr", kind),
                kept,
                "{line} {kind:?}"
            );
        }
    }
}
