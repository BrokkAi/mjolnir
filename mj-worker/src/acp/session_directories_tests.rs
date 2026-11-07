use super::grok_usage_tests::spec;
use super::*;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// What Grok Build 1.0.70 answers to `initialize`, recorded from the pinned
/// binary: its `sessionCapabilities` has no `additionalDirectories`.
const GROK_INITIALIZE: &str = include_str!("testdata/grok_1_0_70_initialize.json");

/// Open a session against a bridge that answers `initialize` with the given
/// result and rejects a non-empty `additionalDirectories` the way Grok does,
/// unless that result advertises the capability.
/// Returns the warnings the worker raised and the directory lists the bridge
/// received on the session request.
async fn open_session(
    initialize: serde_json::Value,
    spec: LaunchSpec,
) -> (Vec<String>, Vec<serde_json::Value>) {
    let (client, bridge) = tokio::io::duplex(64 * 1024);
    let accepts_directories =
        initialize["agentCapabilities"]["sessionCapabilities"]["additionalDirectories"].is_object();
    let bridge = tokio::spawn(async move {
        let (read, mut write) = tokio::io::split(bridge);
        let mut lines = BufReader::new(read).lines();
        let mut seen = Vec::new();
        while let Some(line) = lines.next_line().await.unwrap() {
            let request: serde_json::Value = serde_json::from_str(&line).unwrap();
            let id = request["id"].clone();
            let response = match request["method"].as_str().unwrap_or("") {
                "initialize" => serde_json::json!({"jsonrpc":"2.0","id":id,"result":initialize}),
                "session/new" | "session/load" | "session/resume" => {
                    let directories = request["params"]["additionalDirectories"].clone();
                    seen.push(directories.clone());
                    if !accepts_directories
                        && directories.as_array().is_some_and(|list| !list.is_empty())
                    {
                        serde_json::json!({"jsonrpc":"2.0","id":id,"error":{
                            "code":-32602,"message":"Invalid params",
                            "data":"additionalDirectories is not supported: a session works only in its cwd"}})
                    } else {
                        serde_json::json!({"jsonrpc":"2.0","id":id,"result":{"sessionId":"grok-test"}})
                    }
                }
                _ => continue,
            };
            if write
                .write_all(format!("{response}\n").as_bytes())
                .await
                .is_err()
            {
                break;
            }
        }
        seen
    });
    let (read, write) = tokio::io::split(client);
    let transport = ByteStreams::new(write.compat_write(), read.compat());
    let (requests, mut request_rx) = mpsc::channel(4);
    let (events, mut event_rx) = mpsc::channel(32);
    let driver = tokio::spawn(async move {
        drive(
            transport,
            spec,
            &mut request_rx,
            events,
            Arc::new(Mutex::new(None)),
            false,
        )
        .await
    });
    let mut warnings = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), event_rx.recv())
            .await
            .expect("the session opens or the worker reports why not")
            .expect("the runtime stopped before the session opened");
        match event {
            RuntimeEvent::Warning { message } => warnings.push(message),
            RuntimeEvent::SessionStarted { .. } => break,
            _ => {}
        }
    }
    drop(requests);
    drop(event_rx);
    let _ = tokio::time::timeout(Duration::from_secs(10), driver).await;
    let seen = tokio::time::timeout(Duration::from_secs(10), bridge)
        .await
        .unwrap()
        .unwrap();
    (warnings, seen)
}

fn spec_with_directories() -> LaunchSpec {
    let mut spec = spec(
        "fake".into(),
        BTreeMap::from([("MJ_TOOL_CACHE_DIR".into(), "/cache/tools".into())]),
        std::env::current_dir().unwrap(),
    );
    spec.project_memory = Some(ProjectMemoryLaunchConfig {
        history_socket: None,
        project_key: "project".into(),
        root: "/profile/memory".into(),
        baseline_root: PathBuf::new(),
        repository_roots: BTreeMap::new(),
        mcp_delivery: ProjectMemoryMcpDelivery::Acp,
    });
    spec.additional_directories = vec![
        "/workspace/second".into(),
        "/profile/memory".into(),
        "/cache/tools".into(),
    ];
    spec
}

// Hard-won: #1433: Grok Build rejects additionalDirectories and the session
// never started; the worker sent the memory root and tool cache to every
// harness that is not Claude or Muse.
#[tokio::test]
async fn an_agent_without_the_additional_directories_capability_gets_none_and_the_person_is_told() {
    let initialize: serde_json::Value = serde_json::from_str(GROK_INITIALIZE).unwrap();
    assert!(
        initialize["agentCapabilities"]["sessionCapabilities"]
            .get("additionalDirectories")
            .is_none(),
        "the recorded Grok response is the one that lacks the capability"
    );

    let (warnings, seen) = open_session(initialize, spec_with_directories()).await;

    // An empty list is left off the wire.
    assert_eq!(seen, [serde_json::Value::Null]);
    let notice = warnings
        .iter()
        .find(|message| message.contains("additional directories"))
        .expect("the dropped directories are reported");
    assert!(notice.starts_with("Grok Build does not accept"), "{notice}");
    for named in [
        "workspace directory /workspace/second",
        "project memory /profile/memory",
        "tool cache /cache/tools",
    ] {
        assert!(notice.contains(named), "{named} missing from: {notice}");
    }
}

#[tokio::test]
async fn an_agent_that_advertises_the_capability_receives_every_directory() {
    let mut initialize: serde_json::Value = serde_json::from_str(GROK_INITIALIZE).unwrap();
    initialize["agentCapabilities"]["sessionCapabilities"]["additionalDirectories"] =
        serde_json::json!({});
    let spec = spec_with_directories();
    let expected = serde_json::to_value(&spec.additional_directories).unwrap();

    let (warnings, seen) = open_session(initialize, spec).await;

    assert_eq!(seen, [expected]);
    assert!(
        warnings
            .iter()
            .all(|message| !message.contains("additional directories")),
        "{warnings:?}"
    );
}
