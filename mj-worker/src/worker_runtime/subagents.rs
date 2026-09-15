//! Parent-worker queue behind the Mjolnir sub-agent MCP server.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Notify;
use tokio::time::{Instant, sleep_until};

use mj_core::subagent::{MAX_WAIT_SECONDS, SubagentToolRequest, SubagentToolResult};

pub const SUBAGENT_SOCKET: &str = "subagents.sock";
const SUBAGENT_QUEUE: &str = "subagents.json";

/// How long a socket call waits for the daemon's result before giving up and
/// answering with the "still running" placeholder. The daemon bounds its
/// longest action (`wait`) to [`MAX_WAIT_SECONDS`], so this ceiling is
/// only reached if the daemon never answers; it exists so a lost daemon cannot
/// wedge the socket task forever.
const SOCKET_WAIT_CEILING: Duration = Duration::from_secs(MAX_WAIT_SECONDS + 60);

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct QueueState {
    #[serde(default)]
    requests: BTreeMap<String, SubagentToolRequest>,
    #[serde(default)]
    results: BTreeMap<String, SubagentToolResult>,
}

#[derive(Clone)]
pub struct SubagentEndpoint {
    path: PathBuf,
    state: Arc<Mutex<QueueState>>,
    /// Woken whenever a result lands, so a socket call awaiting its own request
    /// returns the moment the daemon completes it.
    completed: Arc<Notify>,
}

