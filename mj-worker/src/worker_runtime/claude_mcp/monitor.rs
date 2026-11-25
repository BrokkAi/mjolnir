use super::*;
use std::process::Stdio;
use tokio::io::AsyncReadExt;
pub(super) struct Monitor {
    pub cancel: CancellationToken,
    pub view: Value,
    pub events: Arc<AtomicU64>,
    pub done: tokio::sync::watch::Receiver<bool>,
}
pub(super) fn kill(pgid: i32, signal: i32) {
    if unsafe { libc::killpg(pgid, signal) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            tracing::warn!(%error,pgid,"signal monitor group failed");
        }
    }
}
fn take_utf8_chunk(batch: &mut Vec<u8>, flush_all: bool) -> Option<String> {
    if batch.is_empty() {
        return None;
    }
    if flush_all {
        let out = String::from_utf8_lossy(batch).into_owned();
        batch.clear();
        return Some(out);
    }
    // Walk invalid spans as well as valid prefixes, retaining only an incomplete tail.
    let mut end = 0;
    while end < batch.len() {
        match std::str::from_utf8(&batch[end..]) {
            Ok(_) => {
                end = batch.len();
                break;
            }
            Err(error) => {
                end += error.valid_up_to();
                match error.error_len() {
                    Some(len) => end += len,
                    None => break,
                }
            }
        }
    }
    if end == 0 {
        return None;
    }
    let out = String::from_utf8_lossy(&batch[..end]).into_owned();
    batch.drain(..end);
    Some(out)
}

