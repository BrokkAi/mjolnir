//! Listener recovery lives beside the daemon, independently of session control.

use std::net::SocketAddr;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use mj_controller::hel_server::{
    ServerOptions, WebListenerProcess, WebViewerAccess, WebViewerRecovery, run_server_on_listener,
};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

pub(crate) struct ViewerControl {
    access: watch::Sender<WebViewerAccess>,
    commands: Mutex<Option<mpsc::Sender<WebViewerRecovery>>>,
}

impl ViewerControl {
    pub(crate) fn new() -> Self {
        Self {
            access: watch::channel(WebViewerAccess::Starting).0,
            commands: Mutex::new(None),
        }
    }

    pub(crate) fn access(&self) -> WebViewerAccess {
        self.access.borrow().clone()
    }

    pub(crate) fn publish(&self, access: WebViewerAccess) {
        self.access.send_replace(access);
    }

    pub(crate) fn recover(&self, action: WebViewerRecovery) -> Result<()> {
        // Serialize checking and queueing so two attached clients cannot start two recoveries.
        let commands = self.commands.lock().unwrap_or_else(PoisonError::into_inner);
        ensure!(
            matches!(self.access(), WebViewerAccess::Failed { .. }),
            "The viewer is already running or starting. Refresh its status before retrying."
        );
        let sender = commands
            .as_ref()
            .context("Viewer recovery is unavailable")?;
        let previous = self.access();
        if matches!(&action, WebViewerRecovery::StopAndRetry(_)) {
            self.conflict_address()?;
        }
        self.publish(WebViewerAccess::Starting);
        if let Err(error) = sender.try_send(action) {
            self.publish(previous);
            return Err(error).context("A viewer recovery is already in progress");
        }
        Ok(())
    }

    pub(crate) fn conflict_address(&self) -> Result<SocketAddr> {
        match self.access() {
            WebViewerAccess::Failed {
                address,
                port_conflict: true,
                ..
            } => Ok(address),
            _ => bail!("The viewer no longer has a port conflict. Refresh its status."),
        }
    }
}

/// Retain the controller and authentication while the listener waits for recovery.
pub(crate) async fn serve(
    options: ServerOptions,
    ready: WebViewerAccess,
    control: &ViewerControl,
    report: impl Fn(WebViewerAccess),
) -> Result<()> {
    let (commands, mut requests) = mpsc::channel(1);
    *control
        .commands
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some(commands);
    let result = serve_inner(options, ready, &mut requests, report).await;
    *control
        .commands
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = None;
    result
}

async fn serve_inner(
    options: ServerOptions,
    ready: WebViewerAccess,
    requests: &mut mpsc::Receiver<WebViewerRecovery>,
    report: impl Fn(WebViewerAccess),
) -> Result<()> {
    let mut address = options.bind;
    let mut recovery_failure = None;
    loop {
        let failure = if let Some(message) = recovery_failure.take() {
            WebViewerAccess::Failed {
                address,
                message,
                port_conflict: true,
            }
        } else {
            report(WebViewerAccess::Starting);
            let listener = tokio::select! {
                _ = options.shutdown.cancelled() => return Ok(()),
                result = TcpListener::bind(address) => result,
            };
            match listener {
                Ok(listener) => {
                    address = listener.local_addr().context("read reserved viewer port")?;
                    report(ready_at(&ready, address.port())?);
                    let result = run_server_on_listener(options.clone(), listener).await;
                    if options.shutdown.is_cancelled() {
                        return result;
                    }
                    let message = match result {
                        Ok(()) => {
                            "The web viewer stopped unexpectedly. Try starting it again.".to_owned()
                        }
                        Err(error) => {
                            tracing::warn!(error = format!("{error:#}"), %address, "web viewer stopped");
                            format!("The web viewer stopped: {error}")
                        }
                    };
                    WebViewerAccess::Failed {
                        address,
                        message,
                        port_conflict: false,
                    }
                }
                Err(error) => {
                    let port_conflict = error.kind() == std::io::ErrorKind::AddrInUse;
                    let message = if port_conflict {
                        format!("Port {} is already in use.", address.port())
                    } else {
                        format!("Could not listen on {address}: {error}")
                    };
                    WebViewerAccess::Failed {
                        address,
                        message,
                        port_conflict,
                    }
                }
            }
        };
        report(failure);
        let request = tokio::select! {
            _ = options.shutdown.cancelled() => return Ok(()),
            request = requests.recv() => request.context("viewer recovery channel closed")?,
        };
        report(WebViewerAccess::Starting);
        match request {
            WebViewerRecovery::Retry => {}
            WebViewerRecovery::AnotherPort => address.set_port(0),
            WebViewerRecovery::StopAndRetry(process) => {
                let cancellation = options.shutdown.clone();
                let result = stop_listener(address, process, cancellation).await;
                if let Err(error) = result {
                    recovery_failure = Some(format!("Could not stop the server: {error:#}"));
                }
            }
        }
    }
}