impl SubagentEndpoint {
    pub fn open(root: &Path) -> Result<Self> {
        let path = root.join(SUBAGENT_QUEUE);
        let state = match std::fs::read(&path) {
            Ok(body) => serde_json::from_slice(&body)
                .with_context(|| format!("parse sub-agent queue {}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => QueueState::default(),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("read sub-agent queue {}", path.display()));
            }
        };
        Ok(Self {
            path,
            state: Arc::new(Mutex::new(state)),
            completed: Arc::new(Notify::new()),
        })
    }

    fn cached_result(&self, request_id: &str) -> Option<SubagentToolResult> {
        self.state
            .lock()
            .expect("sub-agent queue lock poisoned")
            .results
            .get(request_id)
            .cloned()
    }

    /// Wait for the daemon to complete `request_id`, up to `deadline`. Returns
    /// the result, or `None` at the deadline. Interest is registered before
    /// each read of the queue, so a completion racing the check is never lost.
    pub async fn await_result(
        &self,
        request_id: &str,
        deadline: Instant,
    ) -> Option<SubagentToolResult> {
        loop {
            let notified = self.completed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(result) = self.cached_result(request_id) {
                return Some(result);
            }
            tokio::select! {
                _ = &mut notified => {}
                _ = sleep_until(deadline) => return self.cached_result(request_id),
            }
        }
    }

    pub fn snapshot(&self) -> (Vec<SubagentToolRequest>, Vec<SubagentToolResult>) {
        let state = self.state.lock().expect("sub-agent queue lock poisoned");
        (
            state.requests.values().cloned().collect(),
            state.results.values().cloned().collect(),
        )
    }

    pub fn enqueue(&self, request: SubagentToolRequest) -> Result<Option<SubagentToolResult>> {
        let mut state = self.state.lock().expect("sub-agent queue lock poisoned");
        if let Some(result) = state.results.get(&request.request_id) {
            return Ok(Some(result.clone()));
        }
        state
            .requests
            .entry(request.request_id.clone())
            .or_insert(request);
        self.persist(&state)?;
        Ok(None)
    }

    pub fn complete(&self, result: SubagentToolResult) -> Result<()> {
        let request_id = result.request_id.clone();
        let mut state = self.state.lock().expect("sub-agent queue lock poisoned");
        state.requests.remove(&request_id);
        state.results.insert(request_id, result);
        // Results are small but bound retained history so a long-lived parent does not
        // grow this control file forever.
        while state.results.len() > 256 {
            let Some(oldest) = state
                .results
                .values()
                .min_by_key(|result| result.completed_at_ms)
                .map(|result| result.request_id.clone())
            else {
                break;
            };
            state.results.remove(&oldest);
        }
        self.persist(&state)?;
        drop(state);
        self.completed.notify_waiters();
        Ok(())
    }

    fn persist(&self, state: &QueueState) -> Result<()> {
        let body = serde_json::to_vec_pretty(state)?;
        mj_core::config::atomic_write(&self.path, &body)
            .with_context(|| format!("write sub-agent queue {}", self.path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_core::subagent::SubagentToolAction;

    fn request(id: &str) -> SubagentToolRequest {
        SubagentToolRequest {
            request_id: id.into(),
            created_at_ms: 1,
            action: SubagentToolAction::ListAgents,
        }
    }

    #[test]
    fn queue_survives_reopen_and_returns_cached_completion_idempotently() {
        let directory = tempfile::tempdir().unwrap();
        let endpoint = SubagentEndpoint::open(directory.path()).unwrap();
        assert_eq!(endpoint.enqueue(request("request-1")).unwrap(), None);
        assert_eq!(endpoint.snapshot().0, vec![request("request-1")]);

        let result = SubagentToolResult {
            request_id: "request-1".into(),
            completed_at_ms: 2,
            is_error: false,
            message: "done".into(),
        };
        endpoint.complete(result.clone()).unwrap();

        let reopened = SubagentEndpoint::open(directory.path()).unwrap();
        assert!(reopened.snapshot().0.is_empty());
        assert_eq!(
            reopened.enqueue(request("request-1")).unwrap(),
            Some(result)
        );
    }

    fn done(id: &str) -> SubagentToolResult {
        SubagentToolResult {
            request_id: id.into(),
            completed_at_ms: 2,
            is_error: false,
            message: "done".into(),
        }
    }

    #[tokio::test]
    async fn a_waiting_socket_call_returns_the_daemon_result_when_it_lands() {
        let directory = tempfile::tempdir().unwrap();
        let endpoint = SubagentEndpoint::open(directory.path()).unwrap();
        assert_eq!(endpoint.enqueue(request("r1")).unwrap(), None);
        let waiter = tokio::spawn({
            let endpoint = endpoint.clone();
            async move {
                endpoint
                    .await_result("r1", Instant::now() + Duration::from_secs(5))
                    .await
            }
        });
        // Let the waiter register its interest before the result lands.
        tokio::task::yield_now().await;
        endpoint.complete(done("r1")).unwrap();
        assert_eq!(waiter.await.unwrap(), Some(done("r1")));
    }

    #[tokio::test]
    async fn a_waiting_socket_call_gives_up_at_its_deadline() {
        let directory = tempfile::tempdir().unwrap();
        let endpoint = SubagentEndpoint::open(directory.path()).unwrap();
        assert_eq!(endpoint.enqueue(request("r1")).unwrap(), None);
        let result = endpoint
            .await_result("r1", Instant::now() + Duration::from_millis(50))
            .await;
        assert_eq!(result, None);
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SocketReply {
    accepted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    result: Option<SubagentToolResult>,
}

pub(super) fn serve(root: &Path) -> Result<(SubagentEndpoint, super::unix::SocketGuard)> {
    let endpoint = SubagentEndpoint::open(root)?;
    let path = root.join(SUBAGENT_SOCKET);
    let _ = std::fs::remove_file(&path);
    let listener = mj_core::local_sockets::bind_unix_listener(&path)
        .with_context(|| format!("bind sub-agent socket {}", path.display()))?;
    listener
        .set_nonblocking(true)
        .with_context(|| format!("set sub-agent socket {} nonblocking", path.display()))?;
    let listener = UnixListener::from_std(listener)
        .with_context(|| format!("register sub-agent socket {}", path.display()))?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    let service = endpoint.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let service = service.clone();
            tokio::spawn(async move {
                if let Err(error) = serve_one(stream, service).await {
                    tracing::warn!(error = %format!("{error:#}"), "sub-agent MCP dispatch failed");
                }
            });
        }
    });
    Ok((endpoint, super::unix::SocketGuard(path)))
}

async fn serve_one(stream: UnixStream, endpoint: SubagentEndpoint) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .await
        .context("read sub-agent request")?;
    let request: SubagentToolRequest =
        serde_json::from_str(line.trim()).context("parse sub-agent request")?;
    // Block until the daemon completes this request and return its result as
    // the tool's answer. A repeat request id returns the cached result at
    // once; otherwise wait, so the harness never sees a placeholder while the
    // real answer is delivered elsewhere.
    let request_id = request.request_id.clone();
    let result = match endpoint.enqueue(request)? {
        Some(cached) => Some(cached),
        None => {
            endpoint
                .await_result(&request_id, Instant::now() + SOCKET_WAIT_CEILING)
                .await
        }
    };
    let mut body = serde_json::to_vec(&SocketReply {
        accepted: true,
        result,
    })?;
    body.push(b'\n');
    write
        .write_all(&body)
        .await
        .context("answer sub-agent request")?;
    write.flush().await.context("flush sub-agent response")
}