pub(super) fn list(s: &Service) -> Value {
    json!({"monitors":s.monitors.lock().unwrap().values().map(|m|{let mut v=m.view.clone();v["events_emitted"]=json!(m.events.load(Ordering::Relaxed));v}).collect::<Vec<_>>()})
}
pub(super) fn start(s: &Arc<Service>, a: Value) -> Result<Value> {
    let description = required(&a, "description")?;
    let command = required(&a, "command")?;
    let timeout = if a["persistent"].as_bool().unwrap_or(false) {
        0
    } else {
        a["timeout_ms"].as_u64().unwrap_or(300_000)
    };
    let task_id = id();
    let mut registry = s.monitors.lock().unwrap();
    anyhow::ensure!(!s.cancel.is_cancelled(), "service is shutting down");
    let mut child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(&command)
        .current_dir(&s.cwd)
        .env_clear()
        .envs(&*s.environment.lock().unwrap())
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let pgid = child.id().context("monitor PID missing")? as i32;
    let cancel = s.cancel.child_token();
    let events = Arc::new(AtomicU64::new(0));
    let (done_tx, done) = tokio::sync::watch::channel(false);
    let view = json!({"task_id":task_id,"description":description,"command":command,"started_at_ms":chrono::Utc::now().timestamp_millis(),"timeout_ms":timeout,"persistent":timeout==0});
    registry.insert(
        task_id.clone(),
        Monitor {
            cancel: cancel.clone(),
            view: view.clone(),
            events: events.clone(),
            done,
        },
    );
    drop(registry);
    let s = s.clone();
    mj_core::runtime::spawn(async move {
        let mut stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let out_s = s.clone();
        let out_id = task_id.clone();
        let out_cancel = cancel.clone();
        let mut output = mj_core::runtime::spawn(async move {
            let mut buf = [0; 4096];
            let mut batch = Vec::new();
            let mut tick = tokio::time::interval(Duration::from_millis(200));
            loop {
                let mut flush = false;
                let end = tokio::select! {biased; _=out_cancel.cancelled()=>break,r=stdout.read(&mut buf)=>match r {Ok(0)=>true,Ok(n)=>{batch.extend_from_slice(&buf[..n]);false},Err(error)=>{tracing::warn!(%error,"monitor stdout failed");true}},_=tick.tick()=>{flush=true;false}};
                if out_cancel.is_cancelled() {
                    break;
                }
                if (end || batch.len() >= 16384 || flush)
                    && let Some(chunk) = take_utf8_chunk(&mut batch, end)
                {
                    out_s.deliver_wake(
                        &out_id,
                        format!("[mj-claude monitor {out_id}: {description}]\n{chunk}"),
                    );
                    events.fetch_add(1, Ordering::Relaxed);
                }
                if end {
                    break;
                }
            }
        });
        let err_cancel = cancel.clone();
        let mut errors = mj_core::runtime::spawn(async move {
            let mut tail = Vec::new();
            let mut buf = [0; 4096];
            loop {
                match tokio::select! { biased; _ = err_cancel.cancelled() => break, result = stderr.read(&mut buf) => result }
                {
                    Ok(0) => break,
                    Ok(n) => {
                        tail.extend_from_slice(&buf[..n]);
                        if tail.len() > 4096 {
                            tail.drain(..tail.len() - 4096);
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%error,"monitor stderr failed");
                        break;
                    }
                }
            }
            tail
        });
        let deadline = async {
            if timeout == 0 {
                std::future::pending::<()>().await
            } else {
                tokio::time::sleep(Duration::from_millis(timeout)).await
            }
        };
        // Only this supervisor signals the group, and only before reaping.
        let (status, stopped, timed_out) = tokio::select! {
            _ = cancel.cancelled() => {
                s.monitors.lock().unwrap().remove(&task_id);
                kill(pgid, libc::SIGTERM); kill(pgid, libc::SIGKILL);
                (child.wait().await, true, false)
            },
            _ = deadline => {
                s.monitors.lock().unwrap().remove(&task_id);
                kill(pgid, libc::SIGTERM); kill(pgid, libc::SIGKILL);
                (child.wait().await, false, true)
            },
            result = child.wait() => {
                s.monitors.lock().unwrap().remove(&task_id);
                (result, false, false)
            }
        };
        // Escaped descendants can retain pipes after the owned group exits.
        // Bound only this final drain; cancellation always remains responsive.
        let drained = tokio::select! {
            biased;
            _ = cancel.cancelled() => None,
            result = tokio::time::timeout(Duration::from_secs(1), async { tokio::join!(&mut output, &mut errors) }) => result.ok(),
        };
        let tail = if let Some((out, err)) = drained {
            if let Err(error) = out {
                tracing::error!(%error, "monitor output task failed");
            }
            match err {
                Ok(tail) => tail,
                Err(error) => {
                    tracing::error!(%error,"monitor stderr task failed");
                    Vec::new()
                }
            }
        } else {
            output.abort();
            errors.abort();
            Vec::new()
        };
        if !stopped && !cancel.is_cancelled() {
            s.deliver_wake(
                &task_id,
                format!(
                    "[mj-claude monitor {task_id}] {}{}",
                    if timed_out {
                        "timed out".into()
                    } else {
                        format!("exited: {status:?}")
                    },
                    if status.as_ref().is_ok_and(|v| v.success()) {
                        String::new()
                    } else {
                        format!("\n{}", String::from_utf8_lossy(&tail))
                    }
                ),
            );
        }
        let _ = done_tx.send(true);
    });
    Ok(view)
}
pub(super) async fn stop(s: &Service, id: &str) -> Result<Value> {
    let mut done = {
        let mut map = s.monitors.lock().unwrap();
        let m = map.remove(id).context("unknown monitor")?;
        m.cancel.cancel();
        m.done
    };
    if !*done.borrow() {
        let _ = done.changed().await;
    }
    Ok(json!({"task_id":id,"stopped":true}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn claude_mcp_monitor_utf8_chunks_preserve_incomplete_tail() {
        let mut batch = b"prefix ".to_vec();
        batch.extend_from_slice(&"😀".as_bytes()[..2]);
        let first = take_utf8_chunk(&mut batch, false).unwrap();
        assert_eq!(first, "prefix ");
        assert!(take_utf8_chunk(&mut batch, false).is_none());
        batch.extend_from_slice(&"😀".as_bytes()[2..]);
        assert_eq!(
            first + &take_utf8_chunk(&mut batch, false).unwrap(),
            "prefix 😀"
        );
        batch.extend_from_slice(&[0xff, 0xf0, 0x9f]);
        assert_eq!(take_utf8_chunk(&mut batch, false).unwrap(), "�");
        batch.extend_from_slice(&[0x98, 0x80]);
        assert_eq!(take_utf8_chunk(&mut batch, true).unwrap(), "😀");
    }

    #[tokio::test]
    async fn claude_mcp_monitor_stop_drains_escaped_descendant_pipes() {
        let root = tempfile::tempdir().unwrap();
        let relay = Arc::new(RelayCell::new(
            crate::relay::DurableRelay::open(root.path(), "test-session", "test").unwrap(),
        ));
        let (tx, _rx) = mpsc::channel(100);
        let guard = start_claude_mcp_service(
            root.path(),
            root.path().into(),
            std::env::vars().collect(),
            HarnessKind::Codex,
            relay,
            tx,
            &tokio::runtime::Handle::current(),
        )
        .unwrap();
        let result = start(&guard.service, json!({"description":"escaped pipes", "command":r#"python3 -c 'import os,time; os.setsid(); open("escaped.pid","w").write(str(os.getpid())); time.sleep(30)' & wait"#, "persistent":true})).unwrap();
        let pid = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Ok(text) = tokio::fs::read_to_string(root.path().join("escaped.pid")).await
                    && let Ok(pid) = text.parse::<i32>()
                {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let stopped = tokio::time::timeout(
            Duration::from_secs(2),
            stop(&guard.service, result["task_id"].as_str().unwrap()),
        )
        .await;
        // The escaped session is intentionally outside the monitor's process group.
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
        stopped.unwrap().unwrap();
    }

    #[tokio::test]
    async fn claude_mcp_monitor_stop_and_drop_kill_descendants() {
        for explicit in [true, false] {
            let root = tempfile::tempdir().unwrap();
            let relay = Arc::new(RelayCell::new(
                crate::relay::DurableRelay::open(root.path(), "test-session", "test").unwrap(),
            ));
            let (tx, _rx) = mpsc::channel(100);
            let guard = start_claude_mcp_service(
                root.path(),
                root.path().into(),
                std::env::vars().collect(),
                HarnessKind::Codex,
                relay,
                tx,
                &tokio::runtime::Handle::current(),
            )
            .unwrap();
            let result=start(&guard.service,json!({"description":"descendant","command":"sleep 30 & echo $! > descendant.pid; wait","persistent":true})).unwrap();
            let file = root.path().join("descendant.pid");
            let pid = tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    if let Ok(s) = tokio::fs::read_to_string(&file).await
                        && let Ok(pid) = s.trim().parse::<i32>()
                    {
                        break pid;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
            if explicit {
                stop(&guard.service, result["task_id"].as_str().unwrap())
                    .await
                    .unwrap();
            }
            drop(guard);
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    let dead = unsafe { libc::kill(pid, 0) } < 0;
                    // Linux may retain an orphan zombie until PID 1 reaps it: it is no longer running.
                    let zombie = tokio::fs::read_to_string(format!("/proc/{pid}/stat"))
                        .await
                        .is_ok_and(|s| s.split_whitespace().nth(2) == Some("Z"));
                    if dead || zombie {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
        }
    }
}
