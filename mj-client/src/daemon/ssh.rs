//! SSH endpoint discovery, independent of CLI startup and local schema stores.
use anyhow::{Context, Result, bail, ensure};
use mj_core::config::DaemonConfig;
use mj_core::targets::daemon_login_command;
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::http::{HttpProof, MAX_PROOF_RESPONSE_BYTES, proof_nonce, verify_http_api_proof};

use super::{DaemonEndpoint, ssh_http_proxy::SshHttpProxy};
use crate::build_identity::{BuildIdentity, this_build};

/// Whether discovery may start the remote daemon.
#[derive(Clone, Copy)]
pub enum DiscoveryIntent {
    Start,
    Existing,
}

#[cfg(any(windows, test))]
fn has_log_option(args: &[String]) -> bool {
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let Some(options) = arg.strip_prefix('-') else {
            break;
        };
        if options.is_empty() || options.starts_with('-') {
            break;
        }
        let mut options = options.chars();
        while let Some(option) = options.next() {
            if option == 'E' {
                return true;
            }
            // OpenSSH consumes the rest of the option group or the next
            // argument as the value of these options.
            if "BbcDeFIiJLlmOoPpQRSWw".contains(option) {
                if options.as_str().is_empty() {
                    args.next();
                }
                break;
            }
        }
    }
    false
}

#[cfg(windows)]
fn redirect_client_log(
    command: &mut tokio::process::Command,
    ssh_args: &[String],
) -> Result<Option<tempfile::TempPath>> {
    if has_log_option(ssh_args) {
        return Ok(None);
    }
    // Inbox Win32 OpenSSH 9.5p2 can hang logging a connection error to
    // piped stderr (PowerShell/Win32-OpenSSH#2448). Close the temporary
    // file handle before ssh opens it with -E.
    let path = tempfile::Builder::new()
        .prefix("mj-ssh-")
        .suffix(".log")
        .tempfile()
        .context("create OpenSSH client log")?
        .into_temp_path();
    command
        .arg("-E")
        .arg(mj_core::targets::openssh_local_path(&path));
    Ok(Some(path))
}

#[cfg(any(windows, test))]
fn merge_diagnostics(stderr: &str, client_log: &str) -> String {
    [stderr.trim(), client_log.trim()]
        .into_iter()
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(windows)]
async fn diagnostics(stderr: &[u8], client_log: Option<&tempfile::TempPath>) -> String {
    let log = match client_log {
        Some(path) => tokio::fs::read(path).await.unwrap_or_default(),
        None => Vec::new(),
    };
    merge_diagnostics(
        &String::from_utf8_lossy(stderr),
        &String::from_utf8_lossy(&log),
    )
}

/// Run a remote daemon command, forwarding stderr and console stdout live.
pub async fn run_remote_forwarded(
    placement: &DaemonConfig,
    host: &str,
    argv: &[String],
    inherit_stdout: bool,
    inherit_stderr: bool,
) -> Result<std::process::ExitStatus> {
    ensure!(
        !host.starts_with('-'),
        "[daemon] host cannot be an SSH option"
    );
    let mut command = tokio::process::Command::new("ssh");
    #[cfg(windows)]
    let client_log = if inherit_stderr {
        None
    } else {
        redirect_client_log(&mut command, &placement.ssh_args)?
    };
    command
        .args(&placement.ssh_args)
        .arg(host)
        .arg(daemon_login_command(argv));
    let status = mj_core::subprocess::run_forwarded_with_optional_stdout_file(
        &mut command,
        inherit_stdout,
        inherit_stderr,
    )
    .await;
    #[cfg(windows)]
    if let Some(path) = client_log {
        use std::io::{self, Write};

        let log = tokio::fs::read(&path).await.unwrap_or_default();
        if !log.is_empty() {
            mj_core::runtime::spawn_blocking(move || -> io::Result<()> {
                let mut stderr = io::stderr().lock();
                stderr.write_all(&log)?;
                if !log.ends_with(b"\n") {
                    stderr.write_all(b"\n")?;
                }
                stderr.flush()
            })
            .await
            .context("OpenSSH client log forwarding task failed")?
            .context("write OpenSSH client log to stderr")?;
        }
    }
    status
}

