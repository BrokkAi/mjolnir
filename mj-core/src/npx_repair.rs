//! One-shot recovery for corrupted npx cache entries.
//!
//! npx materializes each package spec into `<npm-cache>/_npx/<hash>` and
//! reuses that tree whenever it contains a manifest satisfying the spec — it
//! never checks that the install actually *completed*. An interrupted install
//! (Ctrl+C, crash, kill) therefore leaves a half-extracted tree that every
//! subsequent invocation trusts and fails on, forever: npm offers no automatic
//! repair and no flag to bypass a broken entry (see issue #896; both reported
//! failures printed the poisoned `_npx` path verbatim).
//!
//! The supported recovery is deleting the entry (`npm cache npx rm <key>`),
//! which this module automates with a deliberately narrow trigger: a launch
//! must have failed AND its captured output must reference an `_npx` path.
//! Auth declines, cancellations, and protocol errors never mention the cache,
//! so healthy entries are never touched. Deleting an entry is cheap to redo:
//! npm's content store (`_cacache`) is checksummed and unaffected, so the
//! reinstall re-extracts locally instead of re-downloading.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha512};

/// Minimum time between repairs of the same cache entry. A repair that did
/// not fix the failure means the cache was not the problem; without the
/// cooldown every retried sign-in would pointlessly delete and reinstall.
const REPAIR_COOLDOWN: Duration = Duration::from_secs(10 * 60);