fn ready_at(ready: &WebViewerAccess, port: u16) -> Result<WebViewerAccess> {
    let WebViewerAccess::Ready {
        viewer_url,
        viewer_code,
        qr_login_url,
        fallback_reason,
    } = ready
    else {
        bail!("viewer startup is missing its access details");
    };
    fn with_port(value: &str, port: u16) -> Result<String> {
        let mut url = url::Url::parse(value).context("parse viewer URL")?;
        url.set_port(Some(port))
            .map_err(|()| anyhow::anyhow!("viewer URL cannot have a port"))?;
        Ok(url.into())
    }
    Ok(WebViewerAccess::Ready {
        viewer_url: with_port(viewer_url, port)?,
        viewer_code: viewer_code.clone(),
        qr_login_url: qr_login_url
            .as_deref()
            .map(|url| with_port(url, port))
            .transpose()?,
        fallback_reason: fallback_reason.clone(),
    })
}

pub(crate) fn inspect_listener(address: SocketAddr) -> Result<Vec<WebListenerProcess>> {
    let pids = listener_pids(address)?;
    let mut system = sysinfo::System::new();
    let own_pid = sysinfo::Pid::from_u32(std::process::id());
    let mut requested = pids
        .iter()
        .map(|pid| sysinfo::Pid::from_u32(*pid))
        .collect::<Vec<_>>();
    if !requested.contains(&own_pid) {
        requested.push(own_pid);
    }
    system.refresh_processes_specifics(
        sysinfo::ProcessesToUpdate::Some(&requested),
        true,
        sysinfo::ProcessRefreshKind::new()
            .with_user(sysinfo::UpdateKind::Always)
            .with_cmd(sysinfo::UpdateKind::Always)
            .with_exe(sysinfo::UpdateKind::Always),
    );
    let own_user = system
        .process(own_pid)
        .and_then(|process| process.user_id());
    Ok(pids
        .into_iter()
        .filter_map(|pid| {
            let process = system.process(sysinfo::Pid::from_u32(pid))?;
            let executable = process.exe().map(std::path::Path::to_path_buf).unwrap_or_default();
            let is_mj = executable.file_stem().is_some_and(|name| name == "mj")
                && process.cmd().get(1).is_some_and(|arg| arg == "daemon-run");
            let reason = if pid == std::process::id() {
                Some("This is the current daemon; stopping it would disconnect this dashboard.")
            } else if own_user.is_none() || process.user_id() != own_user {
                Some("This process belongs to another user or its owner cannot be verified.")
            } else if !is_mj {
                Some("This is not an identified Mjolnir server. Stop it in its own application.")
            } else if !cfg!(target_os = "linux") {
                Some("Safe stopping is unavailable on this platform. Stop this server in its application or use another port.")
            } else {
                None
            };
            Some(WebListenerProcess {
                pid,
                name: process.name().to_string_lossy().into_owned(),
                executable,
                started_at: process.start_time(),
                stop_disabled_reason: reason.map(str::to_owned),
            })
        })
        .collect())
}

