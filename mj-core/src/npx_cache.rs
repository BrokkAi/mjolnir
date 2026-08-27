//! Locating and clearing the npx cache entry behind a bundled command.
//!
//! npx materializes each package spec into `<npm-cache>/_npx/<hash>` and
//! reuses that tree whenever it contains a manifest satisfying the spec. It
//! never checks that the install actually finished, so an install interrupted
//! partway through (Ctrl+C, crash, kill) leaves a tree that every later
//! invocation trusts and fails on: no npm version validates completeness and
//! no flag bypasses a broken entry. The supported recovery is deleting the
//! entry, which is what `npm cache npx rm <key>` does — and what
//! [`remove_entry`] does here so a retry can do it without the user having to
//! find the directory by hand (issue #896).
//!
//! Deleting an entry is cheap to undo: npm's package store (`_cacache`) is
//! content-addressed and checksummed, so the reinstall re-extracts from local
//! tarballs instead of re-downloading.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha512};

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

/// The `_npx` entry directory name for a set of package specs: sha512 over the
/// sorted specs joined by newlines, first 16 hex characters — the derivation
/// libnpmexec uses to pick its install directory.
pub fn entry_name(packages: &[String]) -> String {
    let mut sorted: Vec<&str> = packages.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    let digest = Sha512::digest(sorted.join("\n").as_bytes());
    digest
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// npm's cache root as npx would resolve it: `npm_config_cache` from the spawn
/// env or this process's env, else the platform default. A cache path set only
/// in a user-level `.npmrc` is not consulted; there the computed entry simply
/// will not exist and removal is a no-op.
fn cache_root(env: &HashMap<String, String>) -> Option<PathBuf> {
    for key in ["npm_config_cache", "NPM_CONFIG_CACHE"] {
        if let Some(value) = env.get(key).filter(|value| !value.trim().is_empty()) {
            return Some(PathBuf::from(value));
        }
        if let Some(value) = std::env::var_os(key).filter(|value| !value.is_empty()) {
            return Some(PathBuf::from(value));
        }
    }
    if cfg!(windows) {
        dirs::data_local_dir().map(|dir| dir.join("npm-cache"))
    } else {
        dirs::home_dir().map(|dir| dir.join(".npm"))
    }
}

/// The `_npx` cache entry an npx invocation with these args and env installs
/// into, or `None` when the args name no package.
pub fn entry_dir(args: &[String], env: &HashMap<String, String>) -> Option<PathBuf> {
    let packages = packages_from_args(args);
    if packages.is_empty() {
        return None;
    }
    Some(cache_root(env)?.join("_npx").join(entry_name(&packages)))
}

/// Delete the npx cache entry this invocation runs from, so the next run
/// reinstalls it. Returns the removed directory, or `None` when the args name
/// no package, the entry does not exist, or the deletion failed.
pub async fn remove_entry(args: &[String], env: &HashMap<String, String>) -> Option<PathBuf> {
    let dir = entry_dir(args, env)?;
    // `remove_dir_all` on a computed path: refuse anything that is not shaped
    // like a cache entry, whatever the args or environment contained.
    if !is_entry_dir(&dir) || !dir.is_dir() {
        return None;
    }
    match tokio::fs::remove_dir_all(&dir).await {
        Ok(()) => {
            tracing::warn!(dir = %dir.display(), "removed npx cache entry after a failed launch");
            Some(dir)
        }
        Err(error) => {
            tracing::warn!(dir = %dir.display(), "could not remove npx cache entry: {error}");
            None
        }
    }
}

/// Whether a path is a plausible npx cache entry: a 16-hex-named directory
/// directly inside a `_npx` directory.
fn is_entry_dir(dir: &Path) -> bool {
    let named_like_entry = dir
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.len() == 16 && name.bytes().all(|b| b.is_ascii_hexdigit()));
    let inside_npx_cache = dir
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|name| name == "_npx");
    named_like_entry && inside_npx_cache
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
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

    /// The entry names from issue #896, taken from the reporter's error output
    /// and confirmed against a real npm 11 cache. If this derivation drifts
    /// from libnpmexec's, removal would delete nothing and the retry would
    /// repeat the same failure.
    #[test]
    fn entry_names_match_real_npx_cache_dirs() {
        assert_eq!(
            entry_name(&strings(&["@agentclientprotocol/codex-acp"])),
            "4877722a062902ce"
        );
        assert_eq!(
            entry_name(&strings(&["@agentclientprotocol/claude-agent-acp"])),
            "d820eb7d96bc2600"
        );
    }

    #[tokio::test]
    async fn removes_the_entry_the_invocation_runs_from() {
        let temp = tempfile::tempdir().expect("tempdir");
        let args = strings(&["-y", "removal-test-package"]);
        let env = HashMap::from([(
            "npm_config_cache".to_string(),
            temp.path().to_string_lossy().into_owned(),
        )]);
        let entry = temp
            .path()
            .join("_npx")
            .join(entry_name(&strings(&["removal-test-package"])));
        std::fs::create_dir_all(entry.join("node_modules")).expect("create entry");

        assert_eq!(
            remove_entry(&args, &env).await.as_deref(),
            Some(entry.as_path())
        );
        assert!(!entry.exists());
        // Nothing left to remove: the caller learns there was no repair to
        // make and does not retry.
        assert_eq!(remove_entry(&args, &env).await, None);
    }

    #[tokio::test]
    async fn ignores_invocations_that_name_no_package() {
        let temp = tempfile::tempdir().expect("tempdir");
        let env = HashMap::from([(
            "npm_config_cache".to_string(),
            temp.path().to_string_lossy().into_owned(),
        )]);
        assert_eq!(remove_entry(&strings(&["-y"]), &env).await, None);
    }

    #[test]
    fn only_cache_entry_shaped_paths_are_removable() {
        assert!(is_entry_dir(Path::new(
            "/home/user/.npm/_npx/4877722a062902ce"
        )));
        assert!(!is_entry_dir(Path::new("/home/user/.npm/_npx/not-a-hash")));
        assert!(!is_entry_dir(Path::new("/home/user/.npm/4877722a062902ce")));
        assert!(!is_entry_dir(Path::new("/")));
    }
}
