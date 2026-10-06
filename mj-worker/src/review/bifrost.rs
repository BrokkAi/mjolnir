//! Keeps Bifrost's own state out of the changes a review captures.
//!
//! The Bifrost MCP servers a reviewer role starts write their analyzer
//! database to `.bifrost/` at the root they serve, which is the session's
//! worktree. Without an exclusion it would show up in `git status`, the next
//! review's capture, `mj diff`, and the changed-files view (I2-12).
use std::process::Stdio;

/// Adds `/.bifrost/` to the repository's `info/exclude`, once.
///
/// It goes in Git's own directory for this worktree, which no diff, export, or
/// branch includes, so the exclusion never becomes part of the session's work.
pub(crate) async fn exclude_bifrost_state(root: &std::path::Path) {
    let Some(exclude) = git_path(root, "info/exclude").await else {
        return;
    };
    let existing = tokio::fs::read_to_string(&exclude)
        .await
        .unwrap_or_default();
    let listed = existing.lines().any(|line| {
        matches!(
            line.trim(),
            ".bifrost" | ".bifrost/" | "/.bifrost" | "/.bifrost/"
        )
    });
    if listed {
        return;
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// I2-12: a review left `.bifrost/analyzer.db` in the session worktree,
    /// and it showed up in `mj diff` and in the next review's capture.
    // Hard-won: 9c22154b: Bifrost analyzer artifacts appeared in `mj diff` and triggered a needless later review.
    #[cfg(unix)]
    #[tokio::test]
    async fn bifrost_state_stays_out_of_the_session_changes() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["init", "-q"])
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_DIR")
            .status()
            .unwrap();
        assert!(status.success());
        exclude_bifrost_state(&repo).await;
        // What a reviewer's Bifrost server writes at the root it serves.
        std::fs::create_dir_all(repo.join(".bifrost")).unwrap();
        std::fs::write(repo.join(".bifrost/analyzer.db"), b"").unwrap();
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
        exclude_bifrost_state(&repo).await;
        let exclude = std::fs::read_to_string(repo.join(".git/info/exclude")).unwrap();
        assert_eq!(exclude.matches("/.bifrost/").count(), 1, "{exclude}");
    }
}
