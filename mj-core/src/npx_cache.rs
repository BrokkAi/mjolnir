//! Clearing the npx cache entry behind a bundled command.
//!
//! npx materializes each package spec into an entry under its cache and reuses
//! that entry whenever the tree contains a manifest satisfying the spec. It
//! never checks that the install actually finished, so an install interrupted
//! partway through (Ctrl+C, crash, kill) leaves a tree every later invocation
//! trusts and fails on: no npm version validates completeness, and no flag
//! bypasses a broken entry (issue #896).
//!
//! npm's own recovery is `npm cache npx rm <key>`, which is what
//! [`remove_entry`] runs. npm resolves where its cache lives — configurable in
//! every npmrc layer as well as the environment — and does the deletion, so
//! neither is guessed here.
//!
//! Only the key has to be derived, because `rm` takes keys and a half-finished
//! entry cannot be looked up by package name: `npm cache npx ls` identifies
//! entries by the `_npx` marker that an interrupted install never wrote, and
//! lists them as `(empty/invalid)`. The key is a hash of the package specs, so
//! it is the same whatever state the tree is in.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use sha2::{Digest, Sha512};
use tokio::process::Command;

/// How long to wait for `npm cache npx rm`. Generous next to node's startup
/// cost, and it only delays a launch that has already failed.
const NPM_TIMEOUT: Duration = Duration::from_secs(30);

/// Extract the npm package specs from an npx argument list, mirroring how
/// libnpmexec decides what to install: `--package`/`-p` values win, otherwise
/// the first positional argument is the spec and everything after it belongs
/// to the launched program.
pub fn packages_from_args(args: &[String]) -> Vec<String> {
    let mut packages = Vec::new();
    let mut positional = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if let Some(value) = arg
            .strip_prefix("--package=")
            .or_else(|| arg.strip_prefix("-p="))
        {
            packages.push(value.to_string());
        } else if arg == "--package" || arg == "-p" {
            if let Some(value) = iter.next() {
                packages.push(value.clone());
            }
        } else if arg == "--" {
            positional = iter.next().cloned();
            break;
        } else if arg.starts_with('-') {
            // Boolean flag (-y, --yes, ...); the flags that take a separate
            // value are handled above.
        } else {
            positional = Some(arg.clone());
            break;
        }
    }
    if packages.is_empty() {
        packages.extend(positional);
    }
    packages
}

