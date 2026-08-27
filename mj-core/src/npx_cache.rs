//! Clearing the npx cache entry behind a bundled command.
//!
//! npx reuses whatever sits in a cache entry as long as its tree contains a
//! manifest satisfying the spec; it never checks that the install finished. An
//! install interrupted partway through therefore leaves a tree that every later
//! run fails on, and npm neither validates completeness nor offers a flag to
//! bypass a broken entry (issue #896).
//!
//! npm does supply the recovery, as two commands this module drives and does
//! not second-guess: `npm cache npx ls` names the cached entries, and
//! `npm cache npx rm <key>` removes one. npm owns where its cache lives, what
//! is in it, and the deletion; nothing about any of that is derived here.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

/// How long to wait for an `npm cache npx` command. Generous next to node's
/// startup cost, and it only delays a launch that has already failed.
const NPM_TIMEOUT: Duration = Duration::from_secs(30);

/// Clear the npx cache entry this invocation runs from, so the next run
/// reinstalls it.
///
/// Returns whether npm removed an entry, which is how a caller tells "there
/// was a cached install to clear" from "nothing to do here".
pub async fn remove_entry(
    npx_command: &Path,
    args: &[String],
    env: &HashMap<String, String>,
) -> bool {
    let Some(npm) = sibling_npm(npx_command) else {
        tracing::warn!(
            npx = %npx_command.display(),
            "no npm beside npx; leaving the npx cache alone"
        );
        return false;
    };
    let Some(key) = entry_key(&npm, args, env).await else {
        tracing::debug!("no npx cache entry matches this command; nothing to clear");
        return false;
    };
    match run_npm(&npm, &["cache", "npx", "rm", &key], env).await {
        Some(output) if output.status.success() => {
            tracing::warn!(
                key = %key,
                "cleared npx cache entry after a failed launch: {}",
                String::from_utf8_lossy(&output.stdout).trim()
            );
            true
        }
        Some(output) => {
            tracing::warn!(
                key = %key,
                "npm did not remove the npx cache entry: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
            false
        }
        None => false,
    }
}

/// Ask npm which cache entry belongs to this command.
///
/// `npm cache npx ls` prints `<key>: <spec>[, <spec>...]`, echoing the specs as
/// the invocation gave them, so the entry is found by looking for the one whose
/// specs this command passed — no knowledge of how npm names or locates entries.
///
/// Entries npm reports as `(empty/invalid)` or `(unknown)` name no spec and are
/// skipped. They are also not the stuck case: an entry npm cannot read a
/// manifest from is one npx reinstalls of its own accord.
async fn entry_key(npm: &Path, args: &[String], env: &HashMap<String, String>) -> Option<String> {
    let output = run_npm(npm, &["cache", "npx", "ls"], env).await?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| {
            let (key, specs) = line.split_once(": ")?;
            let specs: Vec<&str> = specs.split(", ").map(str::trim).collect();
            let names_a_spec = specs.iter().all(|spec| !spec.starts_with('('));
            (names_a_spec && specs.iter().all(|spec| command_passed_spec(args, spec)))
                .then(|| key.trim().to_string())
        })
}

/// Whether this command passed `spec` as a package to install: on its own, or
/// as the value of npx's `--package`/`-p`.
fn command_passed_spec(args: &[String], spec: &str) -> bool {
    args.iter().enumerate().any(|(index, arg)| {
        arg == spec
            || arg.strip_prefix("--package=") == Some(spec)
            || arg.strip_prefix("-p=") == Some(spec)
            || ((arg == "--package" || arg == "-p")
                && args.get(index + 1).is_some_and(|next| next == spec))
    })
}

