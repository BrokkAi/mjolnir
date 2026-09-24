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