async fn capture_remote(
    placement: &DaemonConfig,
    host: &str,
    argv: &[String],
    batch_mode: bool,
) -> Result<std::process::Output> {
    ensure!(
        !host.starts_with('-'),
        "[daemon] host cannot be an SSH option"
    );
    let mut command = tokio::process::Command::new("ssh");
    #[cfg(windows)]
    let client_log = redirect_client_log(&mut command, &placement.ssh_args)?;
    if batch_mode {
        command.args(["-o", "BatchMode=yes"]);
    }
    command
        .args(&placement.ssh_args)
        .arg(host)
        .arg(daemon_login_command(argv));
    #[cfg(not(windows))]
    {
        mj_core::subprocess::capture(&mut command).await
    }
    #[cfg(windows)]
    {
        let mut output = mj_core::subprocess::capture_with_stdout_file(&mut command).await?;
        let mut merged = diagnostics(&output.stderr, client_log.as_ref()).await;
        if !merged.is_empty() {
            merged.push('\n');
        }
        output.stderr = merged.into_bytes();
        Ok(output)
    }
}

/// Discover process-private remote credentials. No client-side daemon or store
/// is consulted. Dropping this future cancels the SSH process group.
pub async fn discover(
    placement: &DaemonConfig,
    remote_instance: Option<&str>,
    intent: DiscoveryIntent,
) -> Result<DaemonEndpoint> {
    let host = placement
        .host
        .as_deref()
        .context("[daemon] host is required for SSH placement")?;
    let mut args = vec![placement.remote_command().to_owned()];
    if let Some(instance) = remote_instance {
        args.extend(["--instance".into(), instance.into()]);
    }
    args.extend(["daemon".into(), "endpoint".into(), "--json".into()]);
    if matches!(intent, DiscoveryIntent::Existing) {
        args.push("--existing".into());
    }
    let output = capture_remote(placement, host, &args, true)
        .await
        .context("discover SSH daemon")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if output.status.code() == Some(127) {
            bail!(
                "[daemon] remote_command {:?} was not found on {host}; install mj remotely with scripts/install.sh (or install the same release). SSH: {}",
                placement.remote_command(),
                stderr.trim()
            );
        }
        bail!(
            "SSH daemon discovery failed ({}): {}",
            output.status,
            stderr.trim()
        );
    }
    let endpoint: DaemonEndpoint =
        serde_json::from_slice(&output.stdout).context("parse SSH daemon endpoint JSON")?;
    let remote = BuildIdentity::parse(&endpoint.build_version)?;
    ensure!(
        this_build().same_source(&remote),
        "SSH daemon build {} differs from client build {}; run scripts/dev-remote.ps1 or install the same release on both machines",
        remote.describe(),
        this_build().describe()
    );
    Ok(endpoint)
}

/// Process-private transport ownership. Dropping the lease stops and reaps SSH;
/// it never stops the remote daemon.
pub struct SshLease {
    pub local_addr: std::net::SocketAddr,
    local_api_addr: Option<std::net::SocketAddr>,
    endpoint: DaemonEndpoint,
    http_proxy: Option<SshHttpProxy>,
    child: mj_core::subprocess::SupervisedChild,
    #[cfg(windows)]
    client_log: Option<tempfile::TempPath>,
}

impl SshLease {
    pub fn api_endpoint(&self) -> Option<(String, String)> {
        self.http_proxy.as_ref().map(SshHttpProxy::endpoint)
    }

    pub(super) fn remote_config_path(&self) -> Option<&std::path::Path> {
        self.endpoint.config_path.as_deref()
    }

    /// Credentials for the forwarded connection, never persisted to disk.
    /// PID zero deliberately carries no local process authority.
    pub fn metadata(&self) -> super::DaemonMetadata {
        super::DaemonMetadata {
            protocol_version: self.endpoint.protocol_version,
            pid: 0,
            address: self.local_addr,
            token: self.endpoint.token.clone(),
            started_at: String::new(),
            build_version: self.endpoint.build_version.clone(),
            instance_id: self.endpoint.instance_id.clone(),
            placement: super::DaemonPlacement::Host,
        }
    }

    pub async fn terminate(&mut self) -> Result<()> {
        if let Some(proxy) = self.http_proxy.take() {
            proxy.stop().await;
        }
        self.child.terminate().await?;
        Ok(())
    }
}

