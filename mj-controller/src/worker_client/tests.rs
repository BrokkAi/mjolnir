use super::*;
use mj_core::relay::RelayObservation;
use mj_worker::relay::DurableRelay;
const SESSION_ID: &str = "018f9dd2-a3b4-7c8d-9000-123456789abc";

#[test]
fn relay_decoder_preserves_explicit_desynchronization() {
    let response = RelayResponseEnvelope {
        request_id: "relay-1".into(),
        protocol_version: RELAY_PROTOCOL_VERSION,
        body: RelayResponseBody::Error {
            error: RelayProtocolError {
                code: RelayErrorCode::Desynchronized,
                message: "journal gap".into(),
                retryable: false,
                detail: None,
            },
        },
    };
    let encoded = serde_json::to_string(&response).unwrap();
    let error = decode_relay_response(&encoded, "relay-1", RELAY_PROTOCOL_VERSION).unwrap_err();
    assert!(
        error
            .downcast_ref::<RelayRejected>()
            .is_some_and(RelayRejected::is_desynchronized)
    );
}

#[test]
fn relay_decoder_rejects_crossed_request_ids() {
    let response = RelayResponseEnvelope {
        request_id: "other".into(),
        protocol_version: RELAY_PROTOCOL_VERSION,
        body: RelayResponseBody::Ok {
            payload: RelayResponsePayload::Acknowledged {
                through_ordinal: 4,
                through_digest: "a".repeat(64),
            },
        },
    };
    let encoded = serde_json::to_string(&response).unwrap();
    assert!(
        decode_relay_response(&encoded, "wanted", RELAY_PROTOCOL_VERSION)
            .unwrap_err()
            .to_string()
            .contains("ID mismatch")
    );
}

#[test]
fn command_spec_preserves_argv_boundaries() {
    let spec = CommandSpec::new("ssh", ["host", "hel worker proxy --root '/odd path'"]);
    assert_eq!(spec.program, "ssh");
    assert_eq!(spec.args.len(), 2);
    assert_eq!(spec.args[1], "hel worker proxy --root '/odd path'");
}

#[test]
fn relay_protocol_version_range_contains_current_version() {
    assert_eq!(
        RelayVersionRange::CURRENT.negotiate(RelayVersionRange::CURRENT),
        Some(RELAY_PROTOCOL_VERSION)
    );
    assert_eq!(
        RelayVersionRange::CURRENT.negotiate(RelayVersionRange { min: 1, max: 1 }),
        Some(1)
    );
}