async fn run_npm(
    npm: &Path,
    args: &[&str],
    env: &HashMap<String, String>,
) -> Option<std::process::Output> {
    let mut command = Command::new(npm);
    command
        .args(args)
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    match tokio::time::timeout(NPM_TIMEOUT, command.output()).await {
        Ok(Ok(output)) => Some(output),
        Ok(Err(error)) => {
            tracing::warn!("could not run `npm {}`: {error}", args.join(" "));
            None
        }
        Err(_) => {
            tracing::warn!("`npm {}` timed out", args.join(" "));
            None
        }
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

    /// Real `npm cache npx ls` output, trimmed: the two bundled adapters as mj
    /// launches them, other tools' pinned specs for the same packages, a
    /// multi-package entry, and the two states that name no spec.
    const LS_OUTPUT: &str = "\
0e146165406b1119: @agentclientprotocol/claude-agent-acp@0.44.0
0e9501d4069152f5: @hey-api/openapi-ts@0.99.0, typescript@6.0.3
4877722a062902ce: @agentclientprotocol/codex-acp
ba3092411dcdc02f: (empty/invalid)
d6d842980a021838: (unknown)
d820eb7d96bc2600: @agentclientprotocol/claude-agent-acp
";

    /// A stand-in `npx` with an `npm` beside it that answers `ls` with
    /// [`LS_OUTPUT`], records any other invocation's arguments, and exits with
    /// `rm_exit_code` for those. Returns the fake npx path and the record file.
    #[cfg(unix)]
    fn fake_npm_pair(dir: &Path, rm_exit_code: u8) -> (PathBuf, PathBuf) {
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
                "#!/bin/sh\nif [ \"$3\" = ls ]; then cat <<'EOF'\n{LS_OUTPUT}EOF\n  exit 0\nfi\n\
                 echo \"$@\" > '{}'\nexit {rm_exit_code}\n",
                recorded.display()
            ),
        )
        .expect("write npm");
        for path in [&npx, &npm] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        (npx, recorded)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn removes_the_entry_listed_for_this_commands_spec() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (npx, recorded) = fake_npm_pair(temp.path(), 0);

        // Claude: the spec is a bare argument. The pinned entries for the same
        // package belong to other tools and must not be touched.
        assert!(
            remove_entry(
                &npx,
                &strings(&["-y", "@agentclientprotocol/claude-agent-acp", "--cli"]),
                &HashMap::new()
            )
            .await
        );
        assert_eq!(
            std::fs::read_to_string(&recorded).expect("recorded").trim(),
            "cache npx rm d820eb7d96bc2600"
        );

        // Codex: the spec arrives through --package=.
        assert!(
            remove_entry(
                &npx,
                &strings(&["--yes", "--package=@agentclientprotocol/codex-acp", "codex"]),
                &HashMap::new()
            )
            .await
        );
        assert_eq!(
            std::fs::read_to_string(&recorded).expect("recorded").trim(),
            "cache npx rm 4877722a062902ce"
        );
    }

    /// Nothing cached for this command means the failure was not a broken
    /// install, so there is nothing to clear and nothing to retry.
    #[cfg(unix)]
    #[tokio::test]
    async fn reports_no_removal_when_no_entry_matches() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (npx, recorded) = fake_npm_pair(temp.path(), 0);

        assert!(
            !remove_entry(
                &npx,
                &strings(&["-y", "some-other-package"]),
                &HashMap::new()
            )
            .await
        );
        assert!(!recorded.exists(), "rm must not run without a match");
    }

    /// A failing `rm` is reported as no removal, so the caller does not retry.
    #[cfg(unix)]
    #[tokio::test]
    async fn reports_no_removal_when_npm_rm_fails() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (npx, _) = fake_npm_pair(temp.path(), 1);

        assert!(
            !remove_entry(
                &npx,
                &strings(&["-y", "@agentclientprotocol/claude-agent-acp"]),
                &HashMap::new()
            )
            .await
        );
    }

    #[tokio::test]
    async fn does_nothing_without_an_npm_beside_npx() {
        let temp = tempfile::tempdir().expect("tempdir");
        assert!(
            !remove_entry(
                &temp.path().join("bin").join("npx"),
                &strings(&["-y", "pkg"]),
                &HashMap::new()
            )
            .await
        );
    }

    #[test]
    fn matches_specs_however_the_command_passed_them() {
        let args = strings(&["--package", "separated", "-p=joined", "bare", "--yes"]);
        for spec in ["separated", "joined", "bare"] {
            assert!(command_passed_spec(&args, spec), "{spec}");
        }
        // A pinned spec is a different entry belonging to a different command.
        assert!(!command_passed_spec(&args, "bare@1.2.3"));
        assert!(!command_passed_spec(&args, "never-passed"));
    }
}
