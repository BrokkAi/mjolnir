//! Target-side semantic review execution.
use mj_review::bifrost::*;
use mj_review::{CHANGED_FUNCTIONS_LIMIT, bound_review_section};
use std::process::Stdio;
/// Runs `analyze_diff` over each repository and renders one packet.
///
/// Every repository must succeed. A partial packet would tell the supervisor
/// that a repository changed nothing when in truth Bifrost could not read it,
/// which is the fabricated-evidence failure the whole design refuses.
pub async fn changed_functions_packet(requests: &[AnalyzeRequest]) -> Result<String, String> {
    let mut sections = Vec::new();
    for request in requests {
        let analysis = analyze_diff(request).await?;
        sections.push(format!(
            "Repository: {}\n{}",
            request.repository.display(),
            format_changed_functions(&analysis)
        ));
    }
    Ok(bound_review_section(
        &sections.join("\n\n"),
        CHANGED_FUNCTIONS_LIMIT,
        "changed functions",
    ))
}

/// Runs Bifrost's one-shot `analyze_diff` for one repository.
pub async fn analyze_diff(request: &AnalyzeRequest) -> Result<AnalyzeDiffResult, String> {
    analyze_diff_with(&bifrost_binary(), request).await
}

async fn analyze_diff_with(
    binary: &std::path::Path,
    request: &AnalyzeRequest,
) -> Result<AnalyzeDiffResult, String> {
    tracing::info!(
        event = "review_analyze_diff_started",
        bifrost = %binary.display(),
        root = %request.repository.display(),
        base_tree = %request.base_tree,
        target_tree = %request.target_tree,
        "running bifrost analyze_diff for the captured turn trees"
    );
    let args = serde_json::json!({
        "base": request.base_tree,
        "target": request.target_tree,
    })
    .to_string();
    let mut command = tokio::process::Command::new(binary);
    command
        .current_dir(&request.repository)
        .kill_on_drop(true)
        // The capture ran against a scratch index; a review must never inherit
        // it, or Bifrost would read a half-written index as the repository.
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES");
    if let Some(cache) = keep_bifrost_state_out_of_worktree(&request.repository).await {
        command.env(BIFROST_CACHE_DIR_ENV, cache);
    }
    command
        .arg("--root")
        .arg(&request.repository)
        .args(["--tool", "analyze_diff", "--args"])
        .arg(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // `output()` drains stdout and stderr concurrently with the wait, so a
    // child that fills a pipe buffer cannot deadlock this call. stdin is
    // closed, so there is nothing to feed it.
    let output = match tokio::time::timeout(ANALYZE_DIFF_TIMEOUT, command.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!(
                "the review needs the `{}` binary, which this session's container image does not have; rebuild the image so it includes Bifrost",
                binary.display()
            ));
        }
        Ok(Err(error)) => return Err(format!("could not run bifrost: {error}")),
        Err(_) => {
            return Err(format!(
                "bifrost analysis of {} exceeded its {}s budget",
                request.repository.display(),
                ANALYZE_DIFF_TIMEOUT.as_secs()
            ));
        }
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("Unknown tool") {
            return Err(incompatible_bifrost(binary).await);
        }
        return Err(format!(
            "bifrost exited with {}: {}",
            output.status,
            stderr.trim()
        ));
    }
    let envelope: AnalyzeDiffEnvelope = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("invalid analyze_diff JSON: {error}"))?;
    Ok(envelope.structured_content)
}

/// Bifrost's override for where it keeps its analyzer database. Without it,
/// Bifrost writes `.bifrost/` into the reviewed worktree.
const BIFROST_CACHE_DIR_ENV: &str = "BIFROST_CACHE_DIR";

/// Keeps Bifrost's analyzer database out of the session's changes (I2-12).
///
/// Returns a per-worktree cache directory inside Git's own directory for this
/// worktree, which no diff, export, or branch includes. A Bifrost too old to
/// honor [`BIFROST_CACHE_DIR_ENV`], and the Bifrost MCP servers a reviewer
/// role starts, still write `.bifrost/` into the worktree, so this also adds
/// `/.bifrost/` to the repository's `info/exclude`: `git status`, the review
/// capture, `mj diff`, and the changed-files view then all leave it out.
pub(crate) async fn keep_bifrost_state_out_of_worktree(
    root: &std::path::Path,
) -> Option<std::path::PathBuf> {
    if let Some(exclude) = git_path(root, "info/exclude").await {
        let existing = tokio::fs::read_to_string(&exclude)
            .await
            .unwrap_or_default();
        let listed = existing.lines().any(|line| {
            matches!(
                line.trim(),
                ".bifrost" | ".bifrost/" | "/.bifrost" | "/.bifrost/"
            )
        });
        if !listed {
            let separator = if existing.is_empty() || existing.ends_with('\n') {
                ""
            } else {
                "\n"
            };
            let updated = format!("{existing}{separator}/.bifrost/\n");
            let written = match exclude.parent() {
                Some(parent) => tokio::fs::create_dir_all(parent).await,
                None => Ok(()),
            };
            if let Err(error) = match written {
                Ok(()) => tokio::fs::write(&exclude, updated).await,
                Err(error) => Err(error),
            } {
                tracing::warn!(path = %exclude.display(), %error, "could not exclude .bifrost/ from the review worktree");
            }
        }
    }
    let cache = git_path(root, "mj-bifrost-cache").await?;
    tokio::fs::create_dir_all(&cache).await.ok()?;
    Some(cache)
}