async fn stop_listener(
    address: SocketAddr,
    expected: WebListenerProcess,
    cancel: CancellationToken,
) -> Result<()> {
    let pid = expected.pid;
    let signal_cancel = cancel.clone();
    let mut task = tokio::task::spawn_blocking(move || {
        // Acquire a stable process handle before re-inspecting, so PID reuse cannot redirect a signal.
        #[cfg(target_os = "linux")]
        let process_handle = open_process_handle(pid)?;
        let current = inspect_listener(address)?
            .into_iter()
            .find(|process| process.pid == pid)
            .context("That process no longer owns this listener. Inspect the port again.")?;
        ensure!(
            current == expected,
            "The listener's identity changed. Inspect the port again."
        );
        ensure!(
            current.stop_disabled_reason.is_none(),
            "{}",
            current.stop_disabled_reason.unwrap_or_default()
        );
        ensure!(
            !signal_cancel.is_cancelled(),
            "Viewer shutdown cancelled the stop request"
        );
        #[cfg(target_os = "linux")]
        {
            signal_process(&process_handle)
        }
        #[cfg(not(target_os = "linux"))]
        {
            bail!(
                "Safe process termination is unavailable on this platform. Use another port or stop the identified server in its application."
            )
        }
    });
    tokio::select! {
        result = &mut task => result.context("listener stop task failed")??,
        _ = cancel.cancelled() => {
            // Inspection is bounded; observe its result even when shutdown wins.
            match task.await {
                Ok(Ok(())) => {},
                Ok(Err(error)) => tracing::warn!(%error, "listener stop failed during shutdown"),
                Err(error) => tracing::warn!(%error, "listener stop task failed during shutdown"),
            }
            bail!("Viewer shutdown interrupted recovery");
        }
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => bail!("Viewer shutdown interrupted recovery"),
            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
        }
        match TcpListener::bind(address).await {
            Ok(listener) => {
                drop(listener);
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {}
            Err(error) => return Err(error).context("check listener after stopping the server"),
        }
        ensure!(
            tokio::time::Instant::now() < deadline,
            "Port {} is still occupied after waiting 10 seconds. No force kill was sent. Use another port or inspect again.",
            address.port()
        );
    }
}

#[cfg(target_os = "linux")]
fn signal_process(handle: &std::os::fd::OwnedFd) -> Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: the owned pidfd names the inspected process, never a recycled PID.
    let result = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            handle.as_raw_fd(),
            libc::SIGTERM,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    ensure!(result == 0, "{}", std::io::Error::last_os_error());
    Ok(())
}

#[cfg(target_os = "linux")]
fn open_process_handle(pid: u32) -> Result<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd;
    // SAFETY: pidfd_open has no pointer arguments; success yields an owned descriptor.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    ensure!(
        fd >= 0,
        "Cannot safely open process {pid}: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: the new descriptor is owned by this call.
    Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd as i32) })
}

#[cfg(target_os = "linux")]
fn listener_pids(address: SocketAddr) -> Result<Vec<u32>> {
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::Path;
    let mut inodes = BTreeSet::new();
    for (path, ipv6) in [("/proc/net/tcp", false), ("/proc/net/tcp6", true)] {
        let contents = match fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && ipv6 => continue,
            Err(error) => return Err(error).with_context(|| format!("read {path}")),
        };
        for line in contents.lines().skip(1) {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            ensure!(fields.len() >= 10, "Invalid listener information in {path}");
            if fields[3] != "0A" {
                continue;
            }
            let candidate = proc_address(fields[1], ipv6)?;
            if addresses_overlap(address, candidate) {
                inodes.insert(format!("socket:[{}]", fields[9]));
            }
        }
    }
    if inodes.is_empty() {
        return Ok(Vec::new());
    }
    let mut pids = BTreeSet::new();
    for entry in fs::read_dir("/proc").context("inspect running processes")? {
        let entry = entry?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let descriptors = match fs::read_dir(entry.path().join("fd")) {
            Ok(entries) => entries,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotFound
                ) =>
            {
                continue;
            }
            Err(error) => return Err(error).context("inspect listener process descriptors"),
        };
        for descriptor in descriptors {
            let descriptor = descriptor?;
            let target = match fs::read_link(descriptor.path()) {
                Ok(target) => target,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotFound
                    ) =>
                {
                    continue;
                }
                Err(error) => return Err(error).context("inspect listener socket ownership"),
            };
            if inodes.iter().any(|inode| target == Path::new(inode)) {
                pids.insert(pid);
                break;
            }
        }
    }
    Ok(pids.into_iter().collect())
}

#[cfg(target_os = "linux")]
fn proc_address(value: &str, ipv6: bool) -> Result<SocketAddr> {
    let (host, port) = value.split_once(':').context("invalid listener address")?;
    let port = u16::from_str_radix(port, 16).context("invalid listener port")?;
    if ipv6 {
        ensure!(host.len() == 32 && host.is_ascii(), "invalid IPv6 listener");
        let mut bytes = [0; 16];
        for (index, bytes) in bytes.chunks_mut(4).enumerate() {
            bytes.copy_from_slice(
                &u32::from_str_radix(&host[index * 8..index * 8 + 8], 16)?.to_ne_bytes(),
            );
        }
        Ok(SocketAddr::new(
            std::net::Ipv6Addr::from(bytes).into(),
            port,
        ))
    } else {
        Ok(SocketAddr::new(
            std::net::Ipv4Addr::from(u32::from_str_radix(host, 16)?.to_ne_bytes()).into(),
            port,
        ))
    }
}

