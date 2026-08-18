//! Current-branch pull request probe shared by the TUI status line and the
//! remote session tracker.
//!
//! Both surfaces show the same `PR #N` badge, so they must agree on what
//! counts as "the current PR": the open pull request that `gh` associates
//! with the checked-out branch of `cwd`.

use std::path::Path;
use std::process::Stdio;

use serde::Deserialize;

/// An open pull request associated with the current branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequest {
    pub number: u64,
    pub url: String,
}

/// One probe result. `gh_succeeded == false` means the PR state is unknown
/// (gh missing, not authenticated, no remote); callers should keep their
/// previous answer for the same branch rather than clearing the badge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchProbe {
    pub branch: Option<String>,
    pub gh_succeeded: bool,
    pub pull_request: Option<PullRequest>,
}

#[derive(Deserialize)]
struct GhPullRequestView {
    number: u64,
    url: String,
    state: String,
}

pub async fn probe_current_branch(cwd: &Path) -> BranchProbe {
    let branch = tokio::process::Command::new("git")
        .current_dir(cwd)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(["branch", "--show-current"])
        .stdin(Stdio::null())
        .output()
        .await
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|branch| branch.trim().to_string())
        .filter(|branch| !branch.is_empty());

    let gh_output = tokio::process::Command::new("gh")
        .current_dir(cwd)
        .env("GH_PROMPT_DISABLED", "1")
        .args(["pr", "view", "--json", "number,url,state"])
        .stdin(Stdio::null())
        .output()
        .await;
    let (gh_succeeded, pull_request) = match gh_output {
        Ok(output) if output.status.success() => {
            let pull_request = String::from_utf8(output.stdout)
                .ok()
                .and_then(|output| serde_json::from_str::<GhPullRequestView>(&output).ok())
                .filter(|pull_request| pull_request.state.eq_ignore_ascii_case("open"))
                .map(|pull_request| PullRequest {
                    number: pull_request.number,
                    url: pull_request.url,
                });
            (true, pull_request)
        }
        _ => (false, None),
    };

    BranchProbe {
        branch,
        gh_succeeded,
        pull_request,
    }
}
