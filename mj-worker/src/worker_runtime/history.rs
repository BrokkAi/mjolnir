//! Transient history requests. No history payload is written to worker state.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result, ensure};
use mj_core::history::*;
use tokio::sync::oneshot;

struct Pending {
    request: HistoryRequest,
    expires: Instant,
    reply: oneshot::Sender<HistoryResult>,
}

#[derive(Clone)]
pub(super) struct HistoryEndpoint {
    pending: Arc<Mutex<BTreeMap<String, Pending>>>,
    permits: Arc<tokio::sync::Semaphore>,
}

impl Default for HistoryEndpoint {
    fn default() -> Self {
        Self {
            pending: Default::default(),
            permits: Arc::new(tokio::sync::Semaphore::new(MAX_PENDING)),
        }
    }
}

impl HistoryEndpoint {
    pub fn requests(&self) -> Vec<HistoryRequest> {
        let mut pending = self.pending.lock().expect("history queue lock poisoned");
        pending.retain(|_, entry| entry.expires > Instant::now() && !entry.reply.is_closed());
        pending
            .values()
            .map(|entry| entry.request.clone())
            .collect()
    }

    pub fn complete(&self, result: HistoryResult) {
        if let Some(entry) = self
            .pending
            .lock()
            .expect("history queue lock poisoned")
            .remove(&result.request_id)
            && entry.reply.send(result).is_err()
        {
            tracing::debug!("history caller disconnected before its response");
        }
    }

    pub async fn query(&self, query: HistoryQuery, cwd: &Path) -> Result<HistoryResult> {
        query.validate()?;
        let _permit = self
            .permits
            .try_acquire()
            .context("too many pending history queries; retry shortly")?;
        let id = mj_core::state::new_session_id()?;
        let deadline = tokio::time::Instant::now() + QUERY_TIMEOUT;
        let blame = if let HistoryQuery::BlameFile {
            path,
            start_line,
            end_line,
        } = &query
        {
            Some(
                tokio::time::timeout_at(deadline, capture_blame(cwd, path, *start_line, *end_line))
                    .await
                    .context("target Git blame timed out")??,
            )
        } else {
            None
        };
        let (reply, received) = oneshot::channel();
        {
            let mut pending = self.pending.lock().expect("history queue lock poisoned");
            pending.retain(|_, entry| entry.expires > Instant::now() && !entry.reply.is_closed());
            ensure!(
                pending.len() < MAX_PENDING,
                "too many pending history queries; retry shortly"
            );
            pending.insert(
                id.clone(),
                Pending {
                    request: HistoryRequest {
                        request_id: id.clone(),
                        query,
                        blame,
                    },
                    expires: deadline.into_std(),
                    reply,
                },
            );
        }
        struct Cleanup(HistoryEndpoint, String);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                self.0
                    .pending
                    .lock()
                    .expect("history queue lock poisoned")
                    .remove(&self.1);
            }
        }
        let _cleanup = Cleanup(self.clone(), id);
        tokio::time::timeout_at(deadline, received)
            .await
            .context("history query timed out; controller may be disconnected")?
            .context("history response channel closed")
    }
}

async fn git(cwd: &Path, args: &[std::ffi::OsString]) -> Result<String> {
    let mut command = tokio::process::Command::new("git");
    command.current_dir(cwd).args([
        "--no-pager",
        "-c",
        "core.fsmonitor=",
        "-c",
        "core.hooksPath=",
    ]);
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            command.env_remove(key);
        }
    }
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .args(args);
    let output = mj_core::subprocess::run_bounded(
        &mut command,
        MAX_BLAME_BYTES,
        std::time::Duration::from_secs(20),
    )
    .await?;
    ensure!(
        output.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).context("Git output is not UTF-8")
}