/// A host at its `MaxStartups` ceiling drops the surplus connection before
/// authentication. That says nothing about the worker, so connect retries
/// instead of reporting a dead relay and triggering worker recovery.
#[cfg(unix)]
#[tokio::test]
async fn a_relay_proxy_refused_by_sshd_is_retried_rather_than_reported_dead() {
    mj_core::targets::set_ssh_retry_backoff_for_test(Some(Duration::from_millis(5)));
    let directory = tempfile::tempdir().expect("temp dir");
    let counter = directory.path().join("attempts");
    let script = format!(
        r#"
count=$(cat {counter} 2>/dev/null || echo 0)
echo $((count + 1)) > {counter}
if [ "$count" -eq 0 ]; then
  echo 'kex_exchange_identification: read: Connection reset by peer' >&2
  exit 255
fi
IFS= read -r hello
id=$(printf '%s' "$hello" | sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p')
printf '{{"request_id":"%s","protocol_version":1,"result":"ok","payload":{{"type":"hello","data":{{"negotiated":1,"relay_version":"retry-fixture","session_id":"{session}"}}}}}}\n' "$id"
while IFS= read -r request; do :; done
"#,
        counter = counter.display(),
        session = SESSION_ID
    );
    let spec = CommandSpec::new("sh", ["-c".to_owned(), script])
        .ssh_destination("build@10.0.0.1")
        .purpose("refused relay fixture");

    let client = RelayClient::connect_with_timeout(&spec, SESSION_ID, Duration::from_secs(10))
        .await
        .expect("a refused connection must be retried, not reported as a dead relay");

    assert_eq!(client.relay_version(), "retry-fixture");
    assert_eq!(
        std::fs::read_to_string(&counter)
            .expect("the fixture records its attempts")
            .trim(),
        "2"
    );
    mj_core::targets::set_ssh_retry_backoff_for_test(None);
}

/// A proxy that fails for its own reasons is reported on the first
/// attempt, keeping the existing hello error and its stderr tail.
#[cfg(unix)]
#[tokio::test]
async fn a_relay_proxy_that_fails_for_another_reason_is_not_retried() {
    mj_core::targets::set_ssh_retry_backoff_for_test(Some(Duration::from_millis(5)));
    let directory = tempfile::tempdir().expect("temp dir");
    let counter = directory.path().join("attempts");
    let script = format!(
        r#"
count=$(cat {counter} 2>/dev/null || echo 0)
echo $((count + 1)) > {counter}
echo 'worker socket path is too long' >&2
exit 1
"#,
        counter = counter.display()
    );
    let spec = CommandSpec::new("sh", ["-c".to_owned(), script])
        .ssh_destination("build@10.0.0.1")
        .purpose("broken relay fixture");

    let Err(error) =
        RelayClient::connect_with_timeout(&spec, SESSION_ID, Duration::from_secs(10)).await
    else {
        panic!("a proxy that exits 1 is a real failure");
    };

    let reported = format!("{error:#}");
    assert!(
        reported.contains("relay proxy disconnected during hello"),
        "unexpected error: {reported}"
    );
    assert!(
        reported.contains("worker socket path is too long"),
        "the stderr tail must survive: {reported}"
    );
    assert_eq!(
        std::fs::read_to_string(&counter)
            .expect("the fixture records its attempts")
            .trim(),
        "1"
    );
    mj_core::targets::set_ssh_retry_backoff_for_test(None);
}

#[cfg(unix)]
#[tokio::test]
async fn controller_accepts_negotiated_protocol_v1() {
    let script = format!(
        r#"python3 -c '
import json, sys
session = {session:?}
req = json.loads(sys.stdin.readline())
assert req["request"]["method"] == "hello"
supported = req["request"]["params"]["supported"]
assert supported["min"] <= 1 <= supported["max"]
print(json.dumps({{
"request_id": req["request_id"],
"protocol_version": 1,
"result": "ok",
"payload": {{
    "type": "hello",
    "data": {{
        "negotiated": 1,
        "relay_version": "v1-fixture",
        "session_id": session,
    }},
}},
}}), flush=True)
sys.stdin.read()
'"#,
        session = SESSION_ID
    );
    let spec = CommandSpec::new("sh", ["-c", &script]).purpose("v1 relay fixture");
    let client = RelayClient::connect_with_timeout(&spec, SESSION_ID, Duration::from_secs(5))
        .await
        .expect("protocol v1 hello must be accepted");
    assert_eq!(client.protocol_version(), 1);
    assert_eq!(client.relay_version(), "v1-fixture");
}

/// The build a worker reports is what decides whether it is replaced, so a
/// controller has to read it from hello - and read a worker that reports
/// none as exactly that, rather than failing the handshake.
#[cfg(unix)]
#[tokio::test]
async fn a_hello_reports_the_worker_build_or_none_from_an_older_worker() {
    let hello = |build: Option<&str>| {
        let data = match build {
            Some(build) => format!(
                r#"{{"negotiated":1,"relay_version":"build-fixture","session_id":"%s","worker_build":"{build}"}}"#
            ),
            None => {
                r#"{"negotiated":1,"relay_version":"build-fixture","session_id":"%s"}"#.to_owned()
            }
        };
        format!(
            r#"
IFS= read -r hello
id=$(printf '%s' "$hello" | sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p')
printf '{{"request_id":"%s","protocol_version":1,"result":"ok","payload":{{"type":"hello","data":{data}}}}}
' "$id" "$1"
while IFS= read -r request; do :; done
"#
        )
    };
    for reported in [None, Some("a".repeat(64).as_str())] {
        let spec = CommandSpec::new(
            "sh",
            [
                "-c".to_owned(),
                hello(reported),
                "hel-relay-build-fixture".to_owned(),
                SESSION_ID.to_owned(),
            ],
        )
        .purpose("relay worker build fixture");
        let client = RelayClient::connect_with_timeout(&spec, SESSION_ID, Duration::from_secs(5))
            .await
            .expect("hello must be accepted with and without a worker build");
        assert_eq!(client.worker_build(), reported);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn dropping_a_client_delivers_eof_before_stopping_its_proxy_launcher() {
    let directory = tempfile::tempdir().unwrap();
    let eof = directory.path().join("proxy-saw-eof");
    let script = r#"
IFS= read -r hello
id=$(printf '%s' "$hello" | sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p')
printf '{"request_id":"%s","protocol_version":1,"result":"ok","payload":{"type":"hello","data":{"negotiated":1,"relay_version":"eof-fixture","session_id":"%s"}}}\n' "$id" "$1"
if IFS= read -r _; then exit 9; fi
: > "$2"
"#;
    let spec = CommandSpec::new(
        "sh",
        [
            "-c".to_owned(),
            script.to_owned(),
            "hel-relay-eof-fixture".to_owned(),
            SESSION_ID.to_owned(),
            eof.to_string_lossy().into_owned(),
        ],
    )
    .purpose("relay proxy EOF fixture");
    let client = RelayClient::connect_with_timeout(&spec, SESSION_ID, Duration::from_secs(5))
        .await
        .unwrap();

    drop(client);
    tokio::time::timeout(Duration::from_secs(2), async {
        while !eof.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("proxy launcher was killed before it observed stdin EOF");
}

#[cfg(unix)]
#[tokio::test]
async fn controller_rejects_negotiated_protocol_outside_supported_range() {
    let future_protocol = RELAY_PROTOCOL_VERSION + 1;
    let script = format!(
        r#"python3 -c '
import json, sys
session = {session:?}
req = json.loads(sys.stdin.readline())
print(json.dumps({{
"request_id": req["request_id"],
"protocol_version": {future_protocol},
"result": "ok",
"payload": {{
    "type": "hello",
    "data": {{
        "negotiated": {future_protocol},
        "relay_version": "future",
        "session_id": session,
    }},
}},
}}), flush=True)
sys.stdin.read()
'"#,
        session = SESSION_ID,
        future_protocol = future_protocol,
    );
    let spec = CommandSpec::new("sh", ["-c", &script]).purpose("future relay fixture");
    let error = RelayClient::connect_with_timeout(&spec, SESSION_ID, Duration::from_secs(5))
        .await
        .err()
        .expect("a future protocol hello must be rejected");
    assert!(
        error.to_string().contains(&format!(
            "negotiated unsupported protocol {future_protocol}"
        )),
        "{error:#}"
    );
    // The transport carried the answer perfectly well; restarting the
    // worker cannot make it speak a protocol it does not implement.
    assert!(!RelayTransportDead::marks(&error), "{error:#}");
}

/// A proxy that exits without answering is the ordinary shape of a dead
/// worker. Recovery hangs on this being typed rather than read.
#[cfg(unix)]
#[tokio::test]
async fn a_proxy_that_exits_before_hello_reports_a_dead_transport() {
    let spec = CommandSpec::new("sh", ["-c", "exit 1"]).purpose("exiting relay proxy");

    let error = RelayClient::connect_with_timeout(&spec, SESSION_ID, Duration::from_secs(5))
        .await
        .err()
        .expect("a proxy that exits cannot complete hello");

    assert!(RelayTransportDead::marks(&error), "{error:#}");
    assert!(RelayTransportDead::marks_failed_handshake(&error));
}

/// The proxy explains failures the controller cannot observe itself, such
/// as a worker socket path longer than `sun_path`. Logging that line is
/// not enough: the error the caller reports must carry it too.
#[cfg(unix)]
#[tokio::test]
async fn a_hello_failure_carries_the_proxy_stderr_tail() {
    const COMPLAINT: &str =
        "connect worker socket /x/control.sock: path must be shorter than SUN_LEN";
    let spec = CommandSpec::new("sh", ["-c", &format!("echo '{COMPLAINT}' >&2; exit 1")])
        .purpose("complaining relay proxy");

    let error = RelayClient::connect_with_timeout(&spec, SESSION_ID, Duration::from_secs(5))
        .await
        .err()
        .expect("a proxy that exits cannot complete hello");

    assert!(format!("{error:#}").contains(COMPLAINT), "{error:#}");
    // Added context must not hide the classification recovery reads.
    assert!(RelayTransportDead::marks(&error), "{error:#}");
    assert!(RelayTransportDead::marks_failed_handshake(&error));
}

#[cfg(unix)]
#[tokio::test]
async fn silent_proxy_handshake_has_a_bounded_deadline() {
    let spec = CommandSpec::new("sh", ["-c", "sleep 30"]).purpose("test silent relay proxy");
    let started = std::time::Instant::now();

    let error = RelayClient::connect_with_timeout(&spec, SESSION_ID, Duration::from_millis(50))
        .await
        .err()
        .expect("silent relay must time out");

    assert!(error.to_string().contains("relay hello timed out"));
    // The launcher is still alive. A loaded target can look exactly like
    // this while starting its proxy, so worker recovery must not restart
    // the native session merely because the deadline elapsed.
    assert!(!RelayTransportDead::marks(&error), "{error:#}");
    assert!(!RelayTransportDead::marks_failed_handshake(&error));
    assert!(started.elapsed() < Duration::from_secs(2));
}

/// A relay that answers `hello` at once and then stalls, replying to the
/// next request long after any controller deadline. `$1` is the session id.
#[cfg(unix)]
const STALLING_RELAY: &str = r#"
IFS= read -r hello
id=$(printf '%s' "$hello" | sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p')
printf '{"request_id":"%s","protocol_version":1,"result":"ok","payload":{"type":"hello","data":{"negotiated":1,"relay_version":"stalling-fixture","session_id":"%s"}}}\n' "$id" "$1"
IFS= read -r stalled
id=$(printf '%s' "$stalled" | sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p')
sleep 5
printf '{"request_id":"%s","protocol_version":1,"result":"error","error":{"code":"internal","message":"late reply","retryable":false}}\n' "$id"
cat > /dev/null
"#;

#[cfg(unix)]
#[tokio::test]
async fn a_timed_out_call_abandons_the_connection_instead_of_desynchronizing_it() {
    let spec = CommandSpec::new(
        "sh",
        ["-c", STALLING_RELAY, "hel-relay-fixture", SESSION_ID],
    )
    .purpose("stalling relay fixture");
    let mut client =
        RelayClient::connect_with_timeout(&spec, SESSION_ID, Duration::from_millis(500))
            .await
            .expect("the fixture answers hello immediately");

    let timed_out = client
        .status()
        .await
        .expect_err("the stalled status call must time out");
    assert!(
        format!("{timed_out:#}").contains("relay status timed out"),
        "{timed_out:#}"
    );
    // A busy worker that misses one deadline is not a dead transport: it
    // answered the handshake, and killing it would be worse than waiting.
    assert!(!RelayTransportDead::marks(&timed_out), "{timed_out:#}");

    // The abandoned reply is still in flight. A later call must not read it
    // as its own response, so it fails at once with the real cause. The
    // normal request deadline is long enough that without this the
    // controller would block on someone else's reply.
    let started = std::time::Instant::now();
    let subsequent = client
        .status()
        .await
        .expect_err("a call on an abandoned connection must fail");
    let elapsed = started.elapsed();
    assert!(
        format!("{subsequent:#}").contains("relay connection abandoned after status timed out"),
        "{subsequent:#}"
    );
    assert!(
        elapsed < Duration::from_millis(250),
        "an abandoned connection must fail fast, took {elapsed:?}"
    );

    let repeated = client
        .status()
        .await
        .expect_err("the connection stays abandoned");
    assert!(
        format!("{repeated:#}").contains("relay connection abandoned after status timed out"),
        "{repeated:#}"
    );
}

#[test]
fn an_unsupported_method_answer_still_reads_as_missing_skills_sync() {
    // Workers that predate skills sync answer the unknown method with an
    // `InvalidRequest` rejection, and so does a current worker's structured
    // unsupported-method response. Both must skip the session quietly.
    let response = mj_core::relay::unsupported_relay_method_response(
        "relay-1".into(),
        RELAY_PROTOCOL_VERSION,
        "skills_state".into(),
    );
    let encoded = serde_json::to_string(&response).unwrap();
    let error = decode_relay_response(&encoded, "relay-1", RELAY_PROTOCOL_VERSION).unwrap_err();
    assert!(sync_method_unsupported(&error), "{error:#}");
}

#[tokio::test]
async fn publishing_new_targets_starts_reconciliation_without_waiting_for_the_tick() {
    let profile = tempfile::tempdir().unwrap();
    let mut coordinator = CredentialSyncCoordinator::spawn();
    coordinator.handle().set_targets(vec![CredentialSyncTarget {
        session_id: SESSION_ID.into(),
        profile_id: "work".into(),
        harness: mj_core::config::HarnessKind::Codex,
        profile_home: profile.path().to_path_buf(),
        authenticates_with_api_key: false,
        sync_github_token: false,
        owns_profile_home: true,
        spec: CommandSpec::new("sh", ["-c", "exit 1"]),
    }]);

    let result = tokio::time::timeout(Duration::from_secs(5), coordinator.result())
        .await
        .expect("target publication must not wait for the 60-second periodic tick")
        .expect("credential coordinator stopped");
    assert_eq!(result.profile_id, "work");
    assert_eq!(result.outcomes.len(), 1);
    assert!(result.outcomes[0].outcome.is_err());
}

#[tokio::test]
async fn response_frame_limit_is_enforced_before_newline() {
    let (mut writer, reader) = tokio::io::duplex(32);
    let write = tokio::spawn(async move {
        writer.write_all(b"123456789\n").await.unwrap();
    });
    let mut reader = BufReader::new(reader);

    let error = read_bounded_frame_with_limit(&mut reader, 8, ExchangeKind::Call)
        .await
        .unwrap_err();

    write.await.unwrap();
    assert!(error.to_string().contains("frame is too large"));
    // An oversized frame is a protocol violation, not a dead transport:
    // the same worker would send the same frame after a restart.
    assert!(!RelayTransportDead::marks(&error), "{error:#}");
}

#[tokio::test]
async fn a_half_written_response_frame_reports_a_dead_transport() {
    let (mut writer, reader) = tokio::io::duplex(32);
    writer.write_all(b"{\"partial\":").await.unwrap();
    drop(writer);
    let mut reader = BufReader::new(reader);

    let error = read_bounded_frame(&mut reader, ExchangeKind::Call)
        .await
        .unwrap_err();

    assert!(RelayTransportDead::marks(&error), "{error:#}");
    assert!(!RelayTransportDead::marks_failed_handshake(&error));
}

#[test]
fn catch_up_page_stops_at_the_frontier_captured_before_stream_growth() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION_ID, "1.0.0").unwrap();
    for message in ["one", "two", "arrived concurrently"] {
        relay
            .record_observation(RelayObservation::Warning {
                message: message.into(),
            })
            .unwrap();
    }
    let all = relay.events_after(0, RELAY_EVENT_GENESIS_DIGEST).unwrap();
    let previous = RelayCursor {
        ordinal: all[0].ordinal,
        digest: all[0].digest.clone(),
    };
    let frontier = RelayCursor {
        ordinal: all[1].ordinal,
        digest: all[1].digest.clone(),
    };
    let page = RelayAttachment {
        state: relay.operational_state(),
        events: all[1..].to_vec(),
        through_ordinal: all[2].ordinal,
        through_digest: all[2].digest.clone(),
    };
    let clipped = clip_catch_up_page(page, &previous, &frontier).unwrap();
    assert_eq!(clipped.through_ordinal, frontier.ordinal);
    assert_eq!(clipped.through_digest, frontier.digest);
    assert_eq!(clipped.events.len(), 1);
    assert_eq!(clipped.events.last().unwrap().ordinal, 2);
}

fn skills_sync_target(
    profile_home: &std::path::Path,
    owns_profile_home: bool,
) -> CredentialSyncTarget {
    CredentialSyncTarget {
        session_id: SESSION_ID.into(),
        profile_id: "work".into(),
        harness: mj_core::config::HarnessKind::Claude,
        profile_home: profile_home.to_path_buf(),
        authenticates_with_api_key: false,
        sync_github_token: false,
        owns_profile_home,
        spec: CommandSpec::new("sh", ["-c", "exit 1"]),
    }
}

#[test]
fn a_session_that_does_not_own_its_profile_home_is_pushed_only_the_user_tree() {
    // The profile home here is the user's own harness home. Installing
    // Mjolnir's managed skills into it would leave them there for good.
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("skills/review")).unwrap();
    std::fs::write(home.path().join("skills/review/SKILL.md"), "review").unwrap();

    let archive = canonical_session_skills(&skills_sync_target(home.path(), false)).unwrap();
    let paths = archive
        .entries()
        .iter()
        .map(|entry| entry.path.as_str())
        .collect::<Vec<_>>();
    assert_eq!(paths, vec!["skills/review/SKILL.md"]);
}

#[test]
fn a_session_that_owns_its_profile_home_is_pushed_the_managed_skills_too() {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("skills/review")).unwrap();
    std::fs::write(home.path().join("skills/review/SKILL.md"), "review").unwrap();

    let target = skills_sync_target(home.path(), true);
    let archive = canonical_session_skills(&target).unwrap();
    assert_eq!(
        archive,
        mj_core::skills::session_skills(target.harness, home.path()).unwrap()
    );
    for managed in mj_core::skills::managed_skills(target.harness) {
        assert!(
            archive.entries().contains(&managed),
            "{} is missing",
            managed.path
        );
    }
    assert!(
        archive
            .entries()
            .iter()
            .any(|entry| entry.path == "skills/review/SKILL.md")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn negotiated_protocol_controls_interrupt_support_on_status_and_attach() {
    for protocol in [16, 17] {
        // Existing workers omit the new controller-supplied protocol field.
        let state = mj_core::relay::RelaySnapshot::new(SESSION_ID.into()).operational_state();
        let state_json = serde_json::to_string(&state).unwrap();
        assert!(!state_json.contains("relay_protocol_version"));
        let script = format!(
            r#"
import json, sys
state = json.loads({state_json:?})
protocol = {protocol}
for line in sys.stdin:
    req = json.loads(line)
    method = req["request"]["method"]
    if method == "hello":
        payload = {{"type": "hello", "data": {{"negotiated": protocol, "relay_version": "interrupt-fixture", "session_id": state["session_id"]}}}}
    elif method == "status":
        payload = {{"type": "status", "data": state}}
    elif method == "attach":
        payload = {{"type": "attached", "data": {{"state": state, "events": [], "through_ordinal": 0, "through_digest": state["latest_digest"]}}}}
    elif method == "submit":
        params = req["request"]["params"]
        assert params["command"]["type"] == "cancel_turn"
        payload = {{"type": "accepted", "data": {{"command_id": params["command_id"], "ordinal": 1}}}}
    else:
        raise AssertionError(method)
    print(json.dumps({{"request_id": req["request_id"], "protocol_version": protocol, "result": "ok", "payload": payload}}), flush=True)
"#
        );
        let spec =
            CommandSpec::new("python3", ["-c", &script]).purpose("interrupt compatibility fixture");
        let mut client =
            RelayClient::connect_with_timeout(&spec, SESSION_ID, Duration::from_secs(5))
                .await
                .unwrap();
        let state = client.status().await.unwrap();
        assert_eq!(state.relay_protocol_version, Some(protocol));
        assert_eq!(state.supports_targeted_turn_control(), protocol >= 17);
        let attachment = client.attach(0, RELAY_EVENT_GENESIS_DIGEST).await.unwrap();
        assert_eq!(attachment.state.relay_protocol_version, Some(protocol));
        assert_eq!(
            attachment.state.supports_targeted_turn_control(),
            protocol >= 17
        );
        if protocol == 16 {
            // A targeted command must still be rejected rather than silently
            // losing its protection against cancelling a subsequent turn.
            let error = client
                .submit(
                    "targeted",
                    RelayCommand::CancelTurnFor {
                        active_prompt_id: "active".into(),
                    },
                )
                .await
                .unwrap_err();
            assert!(format!("{error:#}").contains("IncompatibleProtocol"));
        }
        assert_eq!(
            client
                .submit("interrupt", RelayCommand::CancelTurn)
                .await
                .unwrap(),
            1
        );
    }
}