/// The npx cache key for a set of package specs: sha512 over the sorted specs
/// joined by newlines, first 16 hex characters — the derivation libnpmexec
/// uses to name its install directory.
pub fn entry_key(packages: &[String]) -> String {
    let mut sorted: Vec<&str> = packages.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    let digest = Sha512::digest(sorted.join("\n").as_bytes());
    digest
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Clear the npx cache entry this invocation runs from, so the next run
/// reinstalls it, via `npm cache npx rm`.
///
/// Returns whether npm removed the entry. A missing entry is reported by npm
/// as an invalid key and comes back `false`, so a caller can tell "there was
/// something broken to clear" from "nothing to do here".
pub async fn remove_entry(
    npx_command: &Path,
    args: &[String],
    env: &HashMap<String, String>,
) -> bool {
    let packages = packages_from_args(args);
    if packages.is_empty() {
        return false;
    }
    let Some(npm) = sibling_npm(npx_command) else {
        tracing::warn!(
            npx = %npx_command.display(),
            "no npm beside npx; leaving the npx cache alone"
        );
        return false;
    };
    let key = entry_key(&packages);
    let mut command = Command::new(npm);
    command
        .args(["cache", "npx", "rm", &key])
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = match tokio::time::timeout(NPM_TIMEOUT, command.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => {
            tracing::warn!("could not run `npm cache npx rm {key}`: {error}");
            return false;
        }
        Err(_) => {
            tracing::warn!("`npm cache npx rm {key}` timed out");
            return false;
        }
    };
    if output.status.success() {
        tracing::warn!(
            key = %key,
            "cleared npx cache entry after a failed launch: {}",
            String::from_utf8_lossy(&output.stdout).trim()
        );
        true
    } else {
        // Usually just "Invalid npx key": nothing was cached for this spec, so
        // the failure was not a poisoned entry.
        tracing::debug!(
            key = %key,
            "npm did not remove an npx cache entry: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        false
    }
}

/// The `npm` shipped alongside an `npx`, or `None` when it is not there.
fn sibling_npm(npx_command: &Path) -> Option<PathBuf> {
    let npm_name = match npx_command.file_name()?.to_str()? {
        "npx" => "npm",
        "npx.cmd" => "npm.cmd",
        "npx.exe" => "npm.exe",
        _ => return None,
    };
    let npm = npx_command.parent()?.join(npm_name);
    npm.is_file().then_some(npm)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    /// A stand-in `npx` with an `npm` beside it that records its arguments and
    /// exits with `exit_code`. Returns the fake npx path and the file the
    /// arguments land in.
    #[cfg(unix)]
    fn fake_npm_pair(dir: &Path, exit_code: u8) -> (PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).expect("create bin");
        let npx = bin.join("npx");
        std::fs::write(&npx, "#!/bin/sh\nexit 1\n").expect("write npx");
        let recorded = dir.join("npm-args");
        let npm = bin.join("npm");
        std::fs::write(
            &npm,
            format!(
                "#!/bin/sh\necho \"$@\" > '{}'\nexit {exit_code}\n",
                recorded.display()
            ),
        )
        .expect("write npm");
        for path in [&npx, &npm] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        (npx, recorded)
    }

    #[test]
    fn extracts_packages_from_bundled_invocations() {
        assert_eq!(
            packages_from_args(&strings(&[
                "-y",
                "@agentclientprotocol/claude-agent-acp",
                "--cli"
            ])),
            vec!["@agentclientprotocol/claude-agent-acp"]
        );
        assert_eq!(
            packages_from_args(&strings(&[
                "--yes",
                "--package=@agentclientprotocol/codex-acp",
                "codex"
            ])),
            vec!["@agentclientprotocol/codex-acp"]
        );
        assert_eq!(
            packages_from_args(&strings(&["--package", "left", "-p=right", "bin"])),
            vec!["left", "right"]
        );
        assert!(packages_from_args(&strings(&["-y", "--yes"])).is_empty());
    }

    /// The keys from issue #896, taken from the reporter's error output and
    /// confirmed against a real npm 11 cache. `npm cache npx rm` rejects a key
    /// it does not know, so a derivation that drifts from libnpmexec's would
    /// clear nothing and the retry would repeat the same failure.
    #[test]
    fn entry_keys_match_real_npx_cache_dirs() {
        assert_eq!(
            entry_key(&strings(&["@agentclientprotocol/codex-acp"])),
            "4877722a062902ce"
        );
        assert_eq!(
            entry_key(&strings(&["@agentclientprotocol/claude-agent-acp"])),
            "d820eb7d96bc2600"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn asks_npm_to_remove_the_key_for_these_packages() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (npx, recorded) = fake_npm_pair(temp.path(), 0);
        let args = strings(&["-y", "@agentclientprotocol/claude-agent-acp", "--cli"]);

        assert!(remove_entry(&npx, &args, &HashMap::new()).await);
        assert_eq!(
            std::fs::read_to_string(&recorded)
                .expect("recorded args")
                .trim(),
            "cache npx rm d820eb7d96bc2600"
        );
    }

    /// npm rejects a key with no cached entry, which means the failure was not
    /// a poisoned install and there is nothing to retry.
    #[cfg(unix)]
    #[tokio::test]
    async fn reports_no_removal_when_npm_rejects_the_key() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (npx, _) = fake_npm_pair(temp.path(), 1);
        let args = strings(&["-y", "@agentclientprotocol/codex-acp"]);

        assert!(!remove_entry(&npx, &args, &HashMap::new()).await);
    }

    #[tokio::test]
    async fn does_nothing_without_a_package_or_an_npm() {
        let temp = tempfile::tempdir().expect("tempdir");
        let missing_npm = temp.path().join("bin").join("npx");
        assert!(!remove_entry(&missing_npm, &strings(&["-y", "pkg"]), &HashMap::new()).await);
        assert!(!remove_entry(&missing_npm, &strings(&["-y"]), &HashMap::new()).await);
    }
}
