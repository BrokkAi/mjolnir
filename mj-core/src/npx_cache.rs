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
use std::future::Future;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use tokio::process::Command;

const NPM_TIMEOUT: Duration = Duration::from_secs(30);

/// Run `attempt`; if it fails because this command's npx cache entry is a
/// half-finished install, clear the entry and run it once more. `notify` is
/// called before that second run. A second failure, or a failure with nothing
/// to clear, is returned as it is.
pub async fn run_retrying_once_after_clearing<A, F, N>(
    args: &[String],
    env: &HashMap<String, String>,
    attempt: A,
    notify: N,
) -> anyhow::Result<ExitStatus>
where
    A: FnMut() -> F,
    F: Future<Output = anyhow::Result<ExitStatus>>,
    N: FnOnce(),
{
    retry_once_after_clearing(
        attempt,
        ExitStatus::success,
        || remove_entry(args, env),
        notify,
    )
    .await
}

/// The retry policy, over any outcome and any way of clearing.
async fn retry_once_after_clearing<T, A, F, S, C, G, N>(
    mut attempt: A,
    succeeded: S,
    clear: C,
    notify: N,
) -> anyhow::Result<T>
where
    A: FnMut() -> F,
    F: Future<Output = anyhow::Result<T>>,
    S: Fn(&T) -> bool,
    C: FnOnce() -> G,
    G: Future<Output = bool>,
    N: FnOnce(),
{
    let outcome = attempt().await?;
    if succeeded(&outcome) || !clear().await {
        return Ok(outcome);
    }
    notify();
    attempt().await
}

/// Clear the npx cache entry this command runs from, so the next run
/// reinstalls it. Returns whether an entry was removed.
pub async fn remove_entry(args: &[String], env: &HashMap<String, String>) -> bool {
    let Some(npm) = crate::acp::find_npm() else {
        return false;
    };
    remove_matching_entry(args, |npm_args| npm_output(&npm, npm_args, env)).await
}

/// The `ls`-then-`rm` exchange, over any way of running npm.
async fn remove_matching_entry<R, F>(args: &[String], run_npm: R) -> bool
where
    R: Fn(Vec<String>) -> F,
    F: Future<Output = Option<String>>,
{
    let Some(listing) = run_npm(npm_args(&["cache", "npx", "ls"])).await else {
        return false;
    };
    let Some(key) = matching_key(&listing, args) else {
        return false;
    };
    let removed = run_npm(npm_args(&["cache", "npx", "rm", key]))
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

fn npm_args(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| arg.to_string()).collect()
}

/// Run npm and return its stdout, or `None` if it failed, timed out, or could
/// not start.
async fn npm_output(
    npm: &Path,
    args: Vec<String>,
    env: &HashMap<String, String>,
) -> Option<String> {
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

    /// An attempt that fails the first time and succeeds the second, counting
    /// its runs. `true` stands for a successful launch.
    fn counted_attempt(
        runs: &std::cell::Cell<usize>,
    ) -> impl FnMut() -> std::future::Ready<anyhow::Result<bool>> + '_ {
        move || {
            runs.set(runs.get() + 1);
            std::future::ready(Ok(runs.get() > 1))
        }
    }

    #[tokio::test]
    async fn clearing_an_entry_buys_exactly_one_more_attempt() {
        let runs = std::cell::Cell::new(0);
        let notified = std::cell::Cell::new(0);

        let succeeded = retry_once_after_clearing(
            counted_attempt(&runs),
            |ok: &bool| *ok,
            || std::future::ready(true),
            || notified.set(notified.get() + 1),
        )
        .await
        .expect("attempt");

        assert!(succeeded, "the retry's outcome is the one returned");
        assert_eq!(runs.get(), 2, "one retry, not a loop");
        assert_eq!(notified.get(), 1);
    }

    #[tokio::test]
    async fn does_not_retry_when_there_was_nothing_to_clear() {
        let runs = std::cell::Cell::new(0);
        let notified = std::cell::Cell::new(0);

        let succeeded = retry_once_after_clearing(
            counted_attempt(&runs),
            |ok: &bool| *ok,
            || std::future::ready(false),
            || notified.set(notified.get() + 1),
        )
        .await
        .expect("attempt");

        assert!(!succeeded, "the first failure is reported as it is");
        assert_eq!(runs.get(), 1);
        assert_eq!(notified.get(), 0);
    }

    /// A successful launch must not touch the cache at all.
    #[tokio::test]
    async fn does_not_clear_when_the_first_attempt_succeeds() {
        let runs = std::cell::Cell::new(0);
        let cleared = std::cell::Cell::new(false);

        let succeeded = retry_once_after_clearing(
            || {
                runs.set(runs.get() + 1);
                std::future::ready(Ok(true))
            },
            |ok: &bool| *ok,
            || {
                cleared.set(true);
                std::future::ready(true)
            },
            || panic!("must not notify on success"),
        )
        .await
        .expect("attempt");

        assert!(succeeded);
        assert_eq!(runs.get(), 1);
        assert!(!cleared.get(), "the npx cache is left alone");
    }

    /// Records every npm invocation and answers `ls` with [`LISTING`].
    fn recording_npm(
        calls: &std::sync::Mutex<Vec<String>>,
        rm_succeeds: bool,
    ) -> impl Fn(Vec<String>) -> std::future::Ready<Option<String>> + '_ {
        move |args: Vec<String>| {
            let call = args.join(" ");
            calls.lock().expect("calls").push(call.clone());
            std::future::ready(match call.as_str() {
                "cache npx ls" => Some(LISTING.to_string()),
                _ if rm_succeeds => Some(String::new()),
                _ => None,
            })
        }
    }

    #[tokio::test]
    async fn removes_the_matched_key_and_reports_it() {
        let calls = std::sync::Mutex::new(Vec::new());
        let args: Vec<String> = ["-y", "@agentclientprotocol/claude-agent-acp", "--cli"]
            .iter()
            .map(|a| a.to_string())
            .collect();

        assert!(remove_matching_entry(&args, recording_npm(&calls, true)).await);
        assert_eq!(
            *calls.lock().expect("calls"),
            ["cache npx ls", "cache npx rm d820eb7d96bc2600"]
        );
    }

    /// A failing `rm` must report no removal, or the caller retries a sign-in
    /// against the same broken entry.
    #[tokio::test]
    async fn reports_no_removal_when_rm_fails() {
        let calls = std::sync::Mutex::new(Vec::new());
        let args: Vec<String> = ["-y", "@agentclientprotocol/claude-agent-acp"]
            .iter()
            .map(|a| a.to_string())
            .collect();

        assert!(!remove_matching_entry(&args, recording_npm(&calls, false)).await);
        assert_eq!(
            *calls.lock().expect("calls"),
            ["cache npx ls", "cache npx rm d820eb7d96bc2600"]
        );
    }

    #[tokio::test]
    async fn runs_no_rm_when_nothing_matches() {
        let calls = std::sync::Mutex::new(Vec::new());
        let args = vec!["-y".to_string(), "some-other-package".to_string()];

        assert!(!remove_matching_entry(&args, recording_npm(&calls, true)).await);
        assert_eq!(*calls.lock().expect("calls"), ["cache npx ls"]);
    }
}
