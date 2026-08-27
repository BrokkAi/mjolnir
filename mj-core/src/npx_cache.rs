//! Clearing the npx cache entry behind a bundled command.
//!
//! npx reuses whatever sits in a cache entry as long as its tree contains a
//! manifest satisfying the spec, and never checks that the install finished, so
//! an interrupted install leaves a tree every later run fails on (issue #896).
//! The recovery is npm's own: `npm cache npx ls` names the entries and
//! `npm cache npx rm <key>` removes one. `ls` echoes the specs as the
//! invocation gave them, so our entry is the line whose specs we passed —
//! other tools' entries for the same package are pinned to a version and do
//! not match.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

const NPM_TIMEOUT: Duration = Duration::from_secs(30);

/// Clear the npx cache entry this command runs from, so the next run
/// reinstalls it. Returns whether an entry was removed.
pub async fn remove_entry(
    npx_command: &Path,
    args: &[String],
    env: &HashMap<String, String>,
) -> bool {
    let Some(npm) = sibling_npm(npx_command) else {
        return false;
    };
    let Some(listing) = npm_output(&npm, &["cache", "npx", "ls"], env).await else {
        return false;
    };
    let Some(key) = matching_key(&listing, args) else {
        return false;
    };
    let removed = npm_output(&npm, &["cache", "npx", "rm", key], env)
        .await
        .is_some();
    tracing::warn!(key, removed, "clearing npx cache entry after failed launch");
    removed
}

/// The key `npm cache npx ls` lists for the packages this command installs.
fn matching_key<'a>(listing: &'a str, args: &[String]) -> Option<&'a str> {
    for line in listing.lines() {
        let Some((key, specs)) = line.split_once(": ") else {
            continue;
        };
        // Entries npm cannot read a manifest from list as "(empty/invalid)" or
        // "(unknown)", which no spec of ours matches. They are also not the
        // stuck case: npx reinstalls an entry it cannot read.
        if specs.split(", ").all(|spec| passed(args, spec.trim())) {
            return Some(key.trim());
        }
    }
    None
}

/// Whether `spec` is a package this command installs: bare, or after npx's
/// `--package=`/`-p=`.
fn passed(args: &[String], spec: &str) -> bool {
    args.iter()
        .any(|arg| arg == spec || arg.strip_prefix("--package=") == Some(spec))
}

/// Run npm and return its stdout, or `None` if it failed, timed out, or could
/// not start.
async fn npm_output(npm: &Path, args: &[&str], env: &HashMap<String, String>) -> Option<String> {
    let output = tokio::time::timeout(
        NPM_TIMEOUT,
        Command::new(npm)
            .args(args)
            .envs(env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The `npm` shipped alongside an `npx`.
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

    /// Real `npm cache npx ls` output, trimmed: the bundled adapters as mj
    /// launches them, other tools' pinned specs for the same packages, a
    /// multi-package entry, and the states that name no spec.
    const LISTING: &str = "\
0e146165406b1119: @agentclientprotocol/claude-agent-acp@0.44.0
0e9501d4069152f5: @hey-api/openapi-ts@0.99.0, typescript@6.0.3
4877722a062902ce: @agentclientprotocol/codex-acp
ba3092411dcdc02f: (empty/invalid)
d6d842980a021838: (unknown)
d820eb7d96bc2600: @agentclientprotocol/claude-agent-acp
";

    fn key_for(args: &[&str]) -> Option<&'static str> {
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        matching_key(LISTING, &args)
    }

    #[test]
    fn finds_the_entry_for_each_adapters_argument_style() {
        // Claude passes the spec bare; codex passes it through --package=.
        assert_eq!(
            key_for(&["-y", "@agentclientprotocol/claude-agent-acp", "--cli"]),
            Some("d820eb7d96bc2600")
        );
        assert_eq!(
            key_for(&["--yes", "--package=@agentclientprotocol/codex-acp", "codex"]),
            Some("4877722a062902ce")
        );
    }

    #[test]
    fn ignores_entries_this_command_did_not_install() {
        // Every spec of a multi-package entry must be ours.
        assert_eq!(key_for(&["-y", "typescript@6.0.3"]), None);
        assert_eq!(key_for(&["-y", "some-other-package"]), None);
        assert_eq!(key_for(&["-y"]), None);
    }

    /// The listing holds several entries for the adapters mj launches, pinned
    /// to versions by whoever installed them. Clearing one of those would
    /// disrupt another tool and leave our own broken entry in place.
    #[test]
    fn an_unversioned_spec_never_matches_someone_elses_pinned_entry() {
        let pinned = "0e146165406b1119: @agentclientprotocol/claude-agent-acp@0.44.0\n";
        let args = vec![
            "-y".to_string(),
            "@agentclientprotocol/claude-agent-acp".to_string(),
        ];
        assert_eq!(matching_key(pinned, &args), None);
    }

    /// Entries npm cannot read a manifest from name no spec, so no real
    /// invocation matches them.
    #[test]
    fn unreadable_entries_match_nothing() {
        let unreadable = "ba3092411dcdc02f: (empty/invalid)\nd6d842980a021838: (unknown)\n";
        let args = vec![
            "-y".to_string(),
            "@agentclientprotocol/claude-agent-acp".to_string(),
        ];
        assert_eq!(matching_key(unreadable, &args), None);
    }

    #[test]
    fn finds_npm_only_beside_a_real_npx() {
        let temp = tempfile::tempdir().expect("tempdir");
        let bin = temp.path().join("bin");
        std::fs::create_dir_all(&bin).expect("create bin");
        assert_eq!(sibling_npm(&bin.join("npx")), None, "no npm on disk");
        std::fs::write(bin.join("npm"), "").expect("write npm");
        assert_eq!(sibling_npm(&bin.join("npx")), Some(bin.join("npm")));
        assert_eq!(sibling_npm(&bin.join("node")), None, "not an npx");
    }
}
