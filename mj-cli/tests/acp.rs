//! `mj acp` is an ACP agent.
//!
//! This drives the real binary over pipes using the same SDK's client role, so
//! it exercises the transport, the role wiring, and the handshake rather than a
//! function that stands in for them.

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::InitializeRequest;
use agent_client_protocol::{AcpAgent, AcpAgentConfig, Client};

/// The adapter answers a handshake, and reaching it never starts a daemon: a
/// consumer must be able to launch the agent before anything is configured.
#[tokio::test]
async fn the_acp_command_answers_initialize_without_starting_a_daemon() {
    let storage = tempfile::tempdir().unwrap();
    let data = storage.path().join("data");
    let config = storage.path().join("config");
    let agent = AcpAgent::new(
        AcpAgentConfig::new(env!("CARGO_BIN_EXE_mj"))
            .arg("acp")
            .arg("--workspace")
            .arg("editor")
            .env("MJ_DATA_DIR", data.display().to_string())
            .env("MJ_CONFIG_DIR", config.display().to_string()),
    );

    let response = Client
        .builder()
        .name("mj-acp-test")
        .connect_with(agent, async |cx| {
            cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await
        })
        .await
        .expect("the adapter answers initialize");

    assert_eq!(
        response.protocol_version,
        ProtocolVersion::V1,
        "the adapter speaks the version it implements"
    );
    assert!(
        !data.join("daemon.json").exists(),
        "answering a handshake must not start the daemon"
    );
}

/// Launch finding H-3: every session lives in a workspace the dashboard and
/// the viewer list, so `mj acp` without `--workspace` exits at once and says
/// how to make one. It still starts no daemon to say so.
#[test]
fn the_acp_command_without_a_workspace_exits_with_how_to_make_one() {
    let storage = tempfile::tempdir().unwrap();
    let data = storage.path().join("data");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_mj"))
        .arg("acp")
        .env("MJ_DATA_DIR", data.display().to_string())
        .env(
            "MJ_CONFIG_DIR",
            storage.path().join("config").display().to_string(),
        )
        .env("MJ_DAEMON_OWNER_PID", std::process::id().to_string())
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("mj acp needs --workspace NAME"), "{stderr}");
    assert!(stderr.contains("mj workspaces create NAME"), "{stderr}");
    assert!(
        !data.join("daemon.json").exists(),
        "refusing must not start the daemon"
    );
}

/// A consumer that stops its agent with a signal rather than closing the pipe
/// still gets its exit policy, and an adapter that created nothing has nothing
/// to retire, so it leaves promptly and successfully without reaching for a
/// daemon.
#[cfg(unix)]
#[tokio::test]
async fn a_terminated_adapter_applies_its_exit_policy_and_exits_cleanly() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let storage = tempfile::tempdir().unwrap();
    let data = storage.path().join("data");
    let config = storage.path().join("config");
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_mj"))
        .args(["acp", "--workspace", "editor", "--on-exit", "destroy"])
        .env("MJ_DATA_DIR", &data)
        .env("MJ_CONFIG_DIR", &config)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("start the adapter");
    // One small request and one line of answer: the handshake proves the
    // adapter is serving, and so is already listening for the signal.
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":1}}\n",
        )
        .await
        .unwrap();
    let mut answer = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        BufReader::new(child.stdout.take().unwrap()).read_line(&mut answer),
    )
    .await
    .expect("the adapter answers the handshake")
    .unwrap();
    assert!(answer.contains("\"id\":1"), "{answer}");

    let pid = i32::try_from(child.id().expect("the adapter is running")).unwrap();
    // SAFETY: `kill` only sends a signal to the child this test started.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    let status = tokio::time::timeout(std::time::Duration::from_secs(30), child.wait())
        .await
        .expect("the adapter leaves after the signal")
        .unwrap();

    assert!(
        status.success(),
        "the signal ends the adapter through its exit path, not by killing it: {status:?}"
    );
    assert!(
        !data.join("daemon.json").exists(),
        "an adapter with no sessions has nothing to retire and no daemon to reach"
    );
    drop(stdin);
}
