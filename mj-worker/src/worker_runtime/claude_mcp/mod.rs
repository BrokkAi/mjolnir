//! Worker-owned utility service. Lifecycle wiring is added by the launch layer.
mod cron;
mod monitor;
mod web_fetch;
use crate::relay::RelayCell;
use anyhow::{Context, Result};
use mj_core::claude_mcp::{
    CLAUDE_MCP_SERVER, ClaudeMcpCapabilities, ClaudeMcpRpcRequest, ClaudeMcpRpcResponse,
};
use mj_core::config::HarnessKind;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::mpsc,
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
pub const CLAUDE_MCP_SOCKET: &str = "claude-mcp.sock";
static IDS: AtomicU64 = AtomicU64::new(1);
fn id() -> String {
    format!(
        "{}-{}",
        chrono::Utc::now().timestamp_millis(),
        IDS.fetch_add(1, Ordering::Relaxed)
    )
}
fn required(v: &Value, key: &str) -> Result<String> {
    let s = v[key].as_str().with_context(|| format!("missing {key}"))?;
    anyhow::ensure!(!s.trim().is_empty(), "{key} must not be empty");
    Ok(s.into())
}
struct Service {
    root: PathBuf,
    cwd: PathBuf,
    environment: Mutex<BTreeMap<String, String>>,
    harness: Mutex<HarnessKind>,
    relay: Arc<RelayCell>,
    dispatch: mpsc::Sender<()>,
    cancel: CancellationToken,
    monitors: Mutex<BTreeMap<String, monitor::Monitor>>,
    jobs: tokio::sync::Mutex<BTreeMap<String, cron::Job>>,
    cron_io: Arc<tokio::sync::Mutex<()>>,
    wakeup: Mutex<Option<(u64, JoinHandle<()>)>>,
    cron_tasks: Mutex<Vec<tokio::task::AbortHandle>>,
}
impl Service {
    fn deliver_wake(&self, key: &str, text: String) -> bool {
        self.deliver_wake_with_key(&format!("{key}-{}", id()), text)
    }
    fn deliver_wake_with_key(&self, key: &str, text: String) -> bool {
        if let Err(error) = self
            .relay
            .lock()
            .expect("relay state lock poisoned")
            .deliver_agent_wake(CLAUDE_MCP_SERVER, key, text)
        {
            tracing::error!(%error,"utility wake delivery failed");
            return false;
        }
        let _ = self.dispatch.try_send(());
        true
    }
    async fn call(self: &Arc<Self>, req: ClaudeMcpRpcRequest) -> Result<Value> {
        let a = req.arguments;
        match req.tool.as_str() {
            "web_fetch" => web_fetch::fetch(a).await,
            "monitor" => monitor::start(self, a),
            "monitor_stop" => monitor::stop(self, &required(&a, "task_id")?).await,
            "monitor_list" => Ok(monitor::list(self)),
            "cron_create" => cron::create(self, a).await,
            "cron_list" => Ok(
                json!({"jobs":self.jobs.lock().await.values().map(|j|j.view()).collect::<Vec<_>>()}),
            ),
            "cron_delete" => cron::delete(self, &required(&a, "id")?).await,
            "schedule_wakeup" => self.schedule(a, Duration::from_secs(60)),
            _ => anyhow::bail!("unknown tool"),
        }
    }
    fn schedule(self: &Arc<Self>, a: Value, minimum: Duration) -> Result<Value> {
        let stop = a["stop"].as_bool().unwrap_or(false);
        let prepared = if stop {
            None
        } else {
            let delay = a["delay_seconds"]
                .as_i64()
                .context("missing delay_seconds")?;
            Some((delay, required(&a, "prompt")?, required(&a, "reason")?))
        };
        let mut slot = self.wakeup.lock().unwrap();
        let cancelled = slot
            .take()
            .map(|(_, h)| {
                let active = !h.is_finished();
                h.abort();
                usize::from(active)
            })
            .unwrap_or(0);
        let Some((delay, prompt, reason)) = prepared else {
            return Ok(json!({"stopped":true,"cancelled_wakeups":cancelled}));
        };
        let clamped = delay.clamp(60, 3600) as u64;
        let duration = if clamped == 60 {
            minimum
        } else {
            Duration::from_secs(clamped)
        };
        let service = self.clone();
        let generation = IDS.fetch_add(1, Ordering::Relaxed);
        *slot = Some((
            generation,
            mj_core::runtime::spawn(async move {
                tokio::select! {
                    _ = service.cancel.cancelled() => {},
                    _ = tokio::time::sleep(duration) => {
                        loop {
                            {
                                let mut slot = service.wakeup.lock().unwrap();
                                if !slot.as_ref().is_some_and(|(current, _)| *current == generation) { break; }
                                if service.deliver_wake("wakeup", format!("[mj-claude wakeup ({reason})]\n{prompt}")) {
                                    slot.take();
                                    break;
                                }
                            }
                            tokio::select! {
                                _ = service.cancel.cancelled() => break,
                                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                            }
                        }
                    }
                }
            }),
        ));
        Ok(
            json!({"scheduled_for":(chrono::Utc::now()+chrono::Duration::from_std(duration)?).to_rfc3339(),"clamped_delay_seconds":clamped,"was_clamped":delay!=clamped as i64,"noop":a["noop"].as_bool().unwrap_or(false)}),
        )
    }
}
#[derive(Clone)]
pub struct ClaudeMcpController(Arc<ControllerState>);
struct ControllerState {
    root: PathBuf,
    cwd: PathBuf,
    relay: Arc<RelayCell>,
    dispatch: mpsc::Sender<()>,
    runtime: tokio::runtime::Handle,
    service: Mutex<Option<ClaudeMcpShutdownGuard>>,
    cron_io: Arc<tokio::sync::Mutex<()>>,
}
pub struct ClaudeMcpControllerGuard(pub ClaudeMcpController);
impl Drop for ClaudeMcpControllerGuard {
    fn drop(&mut self) {
        self.0.0.service.lock().unwrap().take();
    }
}
impl ClaudeMcpController {
    pub fn new(
        root: PathBuf,
        cwd: PathBuf,
        relay: Arc<RelayCell>,
        dispatch: mpsc::Sender<()>,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        Self(Arc::new(ControllerState {
            root,
            cwd,
            relay,
            dispatch,
            runtime,
            service: Mutex::new(None),
            cron_io: Arc::new(tokio::sync::Mutex::new(())),
        }))
    }
    pub fn reconcile(
        &self,
        harness: HarnessKind,
        environment: BTreeMap<String, String>,
    ) -> Result<bool> {
        let mut slot = self.0.service.lock().unwrap();
        if ClaudeMcpCapabilities::for_harness(harness).is_empty() {
            slot.take();
            return Ok(false);
        }
        if let Some(guard) = slot.as_ref() {
            guard.update_runtime(harness, environment);
        } else {
            *slot = Some(start_with_cron_io(
                &self.0.root,
                self.0.cwd.clone(),
                environment,
                harness,
                self.0.relay.clone(),
                self.0.dispatch.clone(),
                &self.0.runtime,
                self.0.cron_io.clone(),
            )?);
        }
        Ok(true)
    }
}