/// `git rev-parse --git-path`, resolved against `root`.
async fn git_path(root: &std::path::Path, path: &str) -> Option<std::path::PathBuf> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--git-path", path])
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_DIR")
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let resolved = String::from_utf8(output.stdout).ok()?;
    let resolved = std::path::Path::new(resolved.trim());
    Some(if resolved.is_absolute() {
        resolved.to_path_buf()
    } else {
        root.join(resolved)
    })
}

/// Explains a Bifrost that does not offer `analyze_diff`: an older release
/// found on PATH instead of the one the review was built against.
async fn incompatible_bifrost(binary: &std::path::Path) -> String {
    let version = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::process::Command::new(binary)
            .arg("--version")
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()
    .and_then(Result::ok)
    .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
    .filter(|version| !version.is_empty())
    .unwrap_or_else(|| "an unknown version".to_owned());
    format!(
        "the `{}` binary is {version}, which has no analyze_diff tool; the review needs Bifrost {REQUIRED_BIFROST_VERSION} or later (`cargo install brokk-bifrost@{REQUIRED_BIFROST_VERSION} --locked --bin bifrost`, or set {BIFROST_BIN_ENV} to a newer binary)",
        binary.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn a_missing_bifrost_binary_fails_the_review_with_the_fix() {
        // Safety: the env var is process-global; this test names a unique
        // path and does not race another test that reads it, because no other
        // test in this module spawns Bifrost.
        unsafe {
            std::env::set_var(BIFROST_BIN_ENV, "/nonexistent/hel-review-bifrost");
        }
        let error = analyze_diff(&AnalyzeRequest {
            repository: std::env::temp_dir(),
            base_tree: "base".to_string(),
            target_tree: "target".to_string(),
        })
        .await
        .expect_err("a missing binary must fail the review");
        unsafe {
            std::env::remove_var(BIFROST_BIN_ENV);
        }
        assert!(error.contains("rebuild the image"), "{error}");
    }

    /// I1-15: an old Bifrost on PATH fails every analysis with "Unknown
    /// tool: analyze_diff". The review says which binary and version it
    /// found and what it needs, rather than passing the raw error on.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_old_bifrost_is_reported_with_its_version_and_the_fix() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let fake = temp.path().join("bifrost");
        std::fs::write(
            &fake,
            "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'bifrost 0.7.5'; exit 0; fi\necho 'Unknown tool: analyze_diff' >&2\nexit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let error = analyze_diff_with(
            &fake,
            &AnalyzeRequest {
                repository: temp.path().to_path_buf(),
                base_tree: "base".to_string(),
                target_tree: "target".to_string(),
            },
        )
        .await
        .expect_err("an old bifrost must fail the analysis");
        assert!(error.contains("bifrost 0.7.5"), "{error}");
        assert!(error.contains("has no analyze_diff tool"), "{error}");
        assert!(error.contains(REQUIRED_BIFROST_VERSION), "{error}");
    }

    /// I2-12: a review left `.bifrost/analyzer.db` in the session worktree,
    /// and it showed up in `mj diff` and in the next review's capture.
    #[cfg(unix)]
    #[tokio::test]
    async fn bifrost_state_stays_out_of_the_session_changes() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .env_remove("GIT_INDEX_FILE")
                .env_remove("GIT_DIR")
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        };
        git(&["init", "-q"]);
        // A Bifrost that honors the cache override writes there; an old one
        // writes into the worktree. This fake does both, and records the
        // override it was given.
        let fake = temp.path().join("bifrost");
        let seen = temp.path().join("cache-dir");
        std::fs::write(
            &fake,
            format!(
                "#!/bin/sh\n[ \"$1\" = --version ] && exit 0\nprintf '%s' \"$BIFROST_CACHE_DIR\" > '{}'\nmkdir -p .bifrost && : > .bifrost/analyzer.db\necho 'Unknown tool: analyze_diff' >&2\nexit 1\n",
                seen.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _ = analyze_diff_with(
            &fake,
            &AnalyzeRequest {
                repository: repo.clone(),
                base_tree: "base".to_string(),
                target_tree: "target".to_string(),
            },
        )
        .await;
        let cache = std::path::PathBuf::from(std::fs::read_to_string(&seen).unwrap());
        assert!(cache.is_dir(), "{}", cache.display());
        assert!(cache.starts_with(repo.join(".git")), "{}", cache.display());
        assert!(repo.join(".bifrost/analyzer.db").exists());
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["status", "--porcelain=v1", "--untracked-files=all"])
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_DIR")
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&status.stdout), "");
        // Running again does not list the exclusion twice.
        let _ = keep_bifrost_state_out_of_worktree(&repo).await;
        let exclude = std::fs::read_to_string(repo.join(".git/info/exclude")).unwrap();
        assert_eq!(exclude.matches("/.bifrost/").count(), 1, "{exclude}");
    }
}