#[cfg(target_os = "linux")]
fn addresses_overlap(a: SocketAddr, b: SocketAddr) -> bool {
    a.port() == b.port()
        && (a.ip().is_unspecified()
            || b.ip().is_unspecified()
            || a.ip() == b.ip()
            || a.ip().to_canonical() == b.ip().to_canonical())
}

#[cfg(not(target_os = "linux"))]
fn listener_pids(address: SocketAddr) -> Result<Vec<u32>> {
    use hel::hel_targets::{CancellableProcessExecutor, CommandExecutor, CommandSpec};
    let executor = CancellableProcessExecutor::new(std::sync::Arc::new(
        std::sync::atomic::AtomicBool::new(false),
    ))
    .with_deadline(Duration::from_secs(5));
    let output = executor
        .execute(&CommandSpec::new(
            "lsof",
            [
                "-nP".to_owned(),
                "-a".into(),
                format!("-iTCP:{}", address.port()),
                "-sTCP:LISTEN".into(),
                "-Fp".into(),
            ],
        ))
        .context("Could not inspect this port; lsof must be installed")?;
    ensure!(
        output.status == 0 || (output.status == 1 && output.stderr.is_empty()),
        "Could not inspect this port: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)?
        .lines()
        .filter_map(|line| line.strip_prefix('p'))
        .map(|pid| pid.parse().context("invalid listener PID"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_controller::hel_server::{ServerRequests, ViewerSnapshot};
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn options(address: SocketAddr) -> ServerOptions {
        ServerOptions::new(
            address,
            watch::channel(ViewerSnapshot::default()).1,
            watch::channel(BTreeMap::new()).1,
            ServerRequests {
                action_tx: mpsc::channel(1).0,
                bundle_tx: mpsc::channel(1).0,
                receipt_tx: mpsc::channel(1).0,
                preflight_tx: mpsc::channel(1).0,
                move_preparation_tx: mpsc::channel(1).0,
                client_state_tx: mpsc::channel(1).0,
            },
        )
        .unwrap()
    }

    fn ready(address: SocketAddr) -> WebViewerAccess {
        WebViewerAccess::Ready {
            viewer_url: format!("http://{address}"),
            viewer_code: "123456".into(),
            qr_login_url: None,
            fallback_reason: None,
        }
    }

    async fn wait_access(
        control: &ViewerControl,
        predicate: impl Fn(&WebViewerAccess) -> bool,
    ) -> WebViewerAccess {
        let mut updates = control.access.subscribe();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let access = updates.borrow_and_update().clone();
                if predicate(&access) {
                    return access;
                }
                updates.changed().await.unwrap();
            }
        })
        .await
        .expect("viewer state did not arrive")
    }

    async fn http_response(address: SocketAddr) -> String {
        tokio::time::timeout(Duration::from_secs(3), async {
            let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
            stream
                .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).await.unwrap();
            response
        })
        .await
        .expect("viewer did not serve HTTP")
    }

    fn spawn_viewer(
        options: ServerOptions,
        control: Arc<ViewerControl>,
    ) -> tokio::task::JoinHandle<Result<()>> {
        tokio::spawn(async move {
            let ready = ready(options.bind);
            serve(options, ready, &control, |access| control.publish(access)).await
        })
    }

    #[tokio::test]
    async fn occupied_port_can_recover_on_another_reserved_port_and_serve_http() {
        let occupied = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = occupied.local_addr().unwrap();
        let options = options(address);
        let cancel = options.shutdown.clone();
        let control = Arc::new(ViewerControl::new());
        let task = spawn_viewer(options, control.clone());
        let failure = wait_access(&control, |access| {
            matches!(access, WebViewerAccess::Failed { .. })
        })
        .await;
        assert!(
            matches!(failure, WebViewerAccess::Failed { address: failed, port_conflict: true, .. } if failed == address)
        );
        control.recover(WebViewerRecovery::AnotherPort).unwrap();
        assert!(
            control.recover(WebViewerRecovery::AnotherPort).is_err(),
            "concurrent recovery must be rejected"
        );
        let access = wait_access(&control, |access| {
            matches!(access, WebViewerAccess::Ready { .. })
        })
        .await;
        let WebViewerAccess::Ready { viewer_url, .. } = access else {
            unreachable!()
        };
        let url = url::Url::parse(&viewer_url).unwrap();
        let actual = SocketAddr::new(address.ip(), url.port().unwrap());
        assert_ne!(actual.port(), address.port());
        assert_ne!(actual.port(), 0);
        assert!(http_response(actual).await.starts_with("HTTP/1.1 200"));
        assert!(
            TcpListener::bind(address).await.is_err(),
            "recovery must leave the existing listener alone"
        );
        assert!(
            control.recover(WebViewerRecovery::Retry).is_err(),
            "a healthy viewer must not be restarted"
        );
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn retry_uses_the_original_port_after_its_owner_releases_it() {
        let occupied = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = occupied.local_addr().unwrap();
        let options = options(address);
        let cancel = options.shutdown.clone();
        let control = Arc::new(ViewerControl::new());
        let task = spawn_viewer(options, control.clone());
        wait_access(&control, |access| {
            matches!(access, WebViewerAccess::Failed { .. })
        })
        .await;
        drop(occupied);
        control.recover(WebViewerRecovery::Retry).unwrap();
        let access = wait_access(&control, |access| {
            matches!(access, WebViewerAccess::Ready { .. })
        })
        .await;
        assert!(
            matches!(access, WebViewerAccess::Ready { viewer_url, .. } if viewer_url == format!("http://{address}/"))
        );
        assert!(http_response(address).await.starts_with("HTTP/1.1 200"));
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn shutdown_does_not_wait_for_a_port_conflict_to_be_resolved() {
        let occupied = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let options = options(occupied.local_addr().unwrap());
        let cancel = options.shutdown.clone();
        let control = Arc::new(ViewerControl::new());
        let task = spawn_viewer(options, control.clone());
        wait_access(&control, |access| {
            matches!(access, WebViewerAccess::Failed { .. })
        })
        .await;
        cancel.cancel();
        tokio::time::timeout(Duration::from_millis(500), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(control.recover(WebViewerRecovery::Retry).is_err());
    }

    #[test]
    fn changing_ports_preserves_https_hostname_and_login_credentials() {
        let ready = WebViewerAccess::Ready {
            viewer_url: "https://host.tailnet.ts.net:37650/".into(),
            viewer_code: "123456".into(),
            qr_login_url: Some("https://host.tailnet.ts.net:37650/auth/login?token=secret".into()),
            fallback_reason: None,
        };
        let changed = ready_at(&ready, 49152).unwrap();
        assert!(
            matches!(changed, WebViewerAccess::Ready { viewer_url, viewer_code, qr_login_url: Some(login), .. }
            if viewer_url == "https://host.tailnet.ts.net:49152/" && viewer_code == "123456" && login == "https://host.tailnet.ts.net:49152/auth/login?token=secret")
        );
        let ipv6 = ready_at(&self::ready("[::1]:0".parse().unwrap()), 49152).unwrap();
        assert!(
            matches!(ipv6, WebViewerAccess::Ready { viewer_url, .. } if viewer_url == "http://[::1]:49152/")
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn inspection_finds_real_listener_and_refuses_to_stop_the_current_daemon() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let pids = listener_pids(address).unwrap();
        assert!(
            pids.contains(&std::process::id()),
            "socket {address} owners: {pids:?}; expected {}",
            std::process::id()
        );
        let processes = tokio::task::spawn_blocking(move || inspect_listener(address))
            .await
            .unwrap()
            .unwrap();
        let own = processes
            .into_iter()
            .find(|process| process.pid == std::process::id())
            .expect("listener owner must be found");
        assert!(
            own.stop_disabled_reason
                .as_ref()
                .unwrap()
                .contains("current daemon")
        );
        let error = stop_listener(address, own, CancellationToken::new())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("current daemon"));
        assert!(TcpListener::bind(address).await.is_err());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn stopping_rejects_a_stale_inspected_identity() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let mut own = tokio::task::spawn_blocking(move || inspect_listener(address))
            .await
            .unwrap()
            .unwrap()
            .into_iter()
            .find(|process| process.pid == std::process::id())
            .unwrap();
        own.started_at = own.started_at.saturating_sub(1);
        let error = stop_listener(address, own, CancellationToken::new())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("identity changed"));
    }
}