async fn capture_blame(cwd: &Path, path: &Path, start: usize, end: usize) -> Result<BlameEvidence> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let path = tokio::fs::canonicalize(path)
        .await
        .context("resolve target file for blame")?;
    let parent = path.parent().context("file has no parent")?;
    let root = git(parent, &["rev-parse".into(), "--show-toplevel".into()]).await?;
    let repository = PathBuf::from(root.trim());
    let relative_path = path
        .strip_prefix(&repository)
        .context("file is outside Git repository")?
        .to_path_buf();
    let porcelain = git(
        &repository,
        &[
            "blame".into(),
            "--line-porcelain".into(),
            "-M".into(),
            "-C".into(),
            format!("-L{start},{end}").into(),
            "--".into(),
            relative_path.as_os_str().to_owned(),
        ],
    )
    .await?;
    Ok(BlameEvidence {
        repository,
        relative_path,
        porcelain,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn blame_reads_the_target_checkout_including_uncommitted_lines_and_reports_errors() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path();
        crate::test_support::git(repo, &["init", "-q"]);
        crate::test_support::git(repo, &["config", "user.name", "Test"]);
        crate::test_support::git(repo, &["config", "user.email", "test@example.test"]);
        std::fs::write(repo.join("a.rs"), "committed\nold\n").unwrap();
        crate::test_support::git(repo, &["add", "a.rs"]);
        crate::test_support::git(repo, &["commit", "-qm", "initial"]);
        std::fs::write(repo.join("a.rs"), "committed\nuncommitted\n").unwrap();
        let evidence = capture_blame(repo, Path::new("a.rs"), 1, 2).await.unwrap();
        assert_eq!(evidence.relative_path, Path::new("a.rs"));
        assert!(evidence.porcelain.contains("\tcommitted"));
        assert!(
            evidence
                .porcelain
                .contains("0000000000000000000000000000000000000000")
        );
        assert!(
            capture_blame(repo, Path::new("missing.rs"), 1, 1)
                .await
                .is_err()
        );
        assert!(capture_blame(repo, Path::new("a.rs"), 9, 10).await.is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn disconnected_controller_queries_expire_and_release_capacity() {
        let endpoint = HistoryEndpoint::default();
        let mut tasks = Vec::new();
        for _ in 0..MAX_PENDING {
            let endpoint = endpoint.clone();
            tasks.push(tokio::spawn(async move {
                endpoint
                    .query(
                        HistoryQuery::TraceFile {
                            path: "a.rs".into(),
                            limit: 20,
                        },
                        Path::new("/"),
                    )
                    .await
            }));
        }
        while endpoint.requests().len() != MAX_PENDING {
            tokio::task::yield_now().await;
        }
        assert!(
            endpoint
                .query(
                    HistoryQuery::TraceFile {
                        path: "a.rs".into(),
                        limit: 20
                    },
                    Path::new("/")
                )
                .await
                .unwrap_err()
                .to_string()
                .contains("too many")
        );
        tokio::time::advance(QUERY_TIMEOUT + std::time::Duration::from_secs(1)).await;
        for task in tasks {
            assert!(
                task.await
                    .unwrap()
                    .unwrap_err()
                    .to_string()
                    .contains("timed out")
            );
        }
        assert!(endpoint.requests().is_empty());
        assert_eq!(endpoint.permits.available_permits(), MAX_PENDING);
    }
    #[tokio::test]
    async fn history_replies_are_delivered_and_cancelled_requests_do_not_accumulate() {
        let endpoint = HistoryEndpoint::default();
        let request = HistoryQuery::SearchSessions {
            query: "needle".into(),
            limit: 20,
        };
        let cloned = endpoint.clone();
        let pending = tokio::spawn(async move { cloned.query(request, Path::new("/")).await });
        let requests = loop {
            let requests = endpoint.requests();
            if !requests.is_empty() {
                break requests;
            }
            tokio::task::yield_now().await;
        };
        endpoint.complete(HistoryResult {
            request_id: requests[0].request_id.clone(),
            value: serde_json::json!({"text":"x".repeat(100_000)}),
            is_error: false,
        });
        assert_eq!(
            pending.await.unwrap().unwrap().value["text"]
                .as_str()
                .unwrap()
                .len(),
            100_000
        );
        assert!(endpoint.requests().is_empty());
        let cloned = endpoint.clone();
        let pending = tokio::spawn(async move {
            cloned
                .query(
                    HistoryQuery::TraceFile {
                        path: "a.rs".into(),
                        limit: 20,
                    },
                    Path::new("/"),
                )
                .await
        });
        while endpoint.requests().is_empty() {
            tokio::task::yield_now().await;
        }
        pending.abort();
        assert!(pending.await.unwrap_err().is_cancelled());
        assert!(endpoint.requests().is_empty());
    }
}