/// Extract the npm package specs from an npx argument list, mirroring how
/// libnpmexec decides what to install: `--package`/`-p` values win, otherwise
/// the first positional argument is the spec and everything after it belongs
/// to the launched program.
pub fn npx_packages_from_args(args: &[String]) -> Vec<String> {
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
            if positional.is_none() {
                positional = iter.next().cloned();
            }
            break;
        } else if arg.starts_with('-') {
            // Boolean flag (-y, --yes, ...); npx flags that take separate
            // values are handled above.
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

/// The `_npx` entry directory name for a set of package specs: sha512 over
/// the sorted specs joined by newlines, first 16 hex chars — the same
/// derivation libnpmexec uses to pick its install dir.
pub fn npx_cache_entry_name(packages: &[String]) -> String {
    let mut sorted: Vec<&str> = packages.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    let digest = Sha512::digest(sorted.join("\n").as_bytes());
    let mut hex = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

/// npm cache root as npx would resolve it: `npm_config_cache` from the spawn
/// env or process env, else the platform default (`~/.npm` on Unix,
/// `%LocalAppData%\npm-cache` on Windows). A user-level `.npmrc` override is
/// not consulted; in that case the computed entry simply does not exist and
/// repair is a no-op.
fn npm_cache_root(env: &HashMap<String, String>) -> Option<PathBuf> {
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

/// The `_npx` cache entry an npx invocation with these args and env would
/// install into, or `None` when the args name no package.
pub fn npx_entry_dir(args: &[String], env: &HashMap<String, String>) -> Option<PathBuf> {
    let packages = npx_packages_from_args(args);
    if packages.is_empty() {
        return None;
    }
    let root = npm_cache_root(env)?;
    Some(root.join("_npx").join(npx_cache_entry_name(&packages)))
}

/// Whether failure output points at the npx cache. Both corruption symptoms
/// print the offending `_npx` path verbatim: npm errors name the file they
/// tripped on, and Node spawn/module errors name the script or binary path.
pub fn output_mentions_npx_cache(output: &str) -> bool {
    output.contains("_npx/") || output.contains("_npx\\")
}

/// One-shot repair: when a failed npx launch's output references the npx
/// cache, delete the entry this invocation resolves to so the next launch
/// reinstalls it cleanly. Returns the removed directory, or `None` when the
/// failure does not implicate the cache, the entry does not exist, or the
/// entry was already repaired within [`REPAIR_COOLDOWN`].
pub async fn repair_after_failure(
    args: &[String],
    env: &HashMap<String, String>,
    failure_output: &str,
) -> Option<PathBuf> {
    if !output_mentions_npx_cache(failure_output) {
        return None;
    }
    let dir = npx_entry_dir(args, env)?;
    if !entry_dir_shape_is_safe(&dir) || !dir.is_dir() {
        return None;
    }
    {
        static RECENT: LazyLock<Mutex<HashMap<PathBuf, Instant>>> = LazyLock::new(Mutex::default);
        let mut recent = RECENT.lock().expect("npx repair cooldown lock");
        let now = Instant::now();
        if let Some(last) = recent.get(&dir)
            && now.duration_since(*last) < REPAIR_COOLDOWN
        {
            return None;
        }
        recent.insert(dir.clone(), now);
    }
    match tokio::fs::remove_dir_all(&dir).await {
        Ok(()) => {
            tracing::warn!(
                dir = %dir.display(),
                "removed corrupted npx cache entry after launch failure"
            );
            Some(dir)
        }
        Err(error) => {
            tracing::warn!(
                dir = %dir.display(),
                "failed to remove corrupted npx cache entry: {error}"
            );
            None
        }
    }
}

/// Defense in depth before `remove_dir_all`: only ever delete a direct child
/// of a `_npx` directory whose name is the 16-hex entry hash.
fn entry_dir_shape_is_safe(dir: &Path) -> bool {
    let name_ok = dir
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.len() == 16 && name.bytes().all(|b| b.is_ascii_hexdigit()));
    let parent_ok = dir
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|name| name == "_npx");
    name_ok && parent_ok
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
            npx_packages_from_args(&strings(&[
                "-y",
                "@agentclientprotocol/claude-agent-acp",
                "--cli"
            ])),
            vec!["@agentclientprotocol/claude-agent-acp"]
        );
        assert_eq!(
            npx_packages_from_args(&strings(&[
                "--yes",
                "--package=@agentclientprotocol/codex-acp",
                "codex"
            ])),
            vec!["@agentclientprotocol/codex-acp"]
        );
        assert_eq!(
            npx_packages_from_args(&strings(&["--package", "left", "-p=right", "bin"])),
            vec!["left", "right"]
        );
        assert!(npx_packages_from_args(&strings(&["-y", "--yes"])).is_empty());
    }

    /// The two entry names from issue #896, verified against a real npm 11
    /// cache: the computed names must match what npx creates or repair would
    /// delete nothing.
    #[test]
    fn entry_names_match_real_npx_cache_dirs() {
        assert_eq!(
            npx_cache_entry_name(&strings(&["@agentclientprotocol/codex-acp"])),
            "4877722a062902ce"
        );
        assert_eq!(
            npx_cache_entry_name(&strings(&["@agentclientprotocol/claude-agent-acp"])),
            "d820eb7d96bc2600"
        );
    }

    #[test]
    fn signature_requires_npx_path_in_output() {
        assert!(output_mentions_npx_cache(
            "npm error dest /Users/dave/.npm/_npx/4877722a062902ce/node_modules/@agentclientprotocol/.codex-app-tiK2rSoF"
        ));
        assert!(output_mentions_npx_cache(
            "at file:///Users/dave/.npm/_npx/d820eb7d96bc2600/node_modules/@agentclientprotocol/claude-agent-acp/dist/index.js:13:19"
        ));
        assert!(output_mentions_npx_cache(
            r"npm error path C:\Users\dave\AppData\Local\npm-cache\_npx\4877722a062902ce"
        ));
        assert!(!output_mentions_npx_cache("Invalid API key"));
        assert!(!output_mentions_npx_cache(
            "OpenAI / ChatGPT sign-in cancelled"
        ));
    }

    #[tokio::test]
    async fn repairs_once_and_respects_cooldown() {
        let temp = tempfile::tempdir().expect("tempdir");
        let args = strings(&["-y", "cooldown-test-package"]);
        let mut env = HashMap::new();
        env.insert(
            "npm_config_cache".to_string(),
            temp.path().to_string_lossy().into_owned(),
        );
        let entry = temp
            .path()
            .join("_npx")
            .join(npx_cache_entry_name(&strings(&["cooldown-test-package"])));
        std::fs::create_dir_all(entry.join("node_modules")).expect("create entry");

        let failure = format!("npm error dest {}", entry.display());
        let repaired = repair_after_failure(&args, &env, &failure).await;
        assert_eq!(repaired.as_deref(), Some(entry.as_path()));
        assert!(!entry.exists());

        // Recreate the entry: within the cooldown the same failure must not
        // delete it again.
        std::fs::create_dir_all(entry.join("node_modules")).expect("recreate entry");
        assert_eq!(repair_after_failure(&args, &env, &failure).await, None);
        assert!(entry.exists());
    }

    #[tokio::test]
    async fn never_repairs_without_cache_signature_or_entry() {
        let temp = tempfile::tempdir().expect("tempdir");
        let args = strings(&["-y", "signature-test-package"]);
        let mut env = HashMap::new();
        env.insert(
            "npm_config_cache".to_string(),
            temp.path().to_string_lossy().into_owned(),
        );
        let entry = temp
            .path()
            .join("_npx")
            .join(npx_cache_entry_name(&strings(&["signature-test-package"])));
        std::fs::create_dir_all(&entry).expect("create entry");

        // Failure output that never mentions the cache: entry stays.
        assert_eq!(
            repair_after_failure(&args, &env, "login exited with exit status: 1").await,
            None
        );
        assert!(entry.exists());

        // Cache-shaped failure but no entry on disk: nothing to repair.
        std::fs::remove_dir_all(&entry).expect("remove entry");
        assert_eq!(
            repair_after_failure(&args, &env, "npm error path _npx/deadbeef").await,
            None
        );
    }

    #[test]
    fn entry_dir_shape_guard_rejects_unexpected_paths() {
        assert!(entry_dir_shape_is_safe(Path::new(
            "/home/user/.npm/_npx/4877722a062902ce"
        )));
        assert!(!entry_dir_shape_is_safe(Path::new(
            "/home/user/.npm/_npx/not-a-hash"
        )));
        assert!(!entry_dir_shape_is_safe(Path::new(
            "/home/user/.npm/4877722a062902ce"
        )));
        assert!(!entry_dir_shape_is_safe(Path::new("/")));
    }
}