/// Discover and authenticate a supervised tunnel. Cancellation (including
/// dropping this future) releases the process group without remote teardown.
pub async fn open(
    placement: &DaemonConfig,
    remote_instance: Option<&str>,
    intent: DiscoveryIntent,
    cancel: impl std::future::Future<Output = ()>,
) -> Result<SshLease> {
    tokio::pin!(cancel);
    tokio::select! {
        result = open_with_ports(placement, remote_instance, intent, select_ports) => result,
        () = &mut cancel => bail!("SSH daemon connection cancelled"),
    }
}

/// The `-L` destination for the discovered control address, as seen from the
/// remote host. A daemon listening on a wildcard is reached through loopback
/// of the same family; IPv6 needs brackets in OpenSSH forwarding syntax.
fn forward_destination(address: std::net::SocketAddr) -> String {
    let ip = match address.ip() {
        std::net::IpAddr::V4(ip) if ip.is_unspecified() => std::net::Ipv4Addr::LOCALHOST.into(),
        std::net::IpAddr::V6(ip) if ip.is_unspecified() => std::net::Ipv6Addr::LOCALHOST.into(),
        ip => ip,
    };
    match ip {
        std::net::IpAddr::V4(ip) => format!("{ip}:{}", address.port()),
        std::net::IpAddr::V6(ip) => format!("[{ip}]:{}", address.port()),
    }
}

fn select_ports(need_api: bool) -> Result<(u16, Option<u16>)> {
    let control = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let api = need_api
        .then(|| TcpListener::bind((Ipv4Addr::LOCALHOST, 0)))
        .transpose()?;
    // Keep both listeners alive until both ports have been selected.
    let api_port = api
        .as_ref()
        .map(|listener| listener.local_addr().map(|addr| addr.port()))
        .transpose()?;
    Ok((control.local_addr()?.port(), api_port))
}

