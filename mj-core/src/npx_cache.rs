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
//!
//! Finding the entry means knowing where npm keeps its cache, which is
//! configurable in every npmrc layer as well as the environment. npm is asked
//! directly rather than guessing, so a custom cache location does not quietly
//! turn recovery off.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use sha2::{Digest, Sha512};
use tokio::process::Command;

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

/// How long to wait for `npm config get cache`. Generous next to node's
/// startup cost, and it only delays a launch that has already failed.
const NPM_CONFIG_TIMEOUT: Duration = Duration::from_secs(15);

/// Ask npm where its cache lives.
///
/// `cache` is settable in every npmrc layer (project, user, global, builtin)
/// as well as the environment, and an npmrc value wins whenever
/// `npm_config_cache` is unset — so reading the environment alone would send
/// removal to the wrong directory for anyone who configured a custom cache,
/// silently turning the retry off. npm resolves that whole stack itself, so
/// ask it rather than re-implementing the precedence.
///
/// Uses the npm beside the npx that ran, so an embedded Node install answers
/// for its own cache, and inherits this process's working directory, so a
/// project-level npmrc resolves the same way it did for the failed launch.
async fn cache_root_from_npm(npx_command: &Path, env: &HashMap<String, String>) -> Option<PathBuf> {
    let npm = sibling_npm(npx_command)?;
    let mut command = Command::new(npm);
    command
        .args(["config", "get", "cache"])
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let output = tokio::time::timeout(NPM_CONFIG_TIMEOUT, command.output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if value.is_empty() || value == "undefined" || value == "null" {
        return None;
    }
    Some(PathBuf::from(value))
}

/// The `npm` shipped alongside an `npx`, or `None` when it is not there.
fn sibling_npm(npx_command: &Path) -> Option<PathBuf> {
    let file_name = npx_command.file_name()?.to_str()?;
    let npm_name = match file_name {
        "npx" => "npm",
        "npx.cmd" => "npm.cmd",
        "npx.exe" => "npm.exe",
        _ => return None,
    };
    let npm = npx_command.parent()?.join(npm_name);
    npm.is_file().then_some(npm)
}

/// Fallback when npm cannot be asked: `npm_config_cache` from the spawn env or
/// this process's env, else the platform default.
fn cache_root_fallback(env: &HashMap<String, String>) -> Option<PathBuf> {
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

/// The `_npx` cache entry an npx invocation installs into, or `None` when the
/// args name no package or the cache root cannot be determined.
pub async fn entry_dir(
    npx_command: &Path,
    args: &[String],
    env: &HashMap<String, String>,
) -> Option<PathBuf> {
    let packages = packages_from_args(args);
    if packages.is_empty() {
        return None;
    }
    let root = match cache_root_from_npm(npx_command, env).await {
        Some(root) => root,
        None => cache_root_fallback(env)?,
    };
    Some(root.join("_npx").join(entry_name(&packages)))
}

/// Delete the npx cache entry this invocation runs from, so the next run
/// reinstalls it. Returns the removed directory, or `None` when the args name
/// no package, the entry does not exist, or the deletion failed.
pub async fn remove_entry(
    npx_command: &Path,
    args: &[String],
    env: &HashMap<String, String>,
) -> Option<PathBuf> {
    let dir = entry_dir(npx_command, args, env).await?;
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

    /// A stand-in `npx` with an `npm` beside it that reports `cache_path`,
    /// standing for any npm whose cache comes from an npmrc rather than the
    /// environment. Returns the fake npx path.
    #[cfg(unix)]
    fn fake_npm_pair(dir: &Path, cache_path: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).expect("create bin");
        let npx = bin.join("npx");
        std::fs::write(&npx, "#!/bin/sh\nexit 1\n").expect("write npx");
        let npm = bin.join("npm");
        std::fs::write(
            &npm,
            format!("#!/bin/sh\necho '{}'\n", cache_path.display()),
        )
        .expect("write npm");
        for path in [&npx, &npm] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        npx
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

        let npx = PathBuf::from("npx");
        assert_eq!(
            remove_entry(&npx, &args, &env).await.as_deref(),
            Some(entry.as_path())
        );
        assert!(!entry.exists());
        // Nothing left to remove: the caller learns there was no repair to
        // make and does not retry.
        assert_eq!(remove_entry(&npx, &args, &env).await, None);
    }

    /// npm's own answer wins over the environment, so a cache configured in an
    /// npmrc is still found. Without this the entry would be looked for under
    /// the env/default root, nothing would be removed, and the retry would
    /// never happen.
    #[cfg(unix)]
    #[tokio::test]
    async fn uses_the_cache_root_npm_reports_over_the_environment() {
        let temp = tempfile::tempdir().expect("tempdir");
        let npmrc_cache = temp.path().join("npmrc-cache");
        let npx = fake_npm_pair(temp.path(), &npmrc_cache);
        let args = strings(&["-y", "npmrc-test-package"]);
        // A different root in the environment, standing for the default npm
        // would ignore in favour of its npmrc.
        let env = HashMap::from([(
            "npm_config_cache".to_string(),
            temp.path().join("env-cache").to_string_lossy().into_owned(),
        )]);

        let entry = npmrc_cache
            .join("_npx")
            .join(entry_name(&strings(&["npmrc-test-package"])));
        std::fs::create_dir_all(entry.join("node_modules")).expect("create entry");

        assert_eq!(
            entry_dir(&npx, &args, &env).await.as_deref(),
            Some(entry.as_path())
        );
        assert_eq!(
            remove_entry(&npx, &args, &env).await.as_deref(),
            Some(entry.as_path())
        );
        assert!(!entry.exists());
    }

    /// With no npm beside npx to ask, the environment is still honoured.
    #[tokio::test]
    async fn falls_back_to_the_environment_when_npm_cannot_be_asked() {
        let temp = tempfile::tempdir().expect("tempdir");
        let env = HashMap::from([(
            "npm_config_cache".to_string(),
            temp.path().to_string_lossy().into_owned(),
        )]);
        let expected = temp
            .path()
            .join("_npx")
            .join(entry_name(&strings(&["fallback-test-package"])));
        assert_eq!(
            entry_dir(
                &temp.path().join("bin").join("npx"),
                &strings(&["-y", "fallback-test-package"]),
                &env
            )
            .await
            .as_deref(),
            Some(expected.as_path())
        );
    }

    #[tokio::test]
    async fn ignores_invocations_that_name_no_package() {
        let temp = tempfile::tempdir().expect("tempdir");
        let env = HashMap::from([(
            "npm_config_cache".to_string(),
            temp.path().to_string_lossy().into_owned(),
        )]);
        assert_eq!(
            remove_entry(&PathBuf::from("npx"), &strings(&["-y"]), &env).await,
            None
        );
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
