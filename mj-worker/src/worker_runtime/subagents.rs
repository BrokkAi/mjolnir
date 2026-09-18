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

/// Slack a `wait` gets on top of the caller's own timeout before this worker
/// stops waiting for the daemon and answers by itself. It covers the hop that
/// carries the daemon's result back here; past it, answering late helps nobody.
const WORKER_WAIT_GRACE: Duration = Duration::from_secs(5);

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

    #[tokio::test(start_paused = true)]
    async fn a_wait_the_daemon_never_answers_is_answered_here_at_the_callers_deadline() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let directory = tempfile::tempdir().unwrap();
        let endpoint = SubagentEndpoint::open(directory.path()).unwrap();
        let socket = directory.path().join("test.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let served = tokio::spawn({
            let endpoint = endpoint.clone();
            async move {
                let (stream, _) = listener.accept().await.unwrap();
                serve_one(stream, endpoint).await.unwrap();
            }
        });

        let mut client = UnixStream::connect(&socket).await.unwrap();
        let request = SubagentToolRequest {
            request_id: "r-late".into(),
            created_at_ms: 1,
            action: SubagentToolAction::WaitAgents {
                child_session_ids: vec!["child-1".into(), "child-2".into()],
                timeout_seconds: Some(45),
            },
        };
        let mut body = serde_json::to_vec(&request).unwrap();
        body.push(b'\n');
        client.write_all(&body).await.unwrap();
        client.flush().await.unwrap();

        // Nothing ever completes this request. The answer must still arrive,
        // at the caller's deadline plus this worker's small grace.
        let started = Instant::now();
        let mut line = String::new();
        BufReader::new(&mut client)
            .read_line(&mut line)
            .await
            .unwrap();
        let elapsed = started.elapsed();
        served.await.unwrap();

        assert!(
            elapsed >= Duration::from_secs(45) && elapsed <= Duration::from_secs(55),
            "the answer must land at the caller's deadline, took {elapsed:?}"
        );
        let reply: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(reply["accepted"], true);
        assert_eq!(reply["result"]["is_error"], false, "{reply}");
        let payload: serde_json::Value =
            serde_json::from_str(reply["result"]["message"].as_str().unwrap()).unwrap();
        assert_eq!(
            payload["status"],
            mj_core::subagent::WAIT_STATUS_STILL_RUNNING,
            "{payload}"
        );
        assert_eq!(payload["agents"][0]["child_session_id"], "child-1");
        assert!(
            payload["next_action"]
                .as_str()
                .unwrap()
                .contains("Call wait again"),
            "{payload}"
        );
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
            // A transient accept failure (EMFILE, ECONNABORTED) must not end
            // the loop: dropping the listener would refuse every later tool
            // call for the life of the worker.
            let stream = match listener.accept().await {
                Ok((stream, _)) => stream,
                Err(error) => {
                    tracing::warn!(error = %error, "sub-agent socket accept failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
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

/// How long this worker waits for the daemon before answering by itself. Only
/// a `wait` has a deadline of the caller's own choosing; every other action is
/// bounded by the blanket ceiling, because the caller gave no deadline to keep.
fn wait_budget(action: &mj_core::subagent::SubagentToolAction) -> Duration {
    match action {
        mj_core::subagent::SubagentToolAction::WaitAgents {
            timeout_seconds, ..
        } => mj_core::subagent::subagent_wait_timeout(*timeout_seconds) + WORKER_WAIT_GRACE,
        _ => SOCKET_WAIT_CEILING,
    }
}

/// The children a `wait` is about, or `None` for any other action. Only a
/// `wait` can be answered by this worker alone; the rest have no answer that
/// does not come from the daemon.
fn waiting_children(action: &mj_core::subagent::SubagentToolAction) -> Option<Vec<String>> {
    match action {
        mj_core::subagent::SubagentToolAction::WaitAgents {
            child_session_ids, ..
        } => Some(child_session_ids.clone()),
        _ => None,
    }
}

/// This worker's own answer to a `wait` the daemon did not finish in time. It
/// carries the same shape as the daemon's answer, so a model reads one rule:
/// `status` says whether the children are finished, and `next_action` says what
/// to do. It is not an error; the children are still working.
fn late_daemon_reply(
    request_id: &str,
    child_session_ids: &[String],
    waited_seconds: u64,
) -> SubagentToolResult {
    let payload = mj_core::subagent::still_running_payload(
        child_session_ids,
        waited_seconds,
        Some(
            "Mjolnir did not finish checking these children within this call's timeout. \
             Their state here is unknown rather than observed; call wait again to collect it.",
        ),
    );
    SubagentToolResult {
        request_id: request_id.to_owned(),
        completed_at_ms: mj_core::clock::epoch_millis(),
        is_error: false,
        message: serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string()),
    }
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
    //
    // A `wait` gets the caller's own deadline here, on this host's monotonic
    // clock. This is the timer the model depends on: it is unaffected by a
    // daemon restart, by a result that could not be handed back, and by clock
    // skew between the two hosts. When it expires this worker answers the call
    // itself rather than leaving the harness in silence.
    let request_id = request.request_id.clone();
    let deadline_budget = wait_budget(&request.action);
    let waiting_for = waiting_children(&request.action);
    let started = Instant::now();
    let result = match endpoint.enqueue(request)? {
        Some(cached) => Some(cached),
        None => {
            endpoint
                .await_result(&request_id, started + deadline_budget)
                .await
        }
    };
    let result = result.or_else(|| {
        waiting_for.map(|children| {
            tracing::warn!(
                request_id = %request_id,
                waited_seconds = started.elapsed().as_secs(),
                "the daemon did not answer a sub-agent wait by its deadline; \
                 answering that the children are still running"
            );
            late_daemon_reply(&request_id, &children, started.elapsed().as_secs())
        })
    });
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