async fn distinct_ports(
    need_api: bool,
    select: &mut impl FnMut(bool) -> Result<(u16, Option<u16>)>,
) -> Result<(u16, Option<u16>)> {
    loop {
        let (control, api) = select(need_api)?;
        if api != Some(control) {
            return Ok((control, api));
        }
        // Back off as in the readiness loop so a failing selector cannot spin.
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn open_with_ports(
    placement: &DaemonConfig,
    remote_instance: Option<&str>,
    intent: DiscoveryIntent,
    mut select: impl FnMut(bool) -> Result<(u16, Option<u16>)>,
) -> Result<SshLease> {
    let mut endpoint = discover(placement, remote_instance, intent).await?;
    loop {
        let remote_api = endpoint.api_address.zip(
            endpoint
                .api_token
                .as_deref()
                .filter(|token| !token.is_empty()),
        );
        let (control_port, api_port) = distinct_ports(remote_api.is_some(), &mut select).await?;
        let local_addr = SocketAddr::from((Ipv4Addr::LOCALHOST, control_port));
        let mut command = tokio::process::Command::new("ssh");
        #[cfg(windows)]
        let client_log = redirect_client_log(&mut command, &placement.ssh_args)?;
        // The tunnel needs its own connection. Through a shared ControlMaster
        // connection the master would own the listener, and stopping this child would
        // not remove the forward. OpenSSH keeps the first value of an option,
        // so these override the user's `ssh_args` and ssh_config.
        command.args([
            "-N",
            "-o",
            "ControlMaster=no",
            "-o",
            "ControlPath=none",
            "-o",
            "ExitOnForwardFailure=yes",
            "-o",
            "BatchMode=yes",
        ]);
        #[cfg(not(windows))]
        command.args(&placement.ssh_args);
        command.arg("-L").arg(format!(
            "127.0.0.1:{}:{}",
            local_addr.port(),
            forward_destination(endpoint.control_address)
        ));
        let local_api_addr = if let Some((address, _)) = remote_api {
            let api_port = api_port.context("SSH port selector omitted the HTTP port")?;
            command.arg("-L").arg(format!(
                "127.0.0.1:{api_port}:{}",
                forward_destination(address)
            ));
            Some(SocketAddr::from((Ipv4Addr::LOCALHOST, api_port)))
        } else {
            None
        };
        #[cfg(windows)]
        command.args(&placement.ssh_args);
        command.arg(placement.host.as_deref().context("missing SSH host")?);
        let child = mj_core::subprocess::SupervisedChild::spawn(&mut command)?;
        let mut lease = SshLease {
            local_addr,
            local_api_addr,
            endpoint: endpoint.clone(),
            http_proxy: None,
            child,
            #[cfg(windows)]
            client_log,
        };
        // Each forward proves possession of its token without the client
        // sending it. There is no deadline. A silent
        // listener that is not ssh ends when ssh exits on its failed bind
        // (the select below), and a slow link only makes the proof slower.
        let ready = async {
            // Bounds only the pause between probes, never the work.
            const MAX_PROBE_DELAY: std::time::Duration = std::time::Duration::from_secs(2);
            let mut delay = std::time::Duration::from_millis(100);
            loop {
                let proof = async {
                    if !prove_listener(local_addr, &endpoint.token).await? {
                        return Ok(false);
                    }
                    if let Some((api_addr, (_, api_token))) = local_api_addr.zip(remote_api) {
                        return prove_http_listener(api_addr, api_token).await;
                    }
                    Ok(true)
                }
                .await;
                match proof {
                    Ok(true) => return Ok::<_, anyhow::Error>(None),
                    // Nothing listens yet: ssh is still connecting.
                    Err(ref error)
                        if error
                            .downcast_ref::<std::io::Error>()
                            .is_some_and(|e| e.kind() == std::io::ErrorKind::ConnectionRefused) => {
                    }
                    // A wrong or garbled proof, or a dropped connection: either
                    // the daemon was replaced after discovery, or the listener
                    // is another local process. Discovery tells them apart
                    // without sending the token anywhere.
                    _ => {
                        let fresh = discover(placement, remote_instance, intent).await?;
                        if fresh.instance_id != endpoint.instance_id
                            || fresh.control_address != endpoint.control_address
                            || fresh.api_address != endpoint.api_address
                            || fresh.api_token != endpoint.api_token
                        {
                            return Ok(Some(fresh));
                        }
                        delay = (delay * 2).min(MAX_PROBE_DELAY);
                    }
                }
                tokio::time::sleep(delay).await;
            }
        };
        tokio::select! {
            outcome = ready => {
                match outcome? {
                    None => {
                        if let Some((address, token)) =
                            lease.local_api_addr.zip(lease.endpoint.api_token.take())
                        {
                            lease.http_proxy = Some(SshHttpProxy::start(address, token).await?);
                        }
                        return Ok(lease);
                    }
                    // The daemon was replaced after discovery: the new endpoint
                    // is the state change that restarts the tunnel.
                    Some(fresh) => {
                        lease.terminate().await?;
                        endpoint = fresh;
                    }
                }
            }
            status = lease.child.wait() => {
                let status = status?;
                #[cfg(not(windows))]
                let stderr = String::from_utf8_lossy(&lease.child.stderr()).into_owned();
                #[cfg(windows)]
                let stderr = diagnostics(&lease.child.stderr(), lease.client_log.as_ref()).await;
                let collision = stderr.lines().any(|line|
                    (line.contains("bind") && line.contains("Address already in use"))
                    || line.contains("cannot listen to port"));
                if collision { continue; }
                bail!("SSH daemon tunnel exited ({status}): {}", stderr.lines().filter(|line| !line.starts_with("debug") && !line.starts_with("OpenSSH")).collect::<Vec<_>>().join("\n"));
            }
        }
    }
}

async fn prove_listener(address: std::net::SocketAddr, token: &str) -> Result<bool> {
    let mut stream = tokio::net::TcpStream::connect(address).await?;
    prove_stream(&mut stream, token).await
}

async fn prove_http_listener(address: SocketAddr, token: &str) -> Result<bool> {
    let mut stream = tokio::net::TcpStream::connect(address).await?;
    let nonce = proof_nonce()?;
    let request = format!(
        "GET /api/v1/prove?nonce={nonce} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await?;
    let mut response = Vec::new();
    stream
        .take((MAX_PROOF_RESPONSE_BYTES + 1) as u64)
        .read_to_end(&mut response)
        .await?;
    if response.len() > MAX_PROOF_RESPONSE_BYTES {
        return Ok(false);
    }
    let Ok(response) = std::str::from_utf8(&response) else {
        return Ok(false);
    };
    let Some((headers, body)) = response.split_once("\r\n\r\n") else {
        return Ok(false);
    };
    let mut status = headers
        .lines()
        .next()
        .unwrap_or_default()
        .split_whitespace();
    if !matches!(status.next(), Some("HTTP/1.0" | "HTTP/1.1")) || status.next() != Some("200") {
        return Ok(false);
    }
    Ok(serde_json::from_str::<HttpProof>(body)
        .is_ok_and(|proof| verify_http_api_proof(token, &nonce, &proof.mac)))
}

/// Ask the peer on `stream` to prove it holds `token`, sending only a nonce.
/// Every SSH-placement connection runs this before its first token-bearing
/// request: a proof covers only the TCP connection that carried it.
pub(super) async fn prove_stream(stream: &mut tokio::net::TcpStream, token: &str) -> Result<bool> {
    use hmac::Mac;
    let mut random = [0u8; 32];
    getrandom::fill(&mut random).map_err(|error| anyhow::anyhow!("{error}"))?;
    let nonce = mj_core::hex::lower_hex(random);
    super::write_frame(
        stream,
        &super::RequestEnvelope {
            protocol_version: super::PROTOCOL_VERSION,
            request_id: 0,
            token: String::new(),
            action: super::DaemonAction::ProveIdentity {
                nonce: nonce.clone(),
            },
        },
    )
    .await?;
    let response = super::read_frame::<super::ResponseEnvelope>(stream).await?;
    Ok(
        matches!(response.result, Ok(super::DaemonReply::IdentityProof { mac })
        if super::identity_proof(token, &nonce).verify_slice(&mac).is_ok()),
    )
}

#[cfg(test)]
mod tests {
    #[tokio::test(start_paused = true)]
    async fn ssh_port_selection_retries_collision_and_reserves_distinct_ports() {
        let mut attempts = 0;
        let (control, api) = super::distinct_ports(true, &mut |need_api| {
            assert!(need_api);
            attempts += 1;
            if attempts == 1 {
                return Ok((12345, Some(12345)));
            }
            super::select_ports(need_api)
        })
        .await
        .unwrap();
        assert_eq!(attempts, 2);
        assert_ne!(control, api.unwrap());
        assert!(super::select_ports(false).unwrap().1.is_none());
    }

    #[tokio::test]
    async fn http_proof_never_sends_bearer_and_rejects_an_imposter() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        const TOKEN: &str = "proof-test-secret";
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = async {
            for (status, valid_mac) in [
                ("401 Unauthorized", false),
                ("200 OK", false),
                ("200 OK", true),
            ] {
                let (stream, _) = listener.accept().await.unwrap();
                let mut stream = BufReader::new(stream);
                let mut line = String::new();
                assert_ne!(stream.read_line(&mut line).await.unwrap(), 0);
                let nonce = line
                    .strip_prefix("GET /api/v1/prove?nonce=")
                    .unwrap()
                    .split_whitespace()
                    .next()
                    .unwrap()
                    .to_owned();
                assert!(crate::http::is_valid_proof_nonce(&nonce));
                loop {
                    line.clear();
                    assert_ne!(stream.read_line(&mut line).await.unwrap(), 0);
                    if line == "\r\n" {
                        break;
                    }
                    assert!(!line.to_ascii_lowercase().starts_with("authorization:"));
                    assert!(!line.contains(TOKEN));
                }
                let key = if valid_mac { TOKEN } else { "wrong secret" };
                let mac = crate::http::http_api_proof_hex(key, &nonce);
                let body = serde_json::json!({ "mac": mac }).to_string();
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        };

        let client = async {
            for expected in [false, false, true] {
                assert_eq!(
                    super::prove_http_listener(address, TOKEN).await.unwrap(),
                    expected
                );
            }
        };

        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            tokio::join!(server, client);
        })
        .await
        .unwrap();
    }

    #[test]
    fn remote_endpoint_json_works_with_old_and_new_discovery_clients() {
        let old = serde_json::json!({
            "protocol_version": 1,
            "build_version": "2.0.0",
            "control_address": "127.0.0.1:8787",
            "token": "control",
            "instance_id": "started",
        });
        let endpoint: super::DaemonEndpoint = serde_json::from_value(old.clone()).unwrap();
        assert!(endpoint.api_address.is_none());
        assert!(endpoint.api_token.is_none());
        let mut new = old;
        new["api_address"] = serde_json::json!("[::1]:8788");
        new["api_token"] = serde_json::json!("api");
        let endpoint: super::DaemonEndpoint = serde_json::from_value(new).unwrap();
        let round_trip = serde_json::to_value(endpoint).unwrap();
        assert_eq!(round_trip["api_address"], "[::1]:8788");
        assert_eq!(round_trip["api_token"], "api");
    }

    #[test]
    fn detects_log_options_without_matching_option_values() {
        for (args, expected) in [
            (&["-E", "user.log"][..], true),
            (&["-Euser.log"][..], true),
            (&["-p", "2222", "-vE", "user.log"][..], true),
            (&["-o", "-Evalue", "-E", "user.log"][..], true),
            (&["-o", "-Evalue", "-i", "-Eidentity"][..], false),
            (&["-vo-Evalue", "-p22"][..], false),
            (&["-p", "2222", "--", "-E"][..], false),
            (&["host", "-E"][..], false),
            (&[][..], false),
        ] {
            let owned = args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
            assert_eq!(super::has_log_option(&owned), expected, "{args:?}");
        }
    }

    #[test]
    fn merges_client_log_with_remote_stderr() {
        for (stderr, log, expected) in [
            ("", " \r\n", ""),
            ("", "connection refused\r\n", "connection refused"),
            ("remote error\n", "", "remote error"),
            (
                " remote error\r\n",
                " connection refused\r\n",
                "remote error\nconnection refused",
            ),
        ] {
            assert_eq!(super::merge_diagnostics(stderr, log), expected);
        }
    }

    // Hard-won: 5159ed294: a forwarding diagnostic printed before bind() released the token to another listener
    #[tokio::test]
    async fn competing_listener_never_receives_control_token() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let token = "private-control-token-that-must-never-cross-the-wire";
        let peer = async {
            let (mut stream, _) = listener.accept().await.unwrap();
            let length = stream.read_u32().await.unwrap();
            let mut bytes = vec![0; length as usize];
            stream.read_exact(&mut bytes).await.unwrap();
            assert!(!String::from_utf8_lossy(&bytes).contains(token));
            let request: super::super::RequestEnvelope = serde_json::from_slice(&bytes).unwrap();
            assert!(request.token.is_empty());
            assert!(matches!(
                request.action,
                super::super::DaemonAction::ProveIdentity { .. }
            ));
            stream.write_all(&[0, 0, 0, 1, b'!']).await.unwrap();
            let mut remainder = Vec::new();
            stream.read_to_end(&mut remainder).await.unwrap();
            assert!(remainder.is_empty());
        };
        // Every SSH-placement connection goes through connect_proven, so a
        // proof made earlier on another connection never vouches for this one.
        let metadata = super::super::DaemonMetadata {
            protocol_version: super::super::PROTOCOL_VERSION,
            pid: 0,
            address,
            token: token.into(),
            started_at: String::new(),
            build_version: String::new(),
            instance_id: String::new(),
            placement: super::super::DaemonPlacement::Host,
        };
        let (result, ()) = tokio::join!(super::super::DaemonClient::connect_proven(metadata), peer);
        assert!(result.is_err());
    }

    /// Run against a disposable sshd with a real, matching mj, never a live
    /// instance. The driver supplies isolated remote config/data directories.
    #[tokio::test]
    #[ignore = "requires MJ_SSH_DISCOVERY_PLACEMENT pointing at a disposable sshd fixture"]
    async fn real_ssh_endpoint_discovery() {
        let path = std::env::var_os("MJ_SSH_DISCOVERY_PLACEMENT").unwrap();
        let placement =
            mj_core::config::DaemonConfig::load_placement(std::path::Path::new(&path)).unwrap();
        let mut connector_placement = placement.clone();
        connector_placement.instance = Some("ssh-discovery-e2e".into());
        let connector =
            super::super::connector::DaemonConnector::for_placement(connector_placement);
        let mut connected = connector
            .connect_or_start(|| async { panic!("SSH must not run local startup") })
            .await
            .unwrap();
        let first_identity = connected.identity().await.unwrap();
        let old_lease = std::sync::Arc::downgrade(connected.ssh_lease.as_ref().unwrap());
        let old_address = connected.metadata.address;
        let mut second_client = connector.connect_existing().await.unwrap();
        assert_eq!(
            second_client.identity().await.unwrap().instance_id,
            first_identity.instance_id
        );
        let argv = vec![
            placement.remote_command().to_owned(),
            "--instance".into(),
            "ssh-discovery-e2e".into(),
            "daemon".into(),
            "restart".into(),
        ];
        let mut restart = tokio::process::Command::new("ssh");
        restart
            .args(["-o", "BatchMode=yes"])
            .args(&placement.ssh_args)
            .arg(placement.host.as_ref().unwrap())
            .arg(mj_core::targets::daemon_login_command(&argv));
        let restarted = mj_core::subprocess::capture(&mut restart).await.unwrap();
        assert!(
            restarted.status.success(),
            "{}",
            String::from_utf8_lossy(&restarted.stderr)
        );
        let (left, right) =
            tokio::join!(connector.connect_existing(), connector.connect_existing());
        let new_identity = left.unwrap().identity().await.unwrap();
        assert_ne!(new_identity.instance_id, first_identity.instance_id);
        assert_eq!(
            right.unwrap().identity().await.unwrap().instance_id,
            new_identity.instance_id
        );
        // Hard-won: #1514: refreshing one client killed another client's tunnel.
        assert!(old_lease.upgrade().is_some());
        tokio::net::TcpStream::connect(old_address).await.unwrap();
        drop(connected);
        drop(second_client);
        assert!(old_lease.upgrade().is_none());
        drop(connector);
        let endpoint = super::discover(
            &placement,
            Some("ssh-discovery-e2e"),
            super::DiscoveryIntent::Start,
        )
        .await
        .unwrap();
        assert!(endpoint.control_address.ip().is_loopback());
        let existing = super::discover(
            &placement,
            Some("ssh-discovery-e2e"),
            super::DiscoveryIntent::Existing,
        )
        .await
        .unwrap();
        assert_eq!(existing.token, endpoint.token);
        assert_eq!(existing.instance_id, endpoint.instance_id);
        let mut lease = super::open(
            &placement,
            Some("ssh-discovery-e2e"),
            super::DiscoveryIntent::Existing,
            std::future::pending(),
        )
        .await
        .unwrap();
        assert_eq!(lease.endpoint.instance_id, endpoint.instance_id);
        let (url, token) = lease
            .api_endpoint()
            .expect("the remote publishes its HTTP API");
        assert_ne!(
            lease.local_api_addr.unwrap().port(),
            lease.local_addr.port()
        );
        let http = crate::http::HttpDaemonClient::new(url, token).unwrap();
        http.get_json::<serde_json::Value>("/api/v1/options")
            .await
            .unwrap();
        lease.terminate().await.unwrap();
        let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let occupied_port = occupied.local_addr().unwrap().port();
        occupied.set_nonblocking(true).unwrap();
        let mut first = true;
        let mut second = super::open_with_ports(
            &placement,
            Some("ssh-discovery-e2e"),
            super::DiscoveryIntent::Existing,
            |need_api| {
                let (control, api) = super::select_ports(need_api)?;
                if first {
                    first = false;
                    Ok((occupied_port, api))
                } else {
                    Ok((control, api))
                }
            },
        )
        .await
        .unwrap();
        assert!(!first);
        assert_ne!(second.local_addr.port(), occupied_port);
        assert_eq!(second.endpoint.instance_id, endpoint.instance_id);
        second.terminate().await.unwrap();
        let mut missing = placement.clone();
        missing.remote_command = Some("/missing/mj-discovery-test".into());
        let error = super::discover(
            &missing,
            Some("ssh-discovery-e2e"),
            super::DiscoveryIntent::Existing,
        )
        .await
        .err()
        .unwrap()
        .to_string();
        assert!(error.contains("[daemon] remote_command"), "{error}");
        assert!(error.contains("scripts/install.sh"), "{error}");
    }
}