pub struct ClaudeMcpShutdownGuard {
    service: Arc<Service>,
    listener: JoinHandle<()>,
    path: PathBuf,
}
impl ClaudeMcpShutdownGuard {
    pub fn update_runtime(&self, harness: HarnessKind, environment: BTreeMap<String, String>) {
        *self.service.environment.lock().unwrap() = environment;
        *self.service.harness.lock().unwrap() = harness;
    }
    pub fn shutdown(&self) {
        self.service.cancel.cancel();
        if let Some((_, task)) = self.service.wakeup.lock().unwrap().take() {
            task.abort();
        }
        for task in self.service.cron_tasks.lock().unwrap().drain(..) {
            task.abort();
        }
        self.listener.abort();
        if let Err(error) = std::fs::remove_file(&self.path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(%error,"unlink utility socket");
        }
    }
}
impl Drop for ClaudeMcpShutdownGuard {
    fn drop(&mut self) {
        self.shutdown();
    }
}
#[cfg(test)]
pub fn start_claude_mcp_service(
    worker_root: &Path,
    cwd: PathBuf,
    environment: BTreeMap<String, String>,
    harness: HarnessKind,
    relay: Arc<RelayCell>,
    dispatch_wake: mpsc::Sender<()>,
    runtime: &tokio::runtime::Handle,
) -> Result<ClaudeMcpShutdownGuard> {
    start_with_cron_io(
        worker_root,
        cwd,
        environment,
        harness,
        relay,
        dispatch_wake,
        runtime,
        Arc::new(tokio::sync::Mutex::new(())),
    )
}
#[allow(clippy::too_many_arguments)] // Service state plus the worker-owned cross-generation I/O lock.
fn start_with_cron_io(
    worker_root: &Path,
    cwd: PathBuf,
    environment: BTreeMap<String, String>,
    harness: HarnessKind,
    relay: Arc<RelayCell>,
    dispatch_wake: mpsc::Sender<()>,
    runtime: &tokio::runtime::Handle,
    cron_io: Arc<tokio::sync::Mutex<()>>,
) -> Result<ClaudeMcpShutdownGuard> {
    use std::os::unix::fs::PermissionsExt;
    let path = worker_root.join(CLAUDE_MCP_SOCKET);
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let listener = mj_core::local_sockets::bind_unix_listener(&path)?;
    listener.set_nonblocking(true)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    let _entered = runtime.enter();
    let listener = tokio::net::UnixListener::from_std(listener)?;
    let service = Arc::new(Service {
        root: worker_root.into(),
        cwd,
        environment: Mutex::new(environment),
        harness: Mutex::new(harness),
        relay,
        dispatch: dispatch_wake,
        cancel: CancellationToken::new(),
        monitors: Mutex::new(BTreeMap::new()),
        jobs: tokio::sync::Mutex::new(BTreeMap::new()),
        cron_io,
        wakeup: Mutex::new(None),
        cron_tasks: Mutex::new(Vec::new()),
    });
    let s = service.clone();
    let task=runtime.spawn(async move {
        if let Err(error)=cron::reload(&s).await {tracing::error!(%error,"reload durable cron jobs failed");}
        let mut clients=tokio::task::JoinSet::new();
        loop { tokio::select! {
            _=s.cancel.cancelled()=>break,
            result=clients.join_next(),if !clients.is_empty()=>{if let Some(Err(error))=result {tracing::error!(%error,"utility client task failed");}},
            result=listener.accept()=>match result {
                Ok((stream,_))=>{let s=s.clone();clients.spawn(async move {
                    let result=async {
                        let (read,mut write)=stream.into_split();let mut line=String::new();
                        BufReader::new(read).read_line(&mut line).await?;
                        let req:ClaudeMcpRpcRequest=serde_json::from_str(&line)?;
                        let enabled = ClaudeMcpCapabilities::for_harness(*s.harness.lock().unwrap()).supports_tool(&req.tool);
                        let result=if enabled {s.call(req).await} else {Err(anyhow::anyhow!("tool is not enabled for this harness"))};
                        let response=match result {Ok(v)=>ClaudeMcpRpcResponse{ok:true,result:Some(v),error:None},Err(e)=>ClaudeMcpRpcResponse{ok:false,result:None,error:Some(format!("{e:#}"))}};
                        let mut bytes=serde_json::to_vec(&response)?;bytes.push(b'\n');write.write_all(&bytes).await?;
                        Ok::<_,anyhow::Error>(())
                    }.await;
                    if let Err(error)=result {tracing::warn!(%error,"utility RPC failed");}
                });},
                Err(error)=>{tracing::error!(%error,"utility accept failed");break;}
            }
        }}
    });
    Ok(ClaudeMcpShutdownGuard {
        service,
        listener: task,
        path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn rpc(path: &Path, tool: &str, arguments: Value) -> Value {
        let stream = tokio::net::UnixStream::connect(path).await.unwrap();
        let (r, mut w) = stream.into_split();
        let mut bytes = serde_json::to_vec(&ClaudeMcpRpcRequest {
            tool: tool.into(),
            arguments,
        })
        .unwrap();
        bytes.push(b'\n');
        w.write_all(&bytes).await.unwrap();
        let mut line = String::new();
        BufReader::new(r).read_line(&mut line).await.unwrap();
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["ok"], true, "{v}");
        v["result"].clone()
    }
    #[tokio::test]
    async fn claude_mcp_controller_reconciles_harness_and_environment() {
        let root = tempfile::tempdir().unwrap();
        let relay = Arc::new(RelayCell::new(
            crate::relay::DurableRelay::open(root.path(), "test-session", "test").unwrap(),
        ));
        let (tx, _rx) = mpsc::channel(100);
        let controller = ClaudeMcpController::new(
            root.path().into(),
            root.path().into(),
            relay,
            tx,
            tokio::runtime::Handle::current(),
        );
        let guard = ClaudeMcpControllerGuard(controller.clone());
        assert!(
            !controller
                .reconcile(HarnessKind::Claude, BTreeMap::new())
                .unwrap()
        );
        assert!(
            controller
                .reconcile(HarnessKind::Codex, std::env::vars().collect())
                .unwrap()
        );
        let path = root.path().join(CLAUDE_MCP_SOCKET);
        rpc(&path, "monitor_list", json!({})).await;
        assert!(
            controller
                .reconcile(
                    HarnessKind::Muse,
                    BTreeMap::from([("MARKER".into(), "updated".into())])
                )
                .unwrap()
        );
        {
            let active = controller.0.service.lock().unwrap();
            let service = &active.as_ref().unwrap().service;
            assert_eq!(*service.harness.lock().unwrap(), HarnessKind::Muse);
            assert_eq!(
                service.environment.lock().unwrap().get("MARKER").unwrap(),
                "updated"
            );
        }
        assert!(
            !controller
                .reconcile(HarnessKind::Claude, BTreeMap::new())
                .unwrap()
        );
        assert!(!path.exists());
        assert!(
            controller
                .reconcile(HarnessKind::Codex, std::env::vars().collect())
                .unwrap()
        );
        drop(guard);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn claude_mcp_socket_timers_persistence_and_monitor_pipes() {
        let root = tempfile::tempdir().unwrap();
        let relay = Arc::new(RelayCell::new(
            crate::relay::DurableRelay::open(root.path(), "test-session", "test").unwrap(),
        ));
        let (tx, mut rx) = mpsc::channel(100);
        let start = || {
            start_claude_mcp_service(
                root.path(),
                root.path().into(),
                std::env::vars().collect(),
                HarnessKind::Codex,
                relay.clone(),
                tx.clone(),
                &tokio::runtime::Handle::current(),
            )
            .unwrap()
        };
        let guard = start();
        let path = root.path().join(CLAUDE_MCP_SOCKET);
        let job = rpc(
            &path,
            "cron_create",
            json!({"cron":"0 0 29 2 *","prompt":"leap","durable":true}),
        )
        .await;
        let (ordinal, digest) = {
            let state = relay.lock().unwrap();
            (state.latest_ordinal(), state.latest_digest().to_owned())
        };
        guard
            .service
            .schedule(
                json!({"delay_seconds":60,"prompt":"replaced","reason":"test"}),
                Duration::from_millis(30),
            )
            .unwrap();
        guard
            .service
            .schedule(
                json!({"delay_seconds":60,"prompt":"kept","reason":"test"}),
                Duration::from_millis(5),
            )
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let events = relay
            .lock()
            .unwrap()
            .events_after(ordinal, &digest)
            .unwrap();
        let encoded = serde_json::to_string(&events).unwrap();
        assert!(encoded.contains("kept"));
        assert!(!encoded.contains("replaced"));
        assert!(encoded.contains("\"wake\":true"));
        rpc(&path, "schedule_wakeup", json!({"stop":true})).await;
        assert_eq!(
            rpc(&path, "cron_list", json!({})).await["jobs"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let monitor=rpc(&path,"monitor",json!({"description":"pipes","command":"head -c 131072 /dev/zero & head -c 131072 /dev/zero >&2; wait"})).await;
        tokio::time::timeout(Duration::from_secs(10), async {
            while guard
                .service
                .monitors
                .lock()
                .unwrap()
                .contains_key(monitor["task_id"].as_str().unwrap())
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        drop(guard);
        let guard = start();
        for _ in 0..100 {
            if !rpc(&path, "cron_list", json!({})).await["jobs"]
                .as_array()
                .unwrap()
                .is_empty()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            rpc(&path, "cron_list", json!({})).await["jobs"][0]["id"],
            job["id"]
        );
        let m = rpc(
            &path,
            "monitor",
            json!({"description":"timeout","command":"sleep 30","timeout_ms":5}),
        )
        .await;
        let task = m["task_id"].as_str().unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while guard.service.monitors.lock().unwrap().contains_key(task) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }
}
